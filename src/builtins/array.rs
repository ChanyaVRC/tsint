//! `Array.prototype` メソッド群。

use std::borrow::Cow;

use super::{arg, native, norm_index};
use crate::value::*;
use crate::vm::{array_values, Vm};

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
    let (static_name, arity): (&'static str, usize) = match name {
        "push" => ("push", 1),
        "pop" => ("pop", 0),
        "shift" => ("shift", 0),
        "unshift" => ("unshift", 1),
        "slice" => ("slice", 2),
        "splice" => ("splice", 2),
        "join" => ("join", 1),
        "indexOf" => ("indexOf", 1),
        "includes" => ("includes", 1),
        "concat" => ("concat", 1),
        "map" => ("map", 1),
        "filter" => ("filter", 1),
        "forEach" => ("forEach", 1),
        "find" => ("find", 1),
        "flatMap" => ("flatMap", 1),
        "some" => ("some", 1),
        "every" => ("every", 1),
        _ => return None,
    };
    Some(native(static_name, arity, move |vm, this, args| {
        call_array_method(vm, this, static_name, args).unwrap()
    }))
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
            let mut out = String::new();
            with_array_ref(&this, |items| {
                for (i, it) in items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(&sep);
                    }
                    if !matches!(it, Value::Undefined | Value::Null | Value::Uninit) {
                        crate::vm::to_string_into(it, &mut out);
                    }
                }
            });
            Ok(Value::str(out))
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
        "map" => (|| {
            let items = array_values(&this);
            let cb = arg(args, 0);
            let mut out = Vec::with_capacity(items.len());
            for (i, it) in items.iter().enumerate() {
                out.push(vm.call_value(
                    cb.clone(),
                    Value::Undefined,
                    &[it.clone(), Value::Number(i as f64), this.clone()],
                )?);
            }
            Ok(vm.alloc_array(out))
        })(),
        "filter" => (|| {
            let items = array_values(&this);
            let cb = arg(args, 0);
            let mut out = Vec::new();
            for (i, it) in items.iter().enumerate() {
                let keep = vm.call_value(
                    cb.clone(),
                    Value::Undefined,
                    &[it.clone(), Value::Number(i as f64), this.clone()],
                )?;
                if keep.to_bool() {
                    out.push(it.clone());
                }
            }
            Ok(vm.alloc_array(out))
        })(),
        "forEach" => (|| {
            let items = array_values(&this);
            let cb = arg(args, 0);
            for (i, it) in items.iter().enumerate() {
                vm.call_value(
                    cb.clone(),
                    Value::Undefined,
                    &[it.clone(), Value::Number(i as f64), this.clone()],
                )?;
            }
            Ok(Value::Undefined)
        })(),
        "find" => (|| {
            let items = array_values(&this);
            let cb = arg(args, 0);
            for (i, it) in items.iter().enumerate() {
                let hit = vm.call_value(
                    cb.clone(),
                    Value::Undefined,
                    &[it.clone(), Value::Number(i as f64), this.clone()],
                )?;
                if hit.to_bool() {
                    return Ok(it.clone());
                }
            }
            Ok(Value::Undefined)
        })(),
        "flatMap" => (|| {
            let items = array_values(&this);
            let cb = arg(args, 0);
            let mut out = Vec::new();
            for (i, it) in items.iter().enumerate() {
                let r = vm.call_value(
                    cb.clone(),
                    Value::Undefined,
                    &[it.clone(), Value::Number(i as f64), this.clone()],
                )?;
                if let Value::Object(o) = &r {
                    if let ObjKind::Array(sub) = &o.borrow().kind {
                        out.extend_from_slice(sub);
                        continue;
                    }
                }
                out.push(r);
            }
            Ok(vm.alloc_array(out))
        })(),
        "some" => (|| {
            let items = array_values(&this);
            let cb = arg(args, 0);
            for (i, it) in items.iter().enumerate() {
                let hit = vm.call_value(
                    cb.clone(),
                    Value::Undefined,
                    &[it.clone(), Value::Number(i as f64), this.clone()],
                )?;
                if hit.to_bool() {
                    return Ok(Value::Bool(true));
                }
            }
            Ok(Value::Bool(false))
        })(),
        "every" => (|| {
            let items = array_values(&this);
            let cb = arg(args, 0);
            for (i, it) in items.iter().enumerate() {
                let hit = vm.call_value(
                    cb.clone(),
                    Value::Undefined,
                    &[it.clone(), Value::Number(i as f64), this.clone()],
                )?;
                if !hit.to_bool() {
                    return Ok(Value::Bool(false));
                }
            }
            Ok(Value::Bool(true))
        })(),
        _ => return None,
    })
}
