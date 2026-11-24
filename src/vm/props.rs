//! プロパティ/添字アクセス、プロトタイプ探索、`instanceof`、反復実体化。

use std::rc::Rc;

use crate::value::*;

use super::ops::{to_number, to_property_key_string};
use super::{KindTag, Vm};

/// オブジェクトまたは配列の列挙可能な own キー一覧を返す（`for...in` / `Object.keys` 共通）。
pub(crate) fn own_enumerable_keys(v: &Value) -> Vec<Value> {
    match v {
        Value::Object(o) => {
            let b = o.borrow();
            match &b.kind {
                ObjKind::Array(items) => (0..items.len())
                    .map(|i| Value::str(i.to_string()))
                    .collect(),
                _ => b
                    .props
                    .iter()
                    .filter(|(_, s)| matches!(s, PropSlot::Value(_)))
                    .map(|(k, _)| Value::Str(k.clone()))
                    .collect(),
            }
        }
        _ => Vec::new(),
    }
}

#[inline]
pub(super) fn kind_tag_of(kind: &ObjKind) -> Option<KindTag> {
    match kind {
        ObjKind::Array(_) => Some(KindTag::Array),
        ObjKind::Map(_) => Some(KindTag::Map),
        ObjKind::Set(_) => Some(KindTag::Set),
        ObjKind::Regex(_) => Some(KindTag::Regex),
        ObjKind::Promise(_) => Some(KindTag::Promise),
        _ => None,
    }
}

#[inline]
fn kind_prop_lookup(kind: &ObjKind, name: &str) -> Option<Value> {
    match kind {
        ObjKind::Array(items) => {
            if name == "length" {
                Some(Value::Number(items.len() as f64))
            } else if let Ok(idx) = name.parse::<usize>() {
                Some(items.get(idx).cloned().unwrap_or(Value::Undefined))
            } else {
                None
            }
        }
        ObjKind::Map(m) => (name == "size").then(|| Value::Number(m.entries.len() as f64)),
        ObjKind::Set(s) => (name == "size").then(|| Value::Number(s.values.len() as f64)),
        ObjKind::Regex(re) => match name {
            "source" => Some(Value::str(re.source.clone())),
            "flags" => Some(Value::str(re.flags.clone())),
            "global" => Some(Value::Bool(re.global)),
            "ignoreCase" => Some(Value::Bool(re.ignore_case)),
            "multiline" => Some(Value::Bool(re.multiline)),
            "dotAll" => Some(Value::Bool(re.dot_all)),
            "unicode" => Some(Value::Bool(re.unicode)),
            "sticky" => Some(Value::Bool(false)),
            _ => None,
        },
        _ => None,
    }
}

enum PropStep {
    Val(Value),
    Getter(Value),
    Proto(ObjRef),
    Undef,
}

fn classify_slot(slot: &PropSlot) -> PropStep {
    match slot {
        PropSlot::Value(v) => PropStep::Val(v.clone()),
        PropSlot::Accessor { get, .. } => match get {
            Some(f) => PropStep::Getter(f.clone()),
            None => PropStep::Undef,
        },
    }
}

fn classify_fallback_prop(b: &ObjData, name: &str) -> PropStep {
    if let Some(v) = kind_prop_lookup(&b.kind, name) {
        PropStep::Val(v)
    } else if let Some(tag) = kind_tag_of(&b.kind) {
        if let Some(m) = crate::builtins::method_for(tag, name) {
            PropStep::Val(m)
        } else if let Some(p) = &b.proto {
            PropStep::Proto(p.clone())
        } else {
            PropStep::Undef
        }
    } else if let Some(p) = &b.proto {
        PropStep::Proto(p.clone())
    } else {
        PropStep::Undef
    }
}

impl Vm {
    pub(super) fn exec_get_index(&mut self, obj: &Value, key: &Value) -> Result<Value, Value> {
        match key {
            Value::Str(s) => match obj {
                Value::Object(o) => self.get_prop_object(o, o, s),
                other => self.get_prop(other, s),
            },
            other => {
                let name = to_property_key_string(other);
                self.get_prop(obj, &name)
            }
        }
    }

