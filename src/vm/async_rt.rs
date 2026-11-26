//! T3: Promise・マイクロタスクキュー・`await` タスク中断/再開・イベントループ。

use std::cell::Cell;
use std::rc::Rc;

use crate::value::*;

use super::ops::is_callable;
use super::{HostPromise, MicrotaskJob, PumpState, RunOutcome, Vm, MAX_NATIVE_CALL_DEPTH};

#[derive(Clone, Copy, PartialEq, Eq)]
enum SettleMode {
    Resolve,
    Reject,
}

impl Vm {
    pub(super) fn spawn_async_task_from_function(
        &mut self,
        f: &Rc<FunctionObj>,
        this: Value,
        args: &[Value],
    ) -> Result<Value, Value> {
        if self.rust_call_depth >= MAX_NATIVE_CALL_DEPTH {
            return Err(self.range_error("Maximum call stack size exceeded"));
        }
        let frame = self.acquire_frame_from_slice(f, this, args);
        let caller_frames = std::mem::replace(&mut self.frames, vec![frame]);
        self.suspended_callers.push(caller_frames);
        let (promise_val, promise_obj) = self.new_pending_promise();
        self.rust_call_depth += 1;
        self.drive_current_task(promise_obj);
        self.rust_call_depth -= 1;
        self.frames = self.suspended_callers.pop().unwrap_or_default();
        Ok(promise_val)
    }

    pub(super) fn drive_current_task(&mut self, owner_promise: ObjRef) {
        let saved_owner = self.current_owner_promise.replace(owner_promise.clone());
        let saved_depth = self.native_call_depth;
        self.native_call_depth = 0;
        let outcome = self.run_until(0);
        self.native_call_depth = saved_depth;
        self.current_owner_promise = saved_owner;
        match outcome {
            Ok(RunOutcome::Value(v)) => self.resolve_promise(owner_promise, v),
            Ok(RunOutcome::Suspended) => {}
            Err(e) => self.settle_promise(owner_promise, Err(e)),
        }
    }

    fn resume_continuation(&mut self, cont: AwaitContinuation, outcome: Result<Value, Value>) {
        let AwaitContinuation {
            frames,
            owner_promise,
        } = cont;
        // `run_module` でキャンセル済み（既に settled）のタスクは副作用を実行せず破棄する（T-16）。
        let owner_pending = matches!(
            owner_promise.borrow().kind,
            ObjKind::Promise(ref pd) if matches!(pd.state, PromiseState::Pending)
        );
        if !owner_pending {
            for f in frames {
                self.release_frame(f);
            }
            return;
        }
        let caller_frames = std::mem::replace(&mut self.frames, frames);
        self.suspended_callers.push(caller_frames);
        self.rust_call_depth += 1;
        let mut already_done = false;
        match outcome {
            Ok(v) => self.push(v),
            Err(e) => {
                if let Err(final_err) = self.unwind(e, 0) {
                    self.settle_promise(owner_promise.clone(), Err(final_err));
                    already_done = true;
                }
            }
        }
        if !already_done {
            self.drive_current_task(owner_promise);
        }
        self.rust_call_depth -= 1;
        self.frames = self.suspended_callers.pop().unwrap_or_default();
    }

    pub fn new_pending_promise(&mut self) -> (Value, ObjRef) {
        let obj: ObjRef = self.alloc_obj_ref(ObjData::new(ObjKind::Promise(Box::new(
            PromiseData::pending(),
        ))));
        (Value::Object(obj.clone()), obj)
    }

    pub fn new_host_promise(&mut self) -> (Value, HostPromise) {
        let (v, obj) = self.new_pending_promise();
        self.pending_host_count
            .set(self.pending_host_count.get() + 1);
        (
            v,
            HostPromise {
                promise: obj,
                settled: Cell::new(false),
                pending_host_count: self.pending_host_count.clone(),
            },
        )
    }

    pub fn coerce_to_promise(&mut self, v: Value) -> ObjRef {
        if let Value::Object(o) = &v {
            if matches!(o.borrow().kind, ObjKind::Promise(_)) {
                return o.clone();
            }
        }
        let (_, p) = self.new_pending_promise();
        self.resolve_promise(p.clone(), v);
        p
    }

    pub fn resolve_promise(&mut self, promise: ObjRef, value: Value) {
        if let Value::Object(o) = &value {
            if Rc::ptr_eq(o, &promise) {
                let err = self.type_error("Promise を自分自身の解決値にすることはできません");
                self.settle_promise(promise, Err(err));
                return;
            }
            let is_native_promise = matches!(o.borrow().kind, ObjKind::Promise(_));
            if is_native_promise {
                self.add_reaction(
                    o,
                    PromiseReaction::Then {
                        on_fulfilled: None,
                        on_rejected: None,
                        result_promise: promise,
                    },
                );
                return;
            }
            // プロトタイプ上のメソッドや getter も含めて `then` を参照する（T-28）。
            let then_prop = match self.get_prop(&value, "then") {
                Ok(v) => v,
                Err(e) => {
                    self.settle_promise(promise, Err(e));
                    return;
                }
            };
            if is_callable(&then_prop) {
                self.microtasks.push_back(MicrotaskJob::ResolveThenable {
                    promise,
                    thenable: value,
                    then_fn: then_prop,
                });
                return;
            }
        }
        self.settle_promise(promise, Ok(value));
    }

