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
    Value::Object(Rc::new(RefCell::new(ObjData::new(ObjKind::Regex(re)))))
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

/// 整数フォーマット高速パスを使う `f64` の絶対値上限（`i64` キャストで精度欠落しない範囲）。
const FAST_INT_FORMAT_LIMIT: f64 = 1e15;
/// スレッドローカル文字列スクラッチの上限容量（これを超えたら縮める）。
const MAX_STR_SCRATCH_CAP: usize = 64 * 1024;

thread_local! {
    static STR_SCRATCH: RefCell<String> = const { RefCell::new(String::new()) };
}

/// 再利用可能なスレッドローカル `String` バッファへ書き込み、単一アロケーションで `Rc<str>` を生成する。
#[inline]
pub(crate) fn with_str_buf_rc(f: impl FnOnce(&mut String)) -> Rc<str> {
    STR_SCRATCH.with(|cell| {
        if let Ok(mut buf) = cell.try_borrow_mut() {
            buf.clear();
            f(&mut buf);
            let out = Rc::from(buf.as_str());
            if buf.capacity() > MAX_STR_SCRATCH_CAP {
                buf.clear();
                buf.shrink_to(MAX_STR_SCRATCH_CAP);
            }
            out
        } else {
            let mut local = String::new();
            f(&mut local);
            Rc::from(local.as_str())
        }
    })
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
    if n.fract() == 0.0 && n.abs() < FAST_INT_FORMAT_LIMIT {
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
    use std::fmt::Write;
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
                ObjKind::Function(f) => {
                    let _ = write!(out, "function {}() {{ [native code] }}", f.name);
                }
                ObjKind::Native(n) => {
                    let _ = write!(out, "function {}() {{ [native code] }}", n.name);
                }
                ObjKind::Class(c) => {
                    let _ = write!(out, "class {} {{ }}", c.name);
                }
                ObjKind::Error => {
                    let name = match b.props.get("name") {
                        Some(PropSlot::Value(Value::Str(s))) => &**s,
                        _ => "Error",
                    };
                    let msg = match b.props.get("message") {
                        Some(PropSlot::Value(Value::Str(s))) => &**s,
                        _ => "",
                    };
                    out.push_str(name);
                    if !msg.is_empty() {
                        out.push_str(": ");
                        out.push_str(msg);
                    }
                }
                ObjKind::Regex(re) => {
                    let _ = write!(out, "/{}/{}", re.source, re.flags);
                }
                ObjKind::Map(_) => out.push_str("[object Map]"),
                ObjKind::Set(_) => out.push_str("[object Set]"),
                ObjKind::Plain => out.push_str("[object Object]"),
                ObjKind::Promise(pd) => match &pd.state {
                    PromiseState::Pending => out.push_str("Promise { <pending> }"),
                    PromiseState::Fulfilled(v) => {
                        out.push_str("Promise { ");
                        to_string_into(v, out);
                        out.push_str(" }");
                    }
                    PromiseState::Rejected(v) => {
                        out.push_str("Promise { <rejected> ");
                        to_string_into(v, out);
                        out.push_str(" }");
                    }
                },
            }
        }
    }
}

pub fn to_string_basic(v: &Value) -> String {
    if let Value::Str(s) = v {
        return s.to_string();
    }
    let mut out = String::new();
    to_string_into(v, &mut out);
    out
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
        (Value::Str(x), Value::Str(y)) => Value::Str(with_str_buf_rc(|buf| {
            buf.push_str(x);
            buf.push_str(y);
        })),
        (Value::Str(x), other) => Value::Str(with_str_buf_rc(|buf| {
            buf.push_str(x);
            to_string_into(other, buf);
        })),
        (other, Value::Str(y)) => Value::Str(with_str_buf_rc(|buf| {
            to_string_into(other, buf);
            buf.push_str(y);
        })),
        (Value::Object(_), _) | (_, Value::Object(_)) => {
            let pa = to_primitive(a);
            let pb = to_primitive(b);
            if matches!(pa, Value::Str(_)) || matches!(pb, Value::Str(_)) {
                Value::Str(with_str_buf_rc(|buf| {
                    to_string_into(&pa, buf);
                    to_string_into(&pb, buf);
                }))
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
