//! Promise コンストラクタ・静的メソッドおよび `Promise.prototype` メソッド（T3）。

use std::cell::RefCell;
use std::rc::Rc;

use super::{arg, native};
use crate::value::*;
use crate::vm::{is_callable, Vm};

pub(super) fn install_promise(vm: &mut Vm) {
    let ctor = native("Promise", 1, |vm, _this, args| {
        let executor = arg(args, 0);
        let (promise_val, promise_obj) = vm.new_pending_promise();
        let resolve_fn = settle_forward_fn(vm, promise_obj.clone(), false);
        let reject_fn = settle_forward_fn(vm, promise_obj.clone(), true);
        if let Err(e) = vm.call_value(executor, Value::Undefined, &[resolve_fn, reject_fn]) {
            vm.settle_promise(promise_obj, Err(e));
        }
        Ok(promise_val)
    });

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
        let items = vm.materialize_iter_array(&arg(args, 0))?;
        let (result_val, result_obj) = vm.new_pending_promise();
        let (_, dummy) = vm.new_pending_promise();
        for item in items {
            let p = vm.coerce_to_promise(item);
            let on_fulfilled = settle_forward_fn(vm, result_obj.clone(), false);
            let on_rejected = settle_forward_fn(vm, result_obj.clone(), true);
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
        let items = vm.materialize_iter_array(&arg(args, 0))?;
        let (result_val, result_obj) = vm.new_pending_promise();
        if items.is_empty() {
            vm.resolve_promise(result_obj, Value::new_array(Vec::new()));
            return Ok(result_val);
        }
        let total = items.len();
        let results: Rc<RefCell<Vec<Value>>> = Rc::new(RefCell::new(vec![Value::Undefined; total]));
        let remaining: Rc<RefCell<usize>> = Rc::new(RefCell::new(total));
        let (_, dummy) = vm.new_pending_promise();
        for (i, item) in items.into_iter().enumerate() {
            let p = vm.coerce_to_promise(item);
            let results_c = results.clone();
            let remaining_c = remaining.clone();
            let result_obj_c = result_obj.clone();
            let on_fulfilled = native("", 1, move |vm, _this, args| {
                results_c.borrow_mut()[i] = arg(args, 0);
                *remaining_c.borrow_mut() -= 1;
                if *remaining_c.borrow() == 0 {
                    let arr = Value::new_array(results_c.borrow().clone());
                    vm.resolve_promise(result_obj_c.clone(), arr);
                }
                Ok(Value::Undefined)
            });
            let result_obj_r = result_obj.clone();
            let on_rejected = native("", 1, move |vm, _this, args| {
                vm.settle_promise(result_obj_r.clone(), Err(arg(args, 0)));
                Ok(Value::Undefined)
            });
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
    if let Value::Object(o) = &ctor {
        let mut b = o.borrow_mut();
        b.props.set_value(Rc::from("resolve"), resolve);
        b.props.set_value(Rc::from("reject"), reject);
        b.props.set_value(Rc::from("race"), race);
        b.props.set_value(Rc::from("all"), all);
    }
    vm.define_global("Promise", ctor);
}

/// 引数の値/例外で `target` を確定させ、戻り値は捨てるネイティブ関数を生成する
/// （`new Promise` の resolve/reject および `Promise.race` で共通利用）。
fn settle_forward_fn(vm: &Vm, target: ObjRef, is_reject: bool) -> Value {
    let weak = Rc::downgrade(&target);
    vm.alloc_native_with_captures(
        "",
        1,
        vec![target],
        Rc::new(move |vm, _this, args| {
            if let Some(t) = weak.upgrade() {
                let v = arg(args, 0);
                if is_reject {
                    vm.settle_promise(t, Err(v));
                } else {
                    vm.resolve_promise(t, v);
                }
            }
            Ok(Value::Undefined)
        }),
    )
}

pub(super) fn promise_method(name: &str) -> Option<Value> {
    let (static_name, arity): (&'static str, usize) = match name {
        "then" => ("then", 2),
        "catch" => ("catch", 1),
        "finally" => ("finally", 1),
        _ => return None,
    };
    Some(native(static_name, arity, move |vm, this, args| {
        call_promise_method(vm, this, static_name, args).unwrap()
    }))
}

pub(super) fn call_promise_method(
    vm: &mut Vm,
    this: Value,
    name: &str,
    args: &[Value],
) -> Option<Result<Value, Value>> {
    Some(match name {
        "then" => (|| {
            let Value::Object(p) = &this else {
                return Err(vm.type_error("Promise ではありません"));
            };
            let on_fulfilled = match arg(args, 0) {
                v if is_callable(&v) => Some(v),
                _ => None,
            };
            let on_rejected = match arg(args, 1) {
                v if is_callable(&v) => Some(v),
                _ => None,
            };
            let (result_val, result_obj) = vm.new_pending_promise();
            vm.add_promise_reaction(
                p,
                PromiseReaction::Then {
                    on_fulfilled,
                    on_rejected,
                    result_promise: result_obj,
                },
            );
            Ok(result_val)
        })(),
        "catch" => (|| {
            let Value::Object(p) = &this else {
                return Err(vm.type_error("Promise ではありません"));
            };
            let on_rejected = match arg(args, 0) {
                v if is_callable(&v) => Some(v),
                _ => None,
            };
            let (result_val, result_obj) = vm.new_pending_promise();
            vm.add_promise_reaction(
                p,
                PromiseReaction::Then {
                    on_fulfilled: None,
                    on_rejected,
                    result_promise: result_obj,
                },
            );
            Ok(result_val)
        })(),
        "finally" => (|| {
            let Value::Object(p) = &this else {
                return Err(vm.type_error("Promise ではありません"));
            };
            let cb = arg(args, 0);
            let (result_val, result_obj) = vm.new_pending_promise();
            let (on_fulfilled, on_rejected) = if let Value::Object(cb_obj) = cb {
                let cb_w1 = Rc::downgrade(&cb_obj);
                let on_f = vm.alloc_native_with_captures(
                    "",
                    1,
                    vec![cb_obj.clone()],
                    Rc::new(move |vm, _this, args| {
                        let v = arg(args, 0);
                        if let Some(c) = cb_w1.upgrade() {
                            vm.call_value(Value::Object(c), Value::Undefined, &[])?;
                        }
                        Ok(v)
                    }),
                );
                let cb_w2 = Rc::downgrade(&cb_obj);
                let on_r = vm.alloc_native_with_captures(
                    "",
                    1,
                    vec![cb_obj],
                    Rc::new(move |vm, _this, args| {
                        let e = arg(args, 0);
                        if let Some(c) = cb_w2.upgrade() {
                            vm.call_value(Value::Object(c), Value::Undefined, &[])?;
                        }
                        Err(e)
                    }),
                );
                (on_f, on_r)
            } else {
                let cb_f = cb.clone();
                let on_f = native("", 1, move |vm, _this, args| {
                    let v = arg(args, 0);
                    vm.call_value(cb_f.clone(), Value::Undefined, &[])?;
                    Ok(v)
                });
                let on_r = native("", 1, move |vm, _this, args| {
                    let e = arg(args, 0);
                    vm.call_value(cb.clone(), Value::Undefined, &[])?;
                    Err(e)
                });
                (on_f, on_r)
            };
            vm.add_promise_reaction(
                p,
                PromiseReaction::Then {
                    on_fulfilled: Some(on_fulfilled),
                    on_rejected: Some(on_rejected),
                    result_promise: result_obj,
                },
            );
            Ok(result_val)
        })(),
        _ => return None,
    })
}