    pub(super) fn exec_set_index(
        &mut self,
        obj: &Value,
        key: &Value,
        value: Value,
    ) -> Result<(), Value> {
        match key {
            Value::Str(s) => self.set_prop_rc(obj, s, value),
            other => {
                let name = to_property_key_string(other);
                self.set_prop(obj, &name, value)
            }
        }
    }

    pub(super) fn get_prop_ic(
        &mut self,
        obj: &Value,
        name: &Rc<str>,
        op_ip: usize,
    ) -> Result<Value, Value> {
        match obj {
            Value::Object(o) => {
                let hint = self
                    .frames
                    .last()
                    .map_or(crate::bytecode::IC_UNSET as usize, |f| {
                        f.chunk.ic_hint(op_ip)
                    });
                let step = {
                    let b = o.borrow();
                    if let Some((idx, slot)) = b.props.get_with_hint(name, hint) {
                        if let Some(f) = self.frames.last() {
                            f.chunk.set_ic_hint(op_ip, idx);
                        }
                        classify_slot(slot)
                    } else {
                        classify_fallback_prop(&b, name)
                    }
                };
                match step {
                    PropStep::Val(v) => Ok(v),
                    PropStep::Getter(f) => self.call_value(f, Value::Object(o.clone()), &[]),
                    PropStep::Proto(p) => self.get_prop_object(o, &p, name),
                    PropStep::Undef => Ok(Value::Undefined),
                }
            }
            other => self.get_prop(other, name),
        }
    }

    pub fn get_prop(&mut self, obj: &Value, name: &str) -> Result<Value, Value> {
        match obj {
            Value::Undefined | Value::Null | Value::Uninit => Err(self.type_error(format!(
                "'{}' のプロパティ '{name}' は読み取れません（undefined/null）",
                if matches!(obj, Value::Null) {
                    "null"
                } else {
                    "undefined"
                }
            ))),
            Value::Str(s) => crate::builtins::string_get_prop(self, s, name),
            Value::Number(_) | Value::Bool(_) => Ok(Value::Undefined),
            Value::Object(o) => self.get_prop_object(o, o, name),
        }
    }

    fn get_prop_object(
        &mut self,
        receiver: &ObjRef,
        obj_rc: &ObjRef,
        name: &str,
    ) -> Result<Value, Value> {
        let step = {
            let b = obj_rc.borrow();
            if let Some(slot) = b.props.get(name) {
                classify_slot(slot)
            } else {
                classify_fallback_prop(&b, name)
            }
        };
        match step {
            PropStep::Val(v) => Ok(v),
            PropStep::Getter(f) => self.call_value(f, Value::Object(receiver.clone()), &[]),
            PropStep::Proto(p) => self.get_prop_object(receiver, &p, name),
            PropStep::Undef => Ok(Value::Undefined),
        }
    }

    pub fn set_prop(&mut self, obj: &Value, name: &str, value: Value) -> Result<(), Value> {
        let Value::Object(o) = obj else {
            if obj.is_nullish() {
                return Err(self.type_error(format!(
                    "プロパティ '{name}' を設定できません（undefined/null）"
                )));
            }
            return Ok(());
        };
        self.set_prop_object(o, name, None, value)
    }

    pub(super) fn set_prop_rc(
        &mut self,
        obj: &Value,
        name: &Rc<str>,
        value: Value,
    ) -> Result<(), Value> {
        let Value::Object(o) = obj else {
            if obj.is_nullish() {
                return Err(self.type_error(format!(
                    "プロパティ '{name}' を設定できません（undefined/null）"
                )));
            }
            return Ok(());
        };
        self.set_prop_object(o, name, Some(name), value)
    }

