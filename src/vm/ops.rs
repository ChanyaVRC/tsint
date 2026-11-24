//! JS セマンティクス寄りの純粋変換・算術/比較/等価演算ヘルパー群（Vm 不要）。

use std::cell::RefCell;
use std::rc::Rc;

use crate::value::*;

/// 呼び出し可能（関数・ネイティブ関数）かどうか。
pub fn is_callable(v: &Value) -> bool {
    matches!(
        v,
        Value::Object(o) if matches!(o.borrow().kind, ObjKind::Function(_) | ObjKind::Native(_))
    )
}

pub fn array_values(v: &Value) -> Vec<Value> {
    if let Value::Object(o) = v {
        if let ObjKind::Array(items) = &o.borrow().kind {
            return items.clone();
        }
    }
    Vec::new()
}

/// コンパイル済み正規表現から `RegExp` インスタンスを作る。
pub fn new_regex_value(re: Rc<crate::regex::Regex>) -> Value {
    let mut o = ObjData::new(ObjKind::Regex(re));
    o.props.set_value(Rc::from("lastIndex"), Value::Number(0.0));
    Value::Object(Rc::new(RefCell::new(o)))
}

#[inline]
pub fn to_number(v: &Value) -> f64 {
    match v {
        Value::Number(n) => *n,
        Value::Undefined | Value::Uninit => f64::NAN,
        Value::Null => 0.0,
        Value::Bool(b) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        Value::Str(s) => parse_js_number_str(s),
        Value::Object(_) => to_number_from_primitive(&to_primitive(v)),
    }
}

fn parse_js_number_str(s: &str) -> f64 {
    let t = s.trim();
    if t.is_empty() {
        0.0
    } else if t == "Infinity" || t == "+Infinity" {
        f64::INFINITY
    } else if t == "-Infinity" {
        f64::NEG_INFINITY
    } else if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        i64::from_str_radix(hex, 16)
            .map(|n| n as f64)
            .unwrap_or(f64::NAN)
    } else {
        t.parse::<f64>().unwrap_or(f64::NAN)
    }
}

fn to_number_from_primitive(v: &Value) -> f64 {
    match v {
        Value::Object(_) => f64::NAN,
        other => to_number(other),
    }
}

#[inline]
pub fn to_int32(v: &Value) -> i32 {
    let n = to_number(v);
    if !n.is_finite() {
        return 0;
    }
    n as i64 as i32
}

#[inline]
pub fn to_uint32(v: &Value) -> u32 {
    let n = to_number(v);
    if !n.is_finite() {
        return 0;
    }
    n as i64 as u32
}

pub fn to_primitive(v: &Value) -> Value {
    match v {
        Value::Object(_) => Value::str(to_string_basic(v)),
        other => other.clone(),
    }
}

pub(crate) fn number_to_js_string_into(n: f64, out: &mut String) {
    use std::fmt::Write;
    if n.is_nan() {
        out.push_str("NaN");
        return;
    }
    if n.is_infinite() {
        if n > 0.0 {
            out.push_str("Infinity");
        } else {
            out.push_str("-Infinity");
        }
        return;
    }
    if n == 0.0 {
        out.push('0');
        return;
    }
    if n.fract() == 0.0 && n.abs() < 1e15 {
        let _ = write!(out, "{}", n as i64);
        return;
    }
    let start = out.len();
    let _ = write!(out, "{n}");
    let slice = &out[start..];
    if slice.contains('e') {
        return;
    }
    if !slice.contains('.') {
        out.push_str(".0");
    }
    let trimmed_len = out[start..]
        .trim_end_matches('0')
        .trim_end_matches('.')
        .len();
    out.truncate(start + trimmed_len);
}

pub fn number_to_js_string(n: f64) -> String {
    let mut s = String::new();
    number_to_js_string_into(n, &mut s);
    s
}

pub(crate) fn to_string_into(v: &Value, out: &mut String) {
    match v {
        Value::Str(s) => out.push_str(s),
        Value::Number(n) => number_to_js_string_into(*n, out),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Undefined | Value::Uninit => out.push_str("undefined"),
        Value::Null => out.push_str("null"),
        Value::Object(o) => {
            let b = o.borrow();
            match &b.kind {
                ObjKind::Array(items) => {
                    for (i, it) in items.iter().enumerate() {
                        if i > 0 {
                            out.push(',');
                        }
                        if !matches!(it, Value::Undefined | Value::Null | Value::Uninit) {
                            to_string_into(it, out);
                        }
                    }
                }
                _ => {
                    drop(b);
                    out.push_str(&to_string_basic(v));
                }
            }
        }
    }
}

