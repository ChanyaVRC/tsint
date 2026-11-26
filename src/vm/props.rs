//! プロパティ/添字アクセス、プロトタイプ探索、`instanceof`、反復実体化。

use std::rc::Rc;

use crate::value::*;

use super::ops::{to_number, to_property_key_string};
use super::{KindTag, Vm, MAX_ARRAY_LEN};

/// `#private` や `%time` 等の内部プロパティキーかどうか（T-23）。
#[inline]
pub(crate) fn is_internal_prop_key(k: &str) -> bool {
    k.starts_with('#') || k.starts_with('%')
}

/// ES 仕様の正規な配列添字文字列か判定して `usize` へ変換する（T-23）。
/// `"01"`・`"+1"`・空白付き等は配列添字として扱わない。
#[inline]
pub(crate) fn parse_canonical_array_index(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    if bytes.len() == 1 {
        let b = bytes[0];
        return (b.is_ascii_digit()).then_some((b - b'0') as usize);
    }
    if !(b'1'..=b'9').contains(&bytes[0]) || bytes.len() > 10 {
        return None;
    }
    let mut val: u64 = 0;
    for &b in bytes {
        if !b.is_ascii_digit() {
            return None;
        }
        val = val * 10 + (b - b'0') as u64;
    }
    if val < u32::MAX as u64 {
        usize::try_from(val).ok()
    } else {
        None
    }
}

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
                    .filter(|(k, s)| matches!(s, PropSlot::Value(_)) && !is_internal_prop_key(k))
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
            } else {
                parse_canonical_array_index(name)
                    .map(|idx| items.get(idx).cloned().unwrap_or(Value::Undefined))
            }
        }
        ObjKind::Map(m) => (name == "size").then(|| Value::Number(m.entries.len() as f64)),
        ObjKind::Set(s) => (name == "size").then(|| Value::Number(s.values.len() as f64)),
        ObjKind::Regex(re) => match name {
            "source" => Some(Value::Str(re.source.clone())),
            "flags" => Some(Value::Str(re.flags.clone())),
            "lastIndex" => Some(Value::Number(0.0)),
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
        PropSlot::Accessor(acc) => match &acc.get {
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
                    PropSlot::Accessor(acc) => {
                        let s = acc.set.clone();
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
                    let n = to_number(&value);
                    if !n.is_finite() || n < 0.0 || n.fract() != 0.0 || n > (u32::MAX as f64) {
                        drop(b);
                        return Err(self.range_error("Invalid array length"));
                    }
                    let new_len = n as usize;
                    if new_len > MAX_ARRAY_LEN {
                        drop(b);
                        return Err(self.range_error(format!(
                            "Invalid array length: 上限 ({MAX_ARRAY_LEN}) を超えています"
                        )));
                    }
                    items.resize(new_len, Value::Undefined);
                    return Ok(());
                } else if let Some(idx) = parse_canonical_array_index(name) {
                    if idx >= MAX_ARRAY_LEN {
                        drop(b);
                        return Err(self.range_error(format!(
                            "配列添字 {idx} が上限 ({MAX_ARRAY_LEN}) を超えています"
                        )));
                    }
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
        let mut cur = Some(obj_rc.clone());
        while let Some(o) = cur {
            let b = o.borrow();
            if let Some(slot) = b.props.get(name) {
                return match slot {
                    PropSlot::Accessor(acc) => acc.set.clone(),
                    PropSlot::Value(_) => None,
                };
            }
            cur = b.proto.clone();
        }
        None
    }

    pub(super) fn delete_prop(&mut self, obj: &Value, name: &str) -> bool {
        if let Value::Object(o) = obj {
            if let ObjKind::Array(items) = &mut o.borrow_mut().kind {
                if let Some(idx) = parse_canonical_array_index(name) {
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
        let mut cur = Some(o.clone());
        while let Some(obj) = cur {
            let b = obj.borrow();
            if b.props.get(name).is_some() {
                return true;
            }
            if let ObjKind::Array(items) = &b.kind {
                if name == "length" {
                    return true;
                }
                if let Some(i) = parse_canonical_array_index(name) {
                    return i < items.len();
                }
            }
            cur = b.proto.clone();
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
        enum RightTarget {
            Proto(ObjRef),
            Builtin(BuiltinCtorTag),
            CallableNoProto,
        }
        let target = match right {
            Value::Object(o) => {
                let b = o.borrow();
                match &b.kind {
                    ObjKind::Class(cd) => Some(RightTarget::Proto(cd.prototype.clone())),
                    ObjKind::Native(n) => {
                        if let Some(tag) = n.ctor_tag {
                            Some(RightTarget::Builtin(tag))
                        } else if let Some(PropSlot::Value(Value::Object(p))) =
                            b.props.get("prototype")
                        {
                            Some(RightTarget::Proto(p.clone()))
                        } else {
                            Some(RightTarget::CallableNoProto)
                        }
                    }
                    ObjKind::Function(_) => match b.props.get("prototype") {
                        Some(PropSlot::Value(Value::Object(p))) => {
                            Some(RightTarget::Proto(p.clone()))
                        }
                        _ => Some(RightTarget::CallableNoProto),
                    },
                    _ => None,
                }
            }
            _ => None,
        };
        let Some(target) = target else {
            return Err(self.type_error("instanceof の右辺はコンストラクタではありません"));
        };
        let Value::Object(lo) = left else {
            return Ok(Value::Bool(false));
        };
        match target {
            RightTarget::CallableNoProto => Ok(Value::Bool(false)),
            RightTarget::Builtin(tag) => {
                let lb = lo.borrow();
                let matched = match tag {
                    BuiltinCtorTag::Object => true,
                    BuiltinCtorTag::Function => matches!(
                        lb.kind,
                        ObjKind::Function(_) | ObjKind::Native(_) | ObjKind::Class(_)
                    ),
                    BuiltinCtorTag::Array => matches!(lb.kind, ObjKind::Array(_)),
                    BuiltinCtorTag::Map => matches!(lb.kind, ObjKind::Map(_)),
                    BuiltinCtorTag::Set => matches!(lb.kind, ObjKind::Set(_)),
                    BuiltinCtorTag::Promise => matches!(lb.kind, ObjKind::Promise(_)),
                    BuiltinCtorTag::RegExp => matches!(lb.kind, ObjKind::Regex(_)),
                    BuiltinCtorTag::Date => {
                        matches!(lb.kind, ObjKind::Plain) && lb.props.get("%time").is_some()
                    }
                    BuiltinCtorTag::Error => matches!(lb.kind, ObjKind::Error),
                    BuiltinCtorTag::TypeError => is_error_named(&lb, "TypeError"),
                    BuiltinCtorTag::RangeError => is_error_named(&lb, "RangeError"),
                    BuiltinCtorTag::ReferenceError => is_error_named(&lb, "ReferenceError"),
                    BuiltinCtorTag::SyntaxError => is_error_named(&lb, "SyntaxError"),
                };
                Ok(Value::Bool(matched))
            }
            RightTarget::Proto(proto) => {
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
    }
}

fn is_error_named(b: &ObjData, expected: &str) -> bool {
    matches!(b.kind, ObjKind::Error)
        && matches!(
            b.props.get("name"),
            Some(PropSlot::Value(Value::Str(s))) if &**s == expected
        )
}
