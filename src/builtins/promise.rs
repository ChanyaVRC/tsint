//! Promise コンストラクタ・静的メソッドおよび `Promise.prototype` メソッド（T3）。

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use super::{arg, arg_ref, lookup_method, native, set_props};
use crate::value::*;
use crate::vm::{is_callable, Vm};

const PROMISE_METHODS: &[(&str, usize)] = &[("then", 2), ("catch", 1), ("finally", 1)];

#[derive(Clone, Copy, PartialEq, Eq)]
enum SettleMode {
    Resolve,
    Reject,
}

pub(super) fn install_promise(vm: &mut Vm) {
    let ctor = Value::native_ctor(
        "Promise",
        1,
        BuiltinCtorTag::Promise,
        Rc::new(|vm, _this, args| {
            let executor = arg(args, 0);
            if !is_callable(&executor) {
                return Err(vm.type_error("Promise の executor は関数である必要があります"));
            }
            let (promise_val, promise_obj) = vm.new_pending_promise();
            // resolve と reject で共有の settled フラグを持ち、`resolve(pendingPromise); reject(e)`
            // のような呼び出しで後から reject が効くのを防ぐ（T-28）。
            let called = Rc::new(Cell::new(false));
            let resolve_fn =
                settle_guarded_fn(vm, promise_obj.clone(), called.clone(), SettleMode::Resolve);
            let reject_fn =
                settle_guarded_fn(vm, promise_obj.clone(), called.clone(), SettleMode::Reject);
            if let Err(e) = vm.call_value(executor, Value::Undefined, &[resolve_fn, reject_fn]) {
                if !called.replace(true) {
                    vm.settle_promise(promise_obj, Err(e));
                }
            }
            Ok(promise_val)
        }),
    );

    let resolve = native("resolve", 1, |vm, _this, args| {
        let v = arg(args, 0);
        if let Value::Object(o) = &v {
            if matches!(o.borrow().kind, ObjKind::Promise(_)) {
                return Ok(v);
            }
        }
        let p = vm.coerce_to_promise(v);
        Ok(Value::Object(p))
    });
    let reject = native("reject", 1, |vm, _this, args| {
        let (v, obj) = vm.new_pending_promise();
        vm.settle_promise(obj, Err(arg(args, 0)));
        Ok(v)
    });
    let race = native("race", 1, |vm, _this, args| {
        let items = vm.materialize_iter_array(arg_ref(args, 0))?;
        let (result_val, result_obj) = vm.new_pending_promise();
        let (_, dummy) = vm.new_pending_promise();
        for item in items {
            let p = vm.coerce_to_promise(item);
            let on_fulfilled = settle_forward_fn(vm, result_obj.clone(), SettleMode::Resolve);
            let on_rejected = settle_forward_fn(vm, result_obj.clone(), SettleMode::Reject);
            vm.add_promise_reaction(
                &p,
                PromiseReaction::Then {
                    on_fulfilled: Some(on_fulfilled),
                    on_rejected: Some(on_rejected),
                    result_promise: dummy.clone(),
                },
            );
        }
        Ok(result_val)
    });
    let all = native("all", 1, |vm, _this, args| {
        let items = vm.materialize_iter_array(arg_ref(args, 0))?;
        let (result_val, result_obj) = vm.new_pending_promise();
        if items.is_empty() {
            let empty = vm.alloc_array(Vec::new());
            vm.resolve_promise(result_obj, empty);
            return Ok(result_val);
        }
        let total = items.len();
        let results: Rc<RefCell<Vec<Value>>> = Rc::new(RefCell::new(vec![Value::Undefined; total]));
        let remaining = Rc::new(Cell::new(total));
        let (_, dummy) = vm.new_pending_promise();
        for (i, item) in items.into_iter().enumerate() {
            let p = vm.coerce_to_promise(item);
            let results_c = results.clone();
            let remaining_c = remaining.clone();
            let result_weak = Rc::downgrade(&result_obj);
            let on_fulfilled = vm.alloc_native_with_captures(
                "",
                1,
                vec![result_obj.clone()],
                Rc::new(move |vm, _this, args| {
                    results_c.borrow_mut()[i] = arg(args, 0);
                    let rem = remaining_c.get().saturating_sub(1);
                    remaining_c.set(rem);
                    if rem == 0 {
                        if let Some(target) = result_weak.upgrade() {
                            let arr = vm.alloc_array(std::mem::take(&mut *results_c.borrow_mut()));
                            vm.resolve_promise(target, arr);
                        }
                    }
                    Ok(Value::Undefined)
                }),
            );
            let on_rejected = settle_forward_fn(vm, result_obj.clone(), SettleMode::Reject);
            vm.add_promise_reaction(
                &p,
                PromiseReaction::Then {
                    on_fulfilled: Some(on_fulfilled),
                    on_rejected: Some(on_rejected),
                    result_promise: dummy.clone(),
                },
            );
        }
        Ok(result_val)
    });
    set_props(
        &ctor,
        [
            ("resolve", resolve),
            ("reject", reject),
            ("race", race),
            ("all", all),
        ],
    );
    vm.define_global("Promise", ctor);
}

