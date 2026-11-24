//! `Array.prototype` メソッド群。

use std::borrow::Cow;

use super::{arg, lookup_method, norm_index};
use crate::value::*;
use crate::vm::{array_values, Vm};

const ARRAY_METHODS: &[(&str, usize)] = &[
    ("push", 1),
    ("pop", 0),
    ("shift", 0),
    ("unshift", 1),
    ("slice", 2),
    ("splice", 2),
    ("join", 1),
    ("indexOf", 1),
    ("includes", 1),
    ("concat", 1),
    ("map", 1),
    ("filter", 1),
    ("forEach", 1),
    ("find", 1),
    ("flatMap", 1),
    ("some", 1),
    ("every", 1),
];

#[inline]
fn with_array_ref<T>(this: &Value, f: impl FnOnce(&[Value]) -> T) -> Option<T> {
    if let Value::Object(o) = this {
        if let ObjKind::Array(items) = &o.borrow().kind {
            return Some(f(items));
        }
    }
    None
}

#[inline]
fn with_array_mut<T>(this: &Value, f: impl FnOnce(&mut Vec<Value>) -> T) -> Option<T> {
    if let Value::Object(o) = this {
        if let ObjKind::Array(items) = &mut o.borrow_mut().kind {
            return Some(f(items));
        }
    }
    None
}

pub(super) fn array_method(name: &str) -> Option<Value> {
    lookup_method(ARRAY_METHODS, name, call_array_method)
}

