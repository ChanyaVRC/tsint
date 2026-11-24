//! JSON（ストリーミングシリアライザ / バイトベースパーサ）。

use std::rc::Rc;

use super::{native, value_as_cow_str};
use crate::value::*;
use crate::vm::{number_to_js_string_into, with_str_buf_rc, Vm};

/// `f64` で正確に表現できる JSON 整数桁数の高速パス上限。
const FAST_INT_MAX_DIGITS: usize = 15;

pub(super) fn install_json(vm: &mut Vm) {
    let json = Value::new_plain_object();
    let stringify = native("stringify", 1, |_vm, _this, args| {
        let indent = match args.get(2) {
            Some(Value::Number(n)) => n.max(0.0) as usize,
            _ => 0,
        };
        let target = args.first().unwrap_or(&Value::Undefined);
        let mut ok = false;
        let rc = with_str_buf_rc(|out| {
            ok = json_stringify_into(target, indent, 0, out);
        });
        if ok {
            Ok(Value::Str(rc))
        } else {
            Ok(Value::Undefined)
        }
    });
    let parse = native("parse", 1, |vm, _this, args| {
        let s = value_as_cow_str(args.first().unwrap_or(&Value::Undefined));
        json_parse(vm, &s).map_err(|e| vm.syntax_error(format!("JSON.parse: {e}")))
    });
    if let Value::Object(o) = &json {
        let mut b = o.borrow_mut();
        b.props.set_value(Rc::from("stringify"), stringify);
        b.props.set_value(Rc::from("parse"), parse);
    }
    vm.define_global("JSON", json);
}

fn json_quote_into(s: &str, out: &mut String) {
    use std::fmt::Write;
    out.push('"');
    let bytes = s.as_bytes();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        let esc = match b {
            b'"' => Some("\\\""),
            b'\\' => Some("\\\\"),
            b'\n' => Some("\\n"),
            b'\r' => Some("\\r"),
            b'\t' => Some("\\t"),
            0..=0x1f => None,
            _ => {
                i += 1;
                continue;
            }
        };
        if start < i {
            out.push_str(&s[start..i]);
        }
        if let Some(e) = esc {
            out.push_str(e);
        } else {
            let _ = write!(out, "\\u{:04x}", b as u32);
        }
        i += 1;
        start = i;
    }
    if start < bytes.len() {
        out.push_str(&s[start..]);
    }
    out.push('"');
}

fn push_indent(out: &mut String, spaces: usize) {
    for _ in 0..spaces {
        out.push(' ');
    }
}

fn json_stringify_into(v: &Value, indent: usize, depth: usize, out: &mut String) -> bool {
    match v {
        Value::Undefined | Value::Uninit => false,
        Value::Null => {
            out.push_str("null");
            true
        }
        Value::Bool(b) => {
            out.push_str(if *b { "true" } else { "false" });
            true
        }
        Value::Number(n) => {
            if n.is_finite() {
                number_to_js_string_into(*n, out);
            } else {
                out.push_str("null");
            }
            true
        }
        Value::Str(s) => {
            json_quote_into(s, out);
            true
        }
        Value::Object(o) => {
            let b = o.borrow();
            match &b.kind {
                ObjKind::Function(_) | ObjKind::Native(_) | ObjKind::Class(_) => false,
                ObjKind::Array(items) => {
                    if items.is_empty() {
                        out.push_str("[]");
                        return true;
                    }
                    out.push('[');
                    if indent > 0 {
                        out.push('\n');
                    }
                    for (i, it) in items.iter().enumerate() {
                        if i > 0 {
                            out.push(',');
                            if indent > 0 {
                                out.push('\n');
                            }
                        }
                        if indent > 0 {
                            push_indent(out, indent * (depth + 1));
                        }
                        if !json_stringify_into(it, indent, depth + 1, out) {
                            out.push_str("null");
                        }
                    }
                    if indent > 0 {
                        out.push('\n');
                        push_indent(out, indent * depth);
                    }
                    out.push(']');
                    true
                }
                _ => {
                    let start_len = out.len();
                    out.push('{');
                    let mut count = 0usize;
                    for (k, slot) in b.props.iter() {
                        let PropSlot::Value(val) = slot else {
                            continue;
                        };
                        let entry_mark = out.len();
                        if count > 0 {
                            out.push(',');
                        }
                        if indent > 0 {
                            out.push('\n');
                            push_indent(out, indent * (depth + 1));
                        }
                        json_quote_into(k, out);
                        out.push(':');
                        if json_stringify_into(val, indent, depth + 1, out) {
                            count += 1;
                        } else {
                            out.truncate(entry_mark);
                        }
                    }
                    if count == 0 {
                        out.truncate(start_len);
                        out.push_str("{}");
                        return true;
                    }
                    if indent > 0 {
                        out.push('\n');
                        push_indent(out, indent * depth);
                    }
                    out.push('}');
                    true
                }
            }
        }
    }
}

