//! `Map` / `Set` コンストラクタおよびプロトタイプメソッド群。

use std::rc::Rc;

use super::{arg, arg_ref, native};
use crate::value::*;
use crate::vm::{array_values, Vm};

pub(super) fn install_map_set_ctors(vm: &mut Vm) {
    vm.register_native(
        "Map",
        0,
        Rc::new(|vm, _this, args| {
            let mut data = MapData::default();
            if let Some(it) = args.first() {
                if !it.is_nullish() {
                    for pair in vm.materialize_iter_array(it)? {
                        let pv = array_values(&pair);
                        let k = pv.first().cloned().unwrap_or(Value::Undefined);
                        let val = pv.get(1).cloned().unwrap_or(Value::Undefined);
                        data.set(k, val);
                    }
                }
            }
            Ok(vm.alloc_obj(ObjData::new(ObjKind::Map(data))))
        }),
    );
    vm.register_native(
        "Set",
        0,
        Rc::new(|vm, _this, args| {
            let mut data = SetData::default();
            if let Some(it) = args.first() {
                if !it.is_nullish() {
                    for v in vm.materialize_iter_array(it)? {
                        data.add(v);
                    }
                }
            }
            Ok(vm.alloc_obj(ObjData::new(ObjKind::Set(data))))
        }),
    );
}

#[inline]
fn with_map_ref<T>(this: &Value, f: impl FnOnce(&MapData) -> T) -> Option<T> {
    if let Value::Object(o) = this {
        if let ObjKind::Map(m) = &o.borrow().kind {
            return Some(f(m));
        }
    }
    None
}

#[inline]
fn with_map_mut<T>(this: &Value, f: impl FnOnce(&mut MapData) -> T) -> Option<T> {
    if let Value::Object(o) = this {
        if let ObjKind::Map(m) = &mut o.borrow_mut().kind {
            return Some(f(m));
        }
    }
    None
}

#[inline]
fn with_set_ref<T>(this: &Value, f: impl FnOnce(&SetData) -> T) -> Option<T> {
    if let Value::Object(o) = this {
        if let ObjKind::Set(s) = &o.borrow().kind {
            return Some(f(s));
        }
    }
    None
}

#[inline]
fn with_set_mut<T>(this: &Value, f: impl FnOnce(&mut SetData) -> T) -> Option<T> {
    if let Value::Object(o) = this {
        if let ObjKind::Set(s) = &mut o.borrow_mut().kind {
            return Some(f(s));
        }
    }
    None
}

pub(super) fn map_method(name: &str) -> Option<Value> {
    let (static_name, arity): (&'static str, usize) = match name {
        "get" => ("get", 1),
        "set" => ("set", 2),
        "has" => ("has", 1),
        "delete" => ("delete", 1),
        "keys" => ("keys", 0),
        "values" => ("values", 0),
        "entries" => ("entries", 0),
        "forEach" => ("forEach", 1),
        "clear" => ("clear", 0),
        _ => return None,
    };
    Some(native(static_name, arity, move |vm, this, args| {
        call_map_method(vm, this, static_name, args).unwrap()
    }))
}

pub(super) fn call_map_method(
    vm: &mut Vm,
    this: Value,
    name: &str,
    args: &[Value],
) -> Option<Result<Value, Value>> {
    Some(match name {
        "get" => {
            let key = arg_ref(args, 0);
            let val = with_map_ref(&this, |m| m.find(key).map(|i| m.entries[i].1.clone()))
                .flatten()
                .unwrap_or(Value::Undefined);
            Ok(val)
        }
        "set" => {
            let key = arg(args, 0);
            let val = arg(args, 1);
            with_map_mut(&this, |m| m.set(key, val));
            Ok(this)
        }
        "has" => {
            let key = arg_ref(args, 0);
            let found = with_map_ref(&this, |m| m.find(key).is_some()).unwrap_or(false);
            Ok(Value::Bool(found))
        }
        "delete" => {
            let key = arg_ref(args, 0);
            let deleted = with_map_mut(&this, |m| m.delete(key)).unwrap_or(false);
            Ok(Value::Bool(deleted))
        }
        "keys" => {
            let ks = with_map_ref(&this, |m| {
                m.entries.iter().map(|(k, _)| k.clone()).collect()
            })
            .unwrap_or_default();
            Ok(vm.alloc_array(ks))
        }
        "values" => {
            let vs = with_map_ref(&this, |m| {
                m.entries.iter().map(|(_, v)| v.clone()).collect()
            })
            .unwrap_or_default();
            Ok(vm.alloc_array(vs))
        }
        "entries" => {
            let pairs = with_map_ref(&this, |m| {
                m.entries
                    .iter()
                    .map(|(k, v)| vm.alloc_array(vec![k.clone(), v.clone()]))
                    .collect()
            })
            .unwrap_or_default();
            Ok(vm.alloc_array(pairs))
        }
        "forEach" => (|| {
            let cb = arg(args, 0);
            let entries = with_map_ref(&this, |m| m.entries.clone()).unwrap_or_default();
            for (k, v) in entries {
                vm.call_value(cb.clone(), Value::Undefined, &[v, k, this.clone()])?;
            }
            Ok(Value::Undefined)
        })(),
        "clear" => {
            with_map_mut(&this, MapData::clear);
            Ok(Value::Undefined)
        }
        _ => return None,
    })
}

pub(super) fn set_method(name: &str) -> Option<Value> {
    let (static_name, arity): (&'static str, usize) = match name {
        "add" => ("add", 1),
        "has" => ("has", 1),
        "delete" => ("delete", 1),
        "keys" | "values" => ("values", 0),
        "forEach" => ("forEach", 1),
        "clear" => ("clear", 0),
        _ => return None,
    };
    Some(native(static_name, arity, move |vm, this, args| {
        call_set_method(vm, this, static_name, args).unwrap()
    }))
}

pub(super) fn call_set_method(
    vm: &mut Vm,
    this: Value,
    name: &str,
    args: &[Value],
) -> Option<Result<Value, Value>> {
    Some(match name {
        "add" => {
            let v = arg(args, 0);
            with_set_mut(&this, |s| s.add(v));
            Ok(this)
        }
        "has" => {
            let v = arg_ref(args, 0);
            let found = with_set_ref(&this, |s| s.find(v).is_some()).unwrap_or(false);
            Ok(Value::Bool(found))
        }
        "delete" => {
            let v = arg_ref(args, 0);
            let deleted = with_set_mut(&this, |s| s.delete(v)).unwrap_or(false);
            Ok(Value::Bool(deleted))
        }
        "keys" | "values" => {
            let vs = with_set_ref(&this, |s| s.values.clone()).unwrap_or_default();
            Ok(vm.alloc_array(vs))
        }
        "forEach" => (|| {
            let cb = arg(args, 0);
            let values = with_set_ref(&this, |s| s.values.clone()).unwrap_or_default();
            for v in values {
                vm.call_value(cb.clone(), Value::Undefined, &[v.clone(), v, this.clone()])?;
            }
            Ok(Value::Undefined)
        })(),
        "clear" => {
            with_set_mut(&this, SetData::clear);
            Ok(Value::Undefined)
        }
        _ => return None,
    })
}
