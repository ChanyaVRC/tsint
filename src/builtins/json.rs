//! JSON（ストリーミングシリアライザ / バイトベースパーサ）。

use std::cell::RefCell;
use std::rc::Rc;

use super::{define_props_obj, native, value_as_cow_str};
use crate::value::*;
use crate::vm::{is_internal_prop_key, number_to_js_string_into, with_str_buf_rc, Vm};

/// `f64` で正確に表現できる JSON 整数桁数の高速パス上限。
const FAST_INT_MAX_DIGITS: usize = 15;
/// `JSON.stringify` / `JSON.parse` の最大ネスト深さ（T-6）。
const MAX_JSON_DEPTH: usize = 128;

thread_local! {
    static JSON_VISITED: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

enum JsonStringifyErr {
    Circular,
    MaxDepth,
}

pub(super) fn install_json(vm: &mut Vm) {
    define_props_obj(
        vm,
        "JSON",
        [
            (
                "stringify",
                native("stringify", 1, |vm, _this, args| {
                    // ES 仕様準拠: 数値は 0..=10 に丸め、文字列は先頭 10 文字に切る（T-12）。
                    let indent_str: String = match args.get(2) {
                        Some(Value::Number(n)) if n.is_finite() => {
                            let count = n.trunc().clamp(0.0, 10.0) as usize;
                            " ".repeat(count)
                        }
                        Some(Value::Number(n)) if *n > 0.0 => " ".repeat(10),
                        Some(Value::Str(s)) => s.chars().take(10).collect(),
                        _ => String::new(),
                    };
                    let target = args.first().unwrap_or(&Value::Undefined);
                    let mut result: Result<bool, JsonStringifyErr> = Ok(false);
                    let rc = with_str_buf_rc(|out| {
                        JSON_VISITED.with(|cell| {
                            if let Ok(mut visited) = cell.try_borrow_mut() {
                                visited.clear();
                                result =
                                    json_stringify_into(target, &indent_str, 0, &mut visited, out);
                                visited.clear();
                            } else {
                                let mut local = Vec::new();
                                result =
                                    json_stringify_into(target, &indent_str, 0, &mut local, out);
                            }
                        });
                    });
                    match result {
                        Ok(true) => Ok(Value::Str(rc)),
                        Ok(false) => Ok(Value::Undefined),
                        Err(JsonStringifyErr::Circular) => {
                            Err(vm.type_error("Converting circular structure to JSON"))
                        }
                        Err(JsonStringifyErr::MaxDepth) => {
                            Err(vm.range_error("Maximum JSON serialization depth exceeded"))
                        }
                    }
                }),
            ),
            (
                "parse",
                native("parse", 1, |vm, _this, args| {
                    let s = value_as_cow_str(args.first().unwrap_or(&Value::Undefined));
                    json_parse(vm, &s).map_err(|e| vm.syntax_error(format!("JSON.parse: {e}")))
                }),
            ),
        ],
    );
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

fn push_indent(out: &mut String, indent: &str, level: usize) {
    for _ in 0..level {
        out.push_str(indent);
    }
}

fn json_stringify_into(
    v: &Value,
    indent: &str,
    depth: usize,
    visited: &mut Vec<usize>,
    out: &mut String,
) -> Result<bool, JsonStringifyErr> {
    match v {
        Value::Undefined | Value::Uninit => Ok(false),
        Value::Null => {
            out.push_str("null");
            Ok(true)
        }
        Value::Bool(b) => {
            out.push_str(if *b { "true" } else { "false" });
            Ok(true)
        }
        Value::Number(n) => {
            if n.is_finite() {
                number_to_js_string_into(*n, out);
            } else {
                out.push_str("null");
            }
            Ok(true)
        }
        Value::Str(s) => {
            json_quote_into(s, out);
            Ok(true)
        }
        Value::Object(o) => {
            let b = o.borrow();
            if matches!(
                b.kind,
                ObjKind::Function(_) | ObjKind::Native(_) | ObjKind::Class(_)
            ) {
                return Ok(false);
            }
            let ptr = Rc::as_ptr(o) as usize;
            if visited.contains(&ptr) {
                return Err(JsonStringifyErr::Circular);
            }
            if depth >= MAX_JSON_DEPTH {
                return Err(JsonStringifyErr::MaxDepth);
            }
            visited.push(ptr);
            let res = match &b.kind {
                ObjKind::Array(items) => {
                    if items.is_empty() {
                        out.push_str("[]");
                        Ok(true)
                    } else {
                        out.push('[');
                        if !indent.is_empty() {
                            out.push('\n');
                        }
                        for (i, it) in items.iter().enumerate() {
                            if i > 0 {
                                out.push(',');
                                if !indent.is_empty() {
                                    out.push('\n');
                                }
                            }
                            if !indent.is_empty() {
                                push_indent(out, indent, depth + 1);
                            }
                            if !json_stringify_into(it, indent, depth + 1, visited, out)? {
                                out.push_str("null");
                            }
                        }
                        if !indent.is_empty() {
                            out.push('\n');
                            push_indent(out, indent, depth);
                        }
                        out.push(']');
                        Ok(true)
                    }
                }
                _ => {
                    let start_len = out.len();
                    out.push('{');
                    let mut count = 0usize;
                    for (k, slot) in b.props.iter() {
                        if is_internal_prop_key(k) {
                            continue;
                        }
                        let PropSlot::Value(val) = slot else {
                            continue;
                        };
                        let entry_mark = out.len();
                        if count > 0 {
                            out.push(',');
                        }
                        if !indent.is_empty() {
                            out.push('\n');
                            push_indent(out, indent, depth + 1);
                        }
                        json_quote_into(k, out);
                        out.push(':');
                        if json_stringify_into(val, indent, depth + 1, visited, out)? {
                            count += 1;
                        } else {
                            out.truncate(entry_mark);
                        }
                    }
                    if count == 0 {
                        out.truncate(start_len);
                        out.push_str("{}");
                        Ok(true)
                    } else {
                        if !indent.is_empty() {
                            out.push('\n');
                            push_indent(out, indent, depth);
                        }
                        out.push('}');
                        Ok(true)
                    }
                }
            };
            visited.pop();
            res
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
    depth: usize,
}

fn json_parse(vm: &Vm, s: &str) -> Result<Value, JsonError> {
    let mut p = JsonParser {
        src: s,
        bytes: s.as_bytes(),
        pos: 0,
        depth: 0,
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
        if self.depth >= MAX_JSON_DEPTH {
            return Err(JsonError("JSON のネストが深すぎます".to_string()));
        }
        self.depth += 1;
        let res = (|| {
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
        })();
        self.depth -= 1;
        res
    }

    fn parse_array(&mut self, vm: &Vm) -> Result<Value, JsonError> {
        if self.depth >= MAX_JSON_DEPTH {
            return Err(JsonError("JSON のネストが深すぎます".to_string()));
        }
        self.depth += 1;
        let res = (|| {
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
        })();
        self.depth -= 1;
        res
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
                            // バイト境界パニックを避けるため raw bytes から 4 桁 ASCII hex を読む（T-10）。
                            let code = self
                                .bytes
                                .get(self.pos + 1..self.pos + 5)
                                .and_then(parse_hex4_bytes)
                                .ok_or_else(|| {
                                    JsonError("不正な \\u エスケープです".to_string())
                                })?;
                            self.pos += 4;
                            if (0xD800..=0xDBFF).contains(&code) {
                                // サロゲートペアの結合（T-25）。
                                let low =
                                    if self.bytes.get(self.pos + 1..self.pos + 3) == Some(b"\\u") {
                                        self.bytes
                                            .get(self.pos + 3..self.pos + 7)
                                            .and_then(parse_hex4_bytes)
                                            .filter(|c| (0xDC00..=0xDFFF).contains(c))
                                    } else {
                                        None
                                    };
                                if let Some(lo) = low {
                                    let cp = 0x10000 + ((code - 0xD800) << 10) + (lo - 0xDC00);
                                    out.push(
                                        char::from_u32(cp).unwrap_or(char::REPLACEMENT_CHARACTER),
                                    );
                                    self.pos += 6;
                                } else {
                                    out.push(char::REPLACEMENT_CHARACTER);
                                }
                            } else if (0xDC00..=0xDFFF).contains(&code) {
                                out.push(char::REPLACEMENT_CHARACTER);
                            } else {
                                out.push(
                                    char::from_u32(code).unwrap_or(char::REPLACEMENT_CHARACTER),
                                );
                            }
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

#[inline]
fn parse_hex4_bytes(bytes: &[u8]) -> Option<u32> {
    if bytes.len() != 4 {
        return None;
    }
    let mut v = 0u32;
    for &b in bytes {
        let d = match b {
            b'0'..=b'9' => (b - b'0') as u32,
            b'a'..=b'f' => (b - b'a' + 10) as u32,
            b'A'..=b'F' => (b - b'A' + 10) as u32,
            _ => return None,
        };
        v = (v << 4) | d;
    }
    Some(v)
}