    fn set_prop_object(
        &mut self,
        obj_rc: &ObjRef,
        name: &str,
        name_rc: Option<&Rc<str>>,
        value: Value,
    ) -> Result<(), Value> {
        let proto = {
            let mut b = obj_rc.borrow_mut();
            if let Some(slot) = b.props.get_mut(name) {
                match slot {
                    PropSlot::Value(v) => {
                        *v = value;
                        return Ok(());
                    }
                    PropSlot::Accessor { set, .. } => {
                        let s = set.clone();
                        drop(b);
                        if let Some(f) = s {
                            self.call_value(f, Value::Object(obj_rc.clone()), &[value])?;
                        }
                        return Ok(());
                    }
                }
            }
            if let ObjKind::Array(items) = &mut b.kind {
                if name == "length" {
                    let n = to_number(&value).max(0.0) as usize;
                    items.resize(n, Value::Undefined);
                    return Ok(());
                } else if let Ok(idx) = name.parse::<usize>() {
                    if idx >= items.len() {
                        items.resize(idx + 1, Value::Undefined);
                    }
                    items[idx] = value;
                    return Ok(());
                }
            }
            if b.proto.is_none() {
                let key = name_rc.cloned().unwrap_or_else(|| Rc::from(name));
                b.props.set_value(key, value);
                return Ok(());
            }
            b.proto.clone()
        };
        if let Some(p) = &proto {
            if let Some(setter) = self.find_proto_accessor_setter(p, name) {
                self.call_value(setter, Value::Object(obj_rc.clone()), &[value])?;
                return Ok(());
            }
        }
        let key = name_rc.cloned().unwrap_or_else(|| Rc::from(name));
        obj_rc.borrow_mut().props.set_value(key, value);
        Ok(())
    }

    fn find_proto_accessor_setter(&self, obj_rc: &ObjRef, name: &str) -> Option<Value> {
        let b = obj_rc.borrow();
        if let Some(PropSlot::Accessor { set, .. }) = b.props.get(name) {
            return set.clone();
        }
        if b.props.get(name).is_some() {
            return None;
        }
        let proto = b.proto.clone();
        drop(b);
        proto.and_then(|p| self.find_proto_accessor_setter(&p, name))
    }

    pub(super) fn delete_prop(&mut self, obj: &Value, name: &str) -> bool {
        if let Value::Object(o) = obj {
            if let ObjKind::Array(items) = &mut o.borrow_mut().kind {
                if let Ok(idx) = name.parse::<usize>() {
                    if idx < items.len() {
                        items[idx] = Value::Undefined;
                        return true;
                    }
                }
            }
            return o.borrow_mut().props.delete(name);
        }
        true
    }

    pub fn has_prop(&self, o: &ObjRef, name: &str) -> bool {
        let b = o.borrow();
        if b.props.get(name).is_some() {
            return true;
        }
        if let ObjKind::Array(items) = &b.kind {
            if name == "length" {
                return true;
            }
            if let Ok(i) = name.parse::<usize>() {
                return i < items.len();
            }
        }
        if let Some(p) = &b.proto {
            return self.has_prop(p, name);
        }
        false
    }

    pub fn materialize_iter_array(&mut self, v: &Value) -> Result<Vec<Value>, Value> {
        match v {
            Value::Object(o) => {
                let b = o.borrow();
                match &b.kind {
                    ObjKind::Array(items) => Ok(items.clone()),
                    ObjKind::Map(m) => Ok(m
                        .entries
                        .iter()
                        .map(|(k, val)| self.alloc_array(vec![k.clone(), val.clone()]))
                        .collect()),
                    ObjKind::Set(s) => Ok(s.values.clone()),
                    _ => Err(self.type_error(
                        "この値は反復できません（for...of/スプレッド対象ではありません）",
                    )),
                }
            }
            Value::Str(s) => Ok(s.chars().map(|c| Value::str(c.to_string())).collect()),
            _ => {
                Err(self
                    .type_error("この値は反復できません（for...of/スプレッド対象ではありません）"))
            }
        }
    }

    pub(super) fn op_instanceof(&self, left: &Value, right: &Value) -> Result<Value, Value> {
        let proto_target = match right {
            Value::Object(o) => {
                let b = o.borrow();
                match &b.kind {
                    ObjKind::Class(cd) => Some(cd.prototype.clone()),
                    _ => match b.props.get("prototype") {
                        Some(PropSlot::Value(Value::Object(p))) => Some(p.clone()),
                        _ => None,
                    },
                }
            }
            _ => None,
        };
        let Some(proto) = proto_target else {
            return Err(self.type_error("instanceof の右辺はコンストラクタではありません"));
        };
        let Value::Object(lo) = left else {
            return Ok(Value::Bool(false));
        };
        let mut cur = lo.borrow().proto.clone();
        while let Some(p) = cur {
            if Rc::ptr_eq(&p, &proto) {
                return Ok(Value::Bool(true));
            }
            cur = p.borrow().proto.clone();
        }
        Ok(Value::Bool(false))
    }
}