pub fn to_string_basic(v: &Value) -> String {
    match v {
        Value::Undefined | Value::Uninit => "undefined".to_string(),
        Value::Null => "null".to_string(),
        Value::Bool(b) => if *b { "true" } else { "false" }.to_string(),
        Value::Number(n) => number_to_js_string(*n),
        Value::Str(s) => s.to_string(),
        Value::Object(o) => {
            let b = o.borrow();
            match &b.kind {
                ObjKind::Array(items) => {
                    let mut out = String::new();
                    for (i, it) in items.iter().enumerate() {
                        if i > 0 {
                            out.push(',');
                        }
                        if !matches!(it, Value::Undefined | Value::Null | Value::Uninit) {
                            to_string_into(it, &mut out);
                        }
                    }
                    out
                }
                ObjKind::Function(f) => format!("function {}() {{ [native code] }}", f.name),
                ObjKind::Native(n) => format!("function {}() {{ [native code] }}", n.name),
                ObjKind::Class(c) => format!("class {} {{ }}", c.name),
                ObjKind::Error => {
                    let name = match b.props.get("name") {
                        Some(PropSlot::Value(Value::Str(s))) => &**s,
                        _ => "Error",
                    };
                    let msg = match b.props.get("message") {
                        Some(PropSlot::Value(Value::Str(s))) => &**s,
                        _ => "",
                    };
                    if msg.is_empty() {
                        name.to_string()
                    } else {
                        format!("{name}: {msg}")
                    }
                }
                ObjKind::Regex(re) => format!("/{}/{}", re.source, re.flags),
                ObjKind::Map(_) => "[object Map]".to_string(),
                ObjKind::Set(_) => "[object Set]".to_string(),
                ObjKind::Plain => "[object Object]".to_string(),
                ObjKind::Promise(pd) => match &pd.state {
                    PromiseState::Pending => "Promise { <pending> }".to_string(),
                    PromiseState::Fulfilled(v) => format!("Promise {{ {} }}", to_string_basic(v)),
                    PromiseState::Rejected(v) => {
                        format!("Promise {{ <rejected> {} }}", to_string_basic(v))
                    }
                },
            }
        }
    }
}

pub fn to_property_key_string(v: &Value) -> String {
    match v {
        Value::Str(s) => s.to_string(),
        Value::Number(n) => number_to_js_string(*n),
        other => to_string_basic(other),
    }
}

#[inline]
pub(super) fn js_add(a: &Value, b: &Value) -> Value {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => Value::Number(*x + *y),
        (Value::Str(x), Value::Str(y)) => {
            let mut buf = String::with_capacity(x.len() + y.len());
            buf.push_str(x);
            buf.push_str(y);
            Value::Str(buf.into())
        }
        (Value::Str(x), other) => {
            let mut buf = String::with_capacity(x.len() + 16);
            buf.push_str(x);
            to_string_into(other, &mut buf);
            Value::Str(buf.into())
        }
        (other, Value::Str(y)) => {
            let mut buf = String::with_capacity(y.len() + 16);
            to_string_into(other, &mut buf);
            buf.push_str(y);
            Value::Str(buf.into())
        }
        (Value::Object(_), _) | (_, Value::Object(_)) => {
            let pa = to_primitive(a);
            let pb = to_primitive(b);
            if matches!(pa, Value::Str(_)) || matches!(pb, Value::Str(_)) {
                let mut buf = String::new();
                to_string_into(&pa, &mut buf);
                to_string_into(&pb, &mut buf);
                Value::Str(buf.into())
            } else {
                Value::Number(to_number(&pa) + to_number(&pb))
            }
        }
        _ => Value::Number(to_number(a) + to_number(b)),
    }
}

/// `<` `>` `<=` `>=` 共通の比較。NaN が絡む場合は None（常に false になる）。
#[inline]
pub(super) fn js_compare(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.partial_cmp(y),
        (Value::Str(x), Value::Str(y)) => Some(x.as_ref().cmp(y.as_ref())),
        (Value::Object(_), _) | (_, Value::Object(_)) => {
            let pa = to_primitive(a);
            let pb = to_primitive(b);
            if let (Value::Str(x), Value::Str(y)) = (&pa, &pb) {
                return Some(x.as_ref().cmp(y.as_ref()));
            }
            to_number(&pa).partial_cmp(&to_number(&pb))
        }
        _ => to_number(a).partial_cmp(&to_number(b)),
    }
}

/// `==` の主要規則（Symbol は非対応のため考慮しない）。
pub fn loose_eq(a: &Value, b: &Value) -> bool {
    use Value::*;
    match (a, b) {
        (Undefined | Null, Undefined | Null) => true,
        (Number(_), Number(_)) | (Str(_), Str(_)) | (Bool(_), Bool(_)) | (Object(_), Object(_)) => {
            strict_eq(a, b)
        }
        (Number(_), Str(_)) => to_number(a) == to_number(b),
        (Str(_), Number(_)) => to_number(a) == to_number(b),
        (Bool(_), _) => loose_eq(&Value::Number(to_number(a)), b),
        (_, Bool(_)) => loose_eq(a, &Value::Number(to_number(b))),
        (Object(_), Number(_) | Str(_)) => loose_eq(&to_primitive(a), b),
        (Number(_) | Str(_), Object(_)) => loose_eq(a, &to_primitive(b)),
        _ => false,
    }
}
