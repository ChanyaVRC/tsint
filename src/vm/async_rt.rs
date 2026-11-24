//! T3: Promise・マイクロタスクキュー・`await` タスク中断/再開・イベントループ。

use std::cell::RefCell;
use std::rc::Rc;

use crate::value::*;

use super::ops::is_callable;
use super::{HostPromise, MicrotaskJob, PumpState, RunOutcome, Vm};

impl Vm {
    pub(super) fn spawn_async_task_from_function(
        &mut self,
        f: &Rc<FunctionObj>,
        this: Value,
        args: &[Value],
    ) -> Value {
        let frame = self.acquire_frame_from_slice(f, this, args);
        let caller_frames = std::mem::replace(&mut self.frames, vec![frame]);
        let (promise_val, promise_obj) = self.new_pending_promise();
        self.drive_current_task(promise_obj);
        self.frames = caller_frames;
        promise_val
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
        let caller_frames = std::mem::replace(&mut self.frames, frames);
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
        self.frames = caller_frames;
    }

    pub fn new_pending_promise(&mut self) -> (Value, ObjRef) {
        let obj: ObjRef =
            self.alloc_obj_ref(ObjData::new(ObjKind::Promise(PromiseData::pending())));
        (Value::Object(obj.clone()), obj)
    }

    pub fn new_host_promise(&mut self) -> (Value, HostPromise) {
        let (v, obj) = self.new_pending_promise();
        self.pending_host_count += 1;
        (v, HostPromise { promise: obj })
    }

    pub(super) fn settle_host_promise(&mut self, promise: ObjRef, outcome: Result<Value, Value>) {
        let was_pending = matches!(
            promise.borrow().kind,
            ObjKind::Promise(ref pd) if matches!(pd.state, PromiseState::Pending)
        );
        if was_pending {
            self.pending_host_count = self.pending_host_count.saturating_sub(1);
        }
        match outcome {
            Ok(v) => self.resolve_promise(promise, v),
            Err(e) => self.settle_promise(promise, Err(e)),
        }
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
            let then_fn = {
                let b = o.borrow();
                match b.props.get("then") {
                    Some(PropSlot::Value(v)) if is_callable(v) => Some(v.clone()),
                    _ => None,
                }
            };
            if let Some(then_fn) = then_fn {
                self.microtasks.push_back(MicrotaskJob::ResolveThenable {
                    promise,
                    thenable: value,
                    then_fn,
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
        called: &Rc<RefCell<bool>>,
        is_reject: bool,
    ) -> Value {
        let weak = Rc::downgrade(promise);
        let called_c = called.clone();
        self.alloc_native_with_captures(
            "",
            1,
            vec![promise.clone()],
            Rc::new(move |vm, _this, args| {
                if *called_c.borrow() {
                    return Ok(Value::Undefined);
                }
                *called_c.borrow_mut() = true;
                let v = args.first().cloned().unwrap_or(Value::Undefined);
                if let Some(p) = weak.upgrade() {
                    if is_reject {
                        vm.settle_promise(p, Err(v));
                    } else {
                        vm.resolve_promise(p, v);
                    }
                }
                Ok(Value::Undefined)
            }),
        )
    }

    fn run_resolve_thenable(&mut self, promise: ObjRef, thenable: Value, then_fn: Value) {
        let called: Rc<RefCell<bool>> = Rc::new(RefCell::new(false));
        let resolve_fn = self.make_thenable_guard_fn(&promise, &called, false);
        let reject_fn = self.make_thenable_guard_fn(&promise, &called, true);
        if let Err(e) = self.call_value(then_fn, thenable, &[resolve_fn, reject_fn]) {
            if !*called.borrow() {
                *called.borrow_mut() = true;
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
                    (self.on_unhandled_rejection)(&val);
                }
            }
        }
        self.maybe_gc_if_top_level();
        if self.pending_host_count > 0 {
            PumpState::Waiting
        } else {
            PumpState::Idle
        }
    }
}