#[derive(Debug)]
struct JsonError(String);

impl std::fmt::Display for JsonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for JsonError {}

struct JsonParser<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
}

fn json_parse(vm: &Vm, s: &str) -> Result<Value, JsonError> {
    let mut p = JsonParser {
        src: s,
        bytes: s.as_bytes(),
        pos: 0,
    };
    p.skip_ws();
    let v = p.parse_value(vm)?;
    p.skip_ws();
    if p.pos != p.bytes.len() {
        return Err(JsonError("末尾に余分な文字があります".to_string()));
    }
    Ok(v)
}

impl<'a> JsonParser<'a> {
    #[inline]
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    #[inline]
    fn skip_ws(&mut self) {
        while let Some(&b) = self.bytes.get(self.pos) {
            if matches!(b, b' ' | b'\t' | b'\n' | b'\r') {
                self.pos += 1;
            } else if b >= 0x80 {
                let Some(ch) = self.src[self.pos..].chars().next() else {
                    break;
                };
                if ch.is_whitespace() {
                    self.pos += ch.len_utf8();
                } else {
                    break;
                }
            } else {
                break;
            }
        }
    }

    #[inline]
    fn expect(&mut self, c: u8) -> Result<(), JsonError> {
        if self.peek() == Some(c) {
            self.pos += 1;
            Ok(())
        } else {
            Err(JsonError(format!(
                "'{}' を期待しました（位置 {}）",
                c as char, self.pos
            )))
        }
    }

    fn parse_value(&mut self, vm: &Vm) -> Result<Value, JsonError> {
        self.skip_ws();
        match self.peek() {
            Some(b'{') => self.parse_object(vm),
            Some(b'[') => self.parse_array(vm),
            Some(b'"') => Ok(Value::Str(self.parse_string_rc()?)),
            Some(b't') => {
                self.expect_word(b"true")?;
                Ok(Value::Bool(true))
            }
            Some(b'f') => {
                self.expect_word(b"false")?;
                Ok(Value::Bool(false))
            }
            Some(b'n') => {
                self.expect_word(b"null")?;
                Ok(Value::Null)
            }
            Some(b'-') | Some(b'0'..=b'9') => self.parse_number(),
            _ => Err(JsonError(format!("値を期待しました（位置 {}）", self.pos))),
        }
    }

    fn expect_word(&mut self, w: &[u8]) -> Result<(), JsonError> {
        if self.bytes[self.pos..].starts_with(w) {
            self.pos += w.len();
            Ok(())
        } else {
            Err(JsonError(format!(
                "'{}' を期待しました（位置 {}）",
                std::str::from_utf8(w).unwrap_or(""),
                self.pos
            )))
        }
    }