pub(super) fn call_array_method(
    vm: &mut Vm,
    this: Value,
    name: &str,
    args: &[Value],
) -> Option<Result<Value, Value>> {
    Some(match name {
        "push" => {
            let len = with_array_mut(&this, |items| {
                items.extend_from_slice(args);
                items.len()
            })
            .unwrap_or(0);
            Ok(Value::Number(len as f64))
        }
        "pop" => Ok(with_array_mut(&this, |items| items.pop())
            .flatten()
            .unwrap_or(Value::Undefined)),
        "shift" => Ok(with_array_mut(&this, |items| {
            if items.is_empty() {
                None
            } else {
                Some(items.remove(0))
            }
        })
        .flatten()
        .unwrap_or(Value::Undefined)),
        "unshift" => {
            let len = with_array_mut(&this, |items| {
                for (i, a) in args.iter().enumerate() {
                    items.insert(i, a.clone());
                }
                items.len()
            })
            .unwrap_or(0);
            Ok(Value::Number(len as f64))
        }
        "slice" => {
            let out = with_array_ref(&this, |items| {
                let len = items.len() as i64;
                let start = norm_index(args.first(), 0, len);
                let end = norm_index(args.get(1), len, len);
                if start < end {
                    items[start as usize..end as usize].to_vec()
                } else {
                    Vec::new()
                }
            })
            .unwrap_or_default();
            Ok(vm.alloc_array(out))
        }
        "splice" => {
            let removed = with_array_mut(&this, |items| {
                let len = items.len() as i64;
                let start = norm_index(args.first(), 0, len).clamp(0, len) as usize;
                let del_count = match args.get(1) {
                    Some(Value::Number(n)) => (*n as i64).clamp(0, len - start as i64) as usize,
                    _ => items.len() - start,
                };
                let mut rest = items.split_off(start.min(items.len()));
                let removed: Vec<Value> = rest.drain(0..del_count.min(rest.len())).collect();
                for (i, v) in args.iter().skip(2).enumerate() {
                    items.insert(start + i, v.clone());
                }
                items.append(&mut rest);
                removed
            })
            .unwrap_or_default();
            Ok(vm.alloc_array(removed))
        }
        "join" => {
            let sep = match args.first() {
                Some(Value::Undefined) | None => Cow::Borrowed(","),
                Some(Value::Str(s)) => Cow::Borrowed(&**s),
                Some(v) => Cow::Owned(vm.to_display_string(v)),
            };
            let rc = crate::vm::with_str_buf_rc(|out| {
                with_array_ref(&this, |items| {
                    for (i, it) in items.iter().enumerate() {
                        if i > 0 {
                            out.push_str(&sep);
                        }
                        if !matches!(it, Value::Undefined | Value::Null | Value::Uninit) {
                            crate::vm::to_string_into(it, out);
                        }
                    }
                });
            });
            Ok(Value::Str(rc))
        }
        "indexOf" => {
            let target = args.first().unwrap_or(&Value::Undefined);
            let idx = with_array_ref(&this, |items| {
                items
                    .iter()
                    .position(|v| crate::value::strict_eq(v, target))
            })
            .flatten();
            Ok(Value::Number(idx.map(|i| i as f64).unwrap_or(-1.0)))
        }
        "includes" => {
            let target = args.first().unwrap_or(&Value::Undefined);
            let found = with_array_ref(&this, |items| {
                items.iter().any(|v| same_value_zero(v, target))
            })
            .unwrap_or(false);
            Ok(Value::Bool(found))
        }
        "concat" => {
            let mut out = array_values(&this);
            for a in args {
                if let Value::Object(o) = a {
                    if let ObjKind::Array(items) = &o.borrow().kind {
                        out.extend_from_slice(items);
                        continue;
                    }
                }
                out.push(a.clone());
            }
            Ok(vm.alloc_array(out))
        }
        "map" => {
            let len = with_array_ref(&this, <[Value]>::len).unwrap_or(0);
            let mut out = Vec::with_capacity(len);
            array_iter_cb(vm, &this, arg(args, 0), |_it, res| {
                out.push(res);
                std::ops::ControlFlow::<()>::Continue(())
            })
            .map(|_| vm.alloc_array(out))
        }
        "filter" => {
            let mut out = Vec::new();
            array_iter_cb(vm, &this, arg(args, 0), |it, keep| {
                if keep.to_bool() {
                    out.push(it);
                }
                std::ops::ControlFlow::<()>::Continue(())
            })
            .map(|_| vm.alloc_array(out))
        }
        "forEach" => array_iter_cb(vm, &this, arg(args, 0), |_it, _res| {
            std::ops::ControlFlow::<()>::Continue(())
        })
        .map(|_| Value::Undefined),
        "find" => array_iter_cb(vm, &this, arg(args, 0), |it, hit| {
            if hit.to_bool() {
                std::ops::ControlFlow::Break(it)
            } else {
                std::ops::ControlFlow::Continue(())
            }
        })
        .map(|found| found.unwrap_or(Value::Undefined)),
        "flatMap" => {
            let mut out = Vec::new();
            array_iter_cb(vm, &this, arg(args, 0), |_it, r| {
                if let Value::Object(o) = &r {
                    if let ObjKind::Array(sub) = &o.borrow().kind {
                        out.extend_from_slice(sub);
                        return std::ops::ControlFlow::<()>::Continue(());
                    }
                }
                out.push(r);
                std::ops::ControlFlow::<()>::Continue(())
            })
            .map(|_| vm.alloc_array(out))
        }
        "some" => array_iter_cb(vm, &this, arg(args, 0), |_it, hit| {
            if hit.to_bool() {
                std::ops::ControlFlow::Break(true)
            } else {
                std::ops::ControlFlow::Continue(())
            }
        })
        .map(|hit| Value::Bool(hit.unwrap_or(false))),
        "every" => array_iter_cb(vm, &this, arg(args, 0), |_it, hit| {
            if !hit.to_bool() {
                std::ops::ControlFlow::Break(false)
            } else {
                std::ops::ControlFlow::Continue(())
            }
        })
        .map(|ok| Value::Bool(ok.unwrap_or(true))),
        _ => return None,
    })
}

/// 配列を先頭から `len` 件走査し、配列全体を複製せずにコールバックを呼び出す共通ヘルパー。
fn array_iter_cb<B>(
    vm: &mut Vm,
    this: &Value,
    cb: Value,
    mut on_elem: impl FnMut(Value, Value) -> std::ops::ControlFlow<B>,
) -> Result<Option<B>, Value> {
    let len = with_array_ref(this, <[Value]>::len).unwrap_or(0);
    for i in 0..len {
        let Some(it) = with_array_ref(this, |items| items.get(i).cloned()).flatten() else {
            break;
        };
        let res = vm.call_value(
            cb.clone(),
            Value::Undefined,
            &[it.clone(), Value::Number(i as f64), this.clone()],
        )?;
        if let std::ops::ControlFlow::Break(b) = on_elem(it, res) {
            return Ok(Some(b));
        }
    }
    Ok(None)
}