    pub fn settle_promise(&mut self, promise: ObjRef, outcome: Result<Value, Value>) {
        let reactions = {
            let mut b = promise.borrow_mut();
            let ObjKind::Promise(pd) = &mut b.kind else {
                return;
            };
            if !matches!(pd.state, PromiseState::Pending) {
                return;
            }
            pd.state = match &outcome {
                Ok(v) => PromiseState::Fulfilled(v.clone()),
                Err(e) => PromiseState::Rejected(e.clone()),
            };
            std::mem::take(&mut pd.reactions)
        };
        if reactions.is_empty() && outcome.is_err() {
            self.maybe_unhandled.push(promise.clone());
        }
        for r in reactions {
            self.schedule_reaction(r, outcome.clone());
        }
    }

    pub fn add_promise_reaction(&mut self, promise: &ObjRef, reaction: PromiseReaction) {
        self.add_reaction(promise, reaction);
    }

    pub(super) fn add_reaction(&mut self, promise: &ObjRef, reaction: PromiseReaction) {
        enum St {
            Pending,
            Settled(Result<Value, Value>),
        }
        let st = {
            let mut b = promise.borrow_mut();
            let ObjKind::Promise(pd) = &mut b.kind else {
                return;
            };
            pd.handled = true;
            match &pd.state {
                PromiseState::Pending => St::Pending,
                PromiseState::Fulfilled(v) => St::Settled(Ok(v.clone())),
                PromiseState::Rejected(e) => St::Settled(Err(e.clone())),
            }
        };
        match st {
            St::Pending => {
                let mut b = promise.borrow_mut();
                if let ObjKind::Promise(pd) = &mut b.kind {
                    pd.reactions.push(reaction);
                }
            }
            St::Settled(outcome) => self.schedule_reaction(reaction, outcome),
        }
    }

    fn schedule_reaction(&mut self, reaction: PromiseReaction, outcome: Result<Value, Value>) {
        match reaction {
            PromiseReaction::Then {
                on_fulfilled,
                on_rejected,
                result_promise,
            } => self.microtasks.push_back(MicrotaskJob::Then {
                on_fulfilled,
                on_rejected,
                result_promise,
                outcome,
            }),
            PromiseReaction::Await(cont) => self
                .microtasks
                .push_back(MicrotaskJob::ResumeAwait { cont, outcome }),
        }
    }

    fn run_microtask_job(&mut self, job: MicrotaskJob) {
        match job {
            MicrotaskJob::Then {
                on_fulfilled,
                on_rejected,
                result_promise,
                outcome,
            } => match outcome {
                Ok(v) => match on_fulfilled {
                    Some(f) => match self.call_value(f, Value::Undefined, &[v]) {
                        Ok(r) => self.resolve_promise(result_promise, r),
                        Err(e) => self.settle_promise(result_promise, Err(e)),
                    },
                    None => self.resolve_promise(result_promise, v),
                },
                Err(e) => match on_rejected {
                    Some(f) => match self.call_value(f, Value::Undefined, &[e]) {
                        Ok(r) => self.resolve_promise(result_promise, r),
                        Err(e2) => self.settle_promise(result_promise, Err(e2)),
                    },
                    None => self.settle_promise(result_promise, Err(e)),
                },
            },
            MicrotaskJob::ResolveThenable {
                promise,
                thenable,
                then_fn,
            } => self.run_resolve_thenable(promise, thenable, then_fn),
            MicrotaskJob::ResumeAwait { cont, outcome } => self.resume_continuation(cont, outcome),
        }
    }

    fn make_thenable_guard_fn(
        &self,
        promise: &ObjRef,
        called: &Rc<Cell<bool>>,
        mode: SettleMode,
    ) -> Value {
        let weak = Rc::downgrade(promise);
        let called_c = called.clone();
        self.alloc_native_with_captures(
            "",
            1,
            vec![promise.clone()],
            Rc::new(move |vm, _this, args| {
                if called_c.replace(true) {
                    return Ok(Value::Undefined);
                }
                let v = args.first().cloned().unwrap_or(Value::Undefined);
                if let Some(p) = weak.upgrade() {
                    match mode {
                        SettleMode::Resolve => vm.resolve_promise(p, v),
                        SettleMode::Reject => vm.settle_promise(p, Err(v)),
                    }
                }
                Ok(Value::Undefined)
            }),
        )
    }

    fn run_resolve_thenable(&mut self, promise: ObjRef, thenable: Value, then_fn: Value) {
        let called = Rc::new(Cell::new(false));
        let resolve_fn = self.make_thenable_guard_fn(&promise, &called, SettleMode::Resolve);
        let reject_fn = self.make_thenable_guard_fn(&promise, &called, SettleMode::Reject);
        if let Err(e) = self.call_value(then_fn, thenable, &[resolve_fn, reject_fn]) {
            if !called.replace(true) {
                self.settle_promise(promise, Err(e));
            }
        }
    }

    pub fn run_event_loop(&mut self) -> PumpState {
        while let Some(job) = self.microtasks.pop_front() {
            self.run_microtask_job(job);
        }
        if !self.maybe_unhandled.is_empty() {
            let list = std::mem::take(&mut self.maybe_unhandled);
            for p in list {
                let rejected_val = {
                    let b = p.borrow();
                    match &b.kind {
                        ObjKind::Promise(pd) if !pd.handled => match &pd.state {
                            PromiseState::Rejected(v) => Some(v.clone()),
                            _ => None,
                        },
                        _ => None,
                    }
                };
                if let Some(val) = rejected_val {
                    if self.has_custom_unhandled_hook {
                        (self.on_unhandled_rejection)(&val);
                    } else {
                        let msg = format!(
                            "UnhandledPromiseRejection: {}",
                            self.to_display_string(&val)
                        );
                        (self.on_log)(&msg);
                    }
                }
            }
        }
        self.maybe_gc_if_top_level();
        if self.pending_host_count.get() > 0 {
            PumpState::Waiting
        } else {
            PumpState::Idle
        }
    }
}