    fn parse_object(&mut self, vm: &Vm) -> Result<Value, JsonError> {
        self.expect(b'{')?;
        let mut obj_data = ObjData::new(ObjKind::Plain);
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(vm.alloc_obj(obj_data));
        }
        loop {
            self.skip_ws();
            let key = self.parse_string_rc()?;
            self.skip_ws();
            self.expect(b':')?;
            let val = self.parse_value(vm)?;
            obj_data.props.set_value(key, val);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                _ => {
                    return Err(JsonError(format!(
                        "',' か '}}' を期待しました（位置 {}）",
                        self.pos
                    )))
                }
            }
        }
        Ok(vm.alloc_obj(obj_data))
    }

    fn parse_array(&mut self, vm: &Vm) -> Result<Value, JsonError> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(vm.alloc_array(items));
        }
        loop {
            let v = self.parse_value(vm)?;
            items.push(v);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                _ => {
                    return Err(JsonError(format!(
                        "',' か ']' を期待しました（位置 {}）",
                        self.pos
                    )))
                }
            }
        }
        Ok(vm.alloc_array(items))
    }

    fn parse_string_rc(&mut self) -> Result<Rc<str>, JsonError> {
        self.expect(b'"')?;
        let start = self.pos;
        // エスケープ無しの高速パス（大半の JSON 文字列・キーがここに落ちる）
        while let Some(&b) = self.bytes.get(self.pos) {
            if b == b'"' {
                let s = &self.src[start..self.pos];
                self.pos += 1;
                return Ok(Rc::from(s));
            }
            if b == b'\\' {
                break;
            }
            self.pos += 1;
        }
        let mut out = String::with_capacity(self.pos - start + 16);
        out.push_str(&self.src[start..self.pos]);
        loop {
            match self.peek() {
                None => return Err(JsonError("文字列が閉じられていません".to_string())),
                Some(b'"') => {
                    self.pos += 1;
                    break;
                }
                Some(b'\\') => {
                    self.pos += 1;
                    match self.peek() {
                        Some(b'"') => out.push('"'),
                        Some(b'\\') => out.push('\\'),
                        Some(b'/') => out.push('/'),
                        Some(b'n') => out.push('\n'),
                        Some(b't') => out.push('\t'),
                        Some(b'r') => out.push('\r'),
                        Some(b'b') => out.push('\u{8}'),
                        Some(b'f') => out.push('\u{c}'),
                        Some(b'u') => {
                            if self.pos + 5 > self.bytes.len() {
                                return Err(JsonError("不正な \\u エスケープです".to_string()));
                            }
                            let hex = &self.src[self.pos + 1..self.pos + 5];
                            let code = u32::from_str_radix(hex, 16)
                                .map_err(|_| JsonError("不正な \\u エスケープです".to_string()))?;
                            if let Some(c) = char::from_u32(code) {
                                out.push(c);
                            }
                            self.pos += 4;
                        }
                        _ => return Err(JsonError("不正なエスケープです".to_string())),
                    }
                    self.pos += 1;
                }
                Some(_) => {
                    let chunk_start = self.pos;
                    self.pos += 1;
                    while let Some(&b) = self.bytes.get(self.pos) {
                        if b == b'"' || b == b'\\' {
                            break;
                        }
                        self.pos += 1;
                    }
                    out.push_str(&self.src[chunk_start..self.pos]);
                }
            }
        }
        Ok(Rc::from(out))
    }

    fn parse_number(&mut self) -> Result<Value, JsonError> {
        let start = self.pos;
        let neg = if self.peek() == Some(b'-') {
            self.pos += 1;
            true
        } else {
            false
        };
        let mut int_acc: i64 = 0;
        let mut int_digits = 0usize;
        while let Some(&b) = self.bytes.get(self.pos) {
            if b.is_ascii_digit() {
                if int_digits < FAST_INT_MAX_DIGITS {
                    int_acc = int_acc * 10 + (b - b'0') as i64;
                }
                int_digits += 1;
                self.pos += 1;
            } else {
                break;
            }
        }
        let mut is_float = false;
        if self.peek() == Some(b'.') {
            is_float = true;
            self.pos += 1;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            is_float = true;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.pos += 1;
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if !is_float && int_digits > 0 && int_digits <= FAST_INT_MAX_DIGITS {
            let n = if neg {
                -(int_acc as f64)
            } else {
                int_acc as f64
            };
            return Ok(Value::Number(n));
        }
        self.src[start..self.pos]
            .parse::<f64>()
            .map(Value::Number)
            .map_err(|_| JsonError("不正な数値です".to_string()))
    }
}