fn settle_guarded_fn(vm: &Vm, target: ObjRef, called: Rc<Cell<bool>>, mode: SettleMode) -> Value {
    let weak = Rc::downgrade(&target);
    vm.alloc_native_with_captures(
        "",
        1,
        vec![target],
        Rc::new(move |vm, _this, args| {
            if called.replace(true) {
                return Ok(Value::Undefined);
            }
            if let Some(t) = weak.upgrade() {
                let v = arg(args, 0);
                match mode {
                    SettleMode::Resolve => vm.resolve_promise(t, v),
                    SettleMode::Reject => vm.settle_promise(t, Err(v)),
                }
            }
            Ok(Value::Undefined)
        }),
    )
}

/// 引数の値/例外で `target` を確定させ、戻り値は捨てるネイティブ関数を生成する
/// （`Promise.race` / `Promise.all` で共通利用）。
fn settle_forward_fn(vm: &Vm, target: ObjRef, mode: SettleMode) -> Value {
    let weak = Rc::downgrade(&target);
    vm.alloc_native_with_captures(
        "",
        1,
        vec![target],
        Rc::new(move |vm, _this, args| {
            if let Some(t) = weak.upgrade() {
                let v = arg(args, 0);
                match mode {
                    SettleMode::Resolve => vm.resolve_promise(t, v),
                    SettleMode::Reject => vm.settle_promise(t, Err(v)),
                }
            }
            Ok(Value::Undefined)
        }),
    )
}

pub(super) fn promise_method(name: &str) -> Option<Value> {
    lookup_method(PROMISE_METHODS, name, call_promise_method)
}

fn finally_wrapper(vm: &Vm, cb: &Value, reject: bool) -> Value {
    if let Value::Object(cb_obj) = cb {
        let weak = Rc::downgrade(cb_obj);
        vm.alloc_native_with_captures(
            "",
            1,
            vec![cb_obj.clone()],
            Rc::new(move |vm, _this, args| {
                let v = arg(args, 0);
                if let Some(c) = weak.upgrade() {
                    vm.call_value(Value::Object(c), Value::Undefined, &[])?;
                }
                if reject {
                    Err(v)
                } else {
                    Ok(v)
                }
            }),
        )
    } else {
        let cb_c = cb.clone();
        native("", 1, move |vm, _this, args| {
            let v = arg(args, 0);
            vm.call_value(cb_c.clone(), Value::Undefined, &[])?;
            if reject {
                Err(v)
            } else {
                Ok(v)
            }
        })
    }
}

pub(super) fn call_promise_method(
    vm: &mut Vm,
    this: Value,
    name: &str,
    args: &[Value],
) -> Option<Result<Value, Value>> {
    let as_callable = |i: usize| {
        let v = arg(args, i);
        is_callable(&v).then_some(v)
    };
    let reaction = match name {
        "then" => (as_callable(0), as_callable(1)),
        "catch" => (None, as_callable(0)),
        "finally" => {
            let cb = arg(args, 0);
            if is_callable(&cb) {
                (
                    Some(finally_wrapper(vm, &cb, false)),
                    Some(finally_wrapper(vm, &cb, true)),
                )
            } else {
                // 非関数を渡した場合は素通しする（ES 仕様、T-28）。
                (None, None)
            }
        }
        _ => return None,
    };
    Some((|| {
        let Value::Object(p) = &this else {
            return Err(vm.type_error("Promise ではありません"));
        };
        let (result_val, result_obj) = vm.new_pending_promise();
        vm.add_promise_reaction(
            p,
            PromiseReaction::Then {
                on_fulfilled: reaction.0,
                on_rejected: reaction.1,
                result_promise: result_obj,
            },
        );
        Ok(result_val)
    })())
}
