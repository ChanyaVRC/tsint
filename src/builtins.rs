//! 組み込みオブジェクト・グローバル関数群。
//!
//! Noiria コーパス（`runtime/kag/parser.ts` / `runtime/kag/vars.ts`）が
//! 使う分＋要求仕様に挙げられた分だけを実装する。汎用 JS エンジンを
//! 目指すものではないため、対応外の呼び出しは「明確なエラー」を返す
//! （黙って undefined を返したりしない）方針を優先する。

use std::borrow::Cow;
use std::cell::RefCell;
use std::rc::Rc;

use crate::regex::RegexScratch;
use crate::value::*;
use crate::vm::{
    array_values, ascii_char_rc, is_callable, number_to_js_string_into, to_number, to_string_basic,
    KindTag, Vm,
};

thread_local! {
    static REGEX_SCRATCH: RefCell<RegexScratch> = RefCell::new(RegexScratch::default());
    static CHARS_SCRATCH: RefCell<Vec<char>> = const { RefCell::new(Vec::new()) };
    static KEY_LAST_INDEX: Rc<str> = Rc::from("lastIndex");
    static KEY_INDEX: Rc<str> = Rc::from("index");
    static KEY_INPUT: Rc<str> = Rc::from("input");
    static KEY_GROUPS: Rc<str> = Rc::from("groups");
}

#[inline]
fn value_as_cow_str(v: &Value) -> Cow<'_, str> {
    match v {
        Value::Str(s) => Cow::Borrowed(s),
        other => Cow::Owned(to_string_basic(other)),
    }
}

fn with_regex_scratch<R>(f: impl FnOnce(&mut RegexScratch) -> R) -> R {
    REGEX_SCRATCH.with(|cell| {
        if let Ok(mut guard) = cell.try_borrow_mut() {
            f(&mut guard)
        } else {
            let mut local = RegexScratch::default();
            f(&mut local)
        }
    })
}

fn with_chars_buf<R>(s: &str, f: impl FnOnce(&[char]) -> R) -> R {
    CHARS_SCRATCH.with(|cell| {
        if let Ok(mut buf) = cell.try_borrow_mut() {
            buf.clear();
            buf.extend(s.chars());
            f(&buf)
        } else {
            let local: Vec<char> = s.chars().collect();
            f(&local)
        }
    })
}

fn arg(args: &[Value], i: usize) -> Value {
    args.get(i).cloned().unwrap_or(Value::Undefined)
}

fn native(
    name: &str,
    arity: usize,
    f: impl Fn(&mut Vm, Value, &[Value]) -> Result<Value, Value> + 'static,
) -> Value {
    Value::native_fn(name, arity, Rc::new(f))
}

pub fn install(vm: &mut Vm) {
    install_console(vm);
    install_object(vm);
    install_math(vm);
    install_json(vm);
    install_number(vm);
    install_globals_misc(vm);
    install_map_set_ctors(vm);
    install_errors(vm);
    install_date(vm);
    install_promise(vm);
    install_regexp(vm);
}

// =====================================================================
// console
// =====================================================================

fn install_console(vm: &mut Vm) {
    let console = Value::new_plain_object();
    let log = native("log", 0, |vm, _this, args| {
        let parts: Vec<String> = args.iter().map(|a| vm.to_display_string(a)).collect();
        let line = parts.join(" ");
        (vm.on_log)(&line);
        Ok(Value::Undefined)
    });
    if let Value::Object(o) = &console {
        o.borrow_mut().props.set_value(Rc::from("log"), log.clone());
        o.borrow_mut()
            .props
            .set_value(Rc::from("error"), log.clone());
        o.borrow_mut()
            .props
            .set_value(Rc::from("warn"), log.clone());
        o.borrow_mut().props.set_value(Rc::from("info"), log);
    }
    vm.define_global("console", console);
}

// =====================================================================
// Object
// =====================================================================

fn install_object(vm: &mut Vm) {
    let object_global = Value::new_plain_object();
    let keys = native("keys", 1, |_vm, _this, args| {
        Ok(Value::new_array(own_enumerable_keys(&arg(args, 0))))
    });
    let values = native("values", 1, |_vm, _this, args| {
        Ok(Value::new_array(own_enumerable_values(&arg(args, 0))))
    });
    let entries = native("entries", 1, |_vm, _this, args| {
        let obj = arg(args, 0);
        let keys = own_enumerable_keys(&obj);
        let values = own_enumerable_values(&obj);
        let pairs = keys
            .into_iter()
            .zip(values)
            .map(|(k, v)| Value::new_array(vec![k, v]))
            .collect();
        Ok(Value::new_array(pairs))
    });
    let assign = native("assign", 2, |_vm, _this, args| {
        let target = arg(args, 0);
        let Value::Object(t) = &target else {
            return Ok(target);
        };
        for src in &args[1.min(args.len())..] {
            if let Value::Object(s) = src {
                let entries: Vec<(Rc<str>, Value)> = s
                    .borrow()
                    .props
                    .iter()
                    .filter_map(|(k, slot)| match slot {
                        PropSlot::Value(v) => Some((k.clone(), v.clone())),
                        PropSlot::Accessor { .. } => None,
                    })
                    .collect();
                for (k, v) in entries {
                    t.borrow_mut().props.set_value(k, v);
                }
            }
        }
        Ok(target)
    });
    if let Value::Object(o) = &object_global {
        let mut b = o.borrow_mut();
        b.props.set_value(Rc::from("keys"), keys);
        b.props.set_value(Rc::from("values"), values);
        b.props.set_value(Rc::from("entries"), entries);
        b.props.set_value(Rc::from("assign"), assign);
    }
    vm.define_global("Object", object_global);
}

fn own_enumerable_keys(v: &Value) -> Vec<Value> {
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

fn own_enumerable_values(v: &Value) -> Vec<Value> {
    match v {
        Value::Object(o) => {
            let b = o.borrow();
            match &b.kind {
                ObjKind::Array(items) => items.clone(),
                _ => b
                    .props
                    .iter()
                    .filter_map(|(_, s)| match s {
                        PropSlot::Value(v) => Some(v.clone()),
                        PropSlot::Accessor { .. } => None,
                    })
                    .collect(),
            }
        }
        _ => Vec::new(),
    }
}

// =====================================================================
// Math
// =====================================================================

fn install_math(vm: &mut Vm) {
    let math = Value::new_plain_object();
    macro_rules! unary {
        ($name:literal, $f:expr) => {
            native($name, 1, |_vm, _this, args| {
                Ok(Value::Number($f(to_number(&arg(args, 0)))))
            })
        };
    }
    let floor = unary!("floor", f64::floor);
    let ceil = unary!("ceil", f64::ceil);
    let round = unary!("round", |n: f64| (n + 0.5).floor());
    let abs = unary!("abs", f64::abs);
    let min = native("min", 2, |_vm, _this, args| {
        let mut m = f64::INFINITY;
        for a in args {
            let n = to_number(a);
            if n.is_nan() {
                return Ok(Value::Number(f64::NAN));
            }
            if n < m {
                m = n;
            }
        }
        Ok(Value::Number(m))
    });
    let max = native("max", 2, |_vm, _this, args| {
        let mut m = f64::NEG_INFINITY;
        for a in args {
            let n = to_number(a);
            if n.is_nan() {
                return Ok(Value::Number(f64::NAN));
            }
            if n > m {
                m = n;
            }
        }
        Ok(Value::Number(m))
    });
    let pow = native("pow", 2, |_vm, _this, args| {
        Ok(Value::Number(
            to_number(&arg(args, 0)).powf(to_number(&arg(args, 1))),
        ))
    });
    let random = native("random", 0, |_vm, _this, _args| Ok(Value::Number(0.0)));
    if let Value::Object(o) = &math {
        let mut b = o.borrow_mut();
        b.props.set_value(Rc::from("floor"), floor);
        b.props.set_value(Rc::from("ceil"), ceil);
        b.props.set_value(Rc::from("round"), round);
        b.props.set_value(Rc::from("abs"), abs);
        b.props.set_value(Rc::from("min"), min);
        b.props.set_value(Rc::from("max"), max);
        b.props.set_value(Rc::from("pow"), pow);
        b.props.set_value(Rc::from("random"), random);
        b.props
            .set_value(Rc::from("PI"), Value::Number(std::f64::consts::PI));
    }
    vm.define_global("Math", math);
}

// =====================================================================
// JSON（自作パーサ / シリアライザ）
// =====================================================================

fn install_json(vm: &mut Vm) {
    let json = Value::new_plain_object();
    let stringify = native("stringify", 1, |_vm, _this, args| {
        let indent = match args.get(2) {
            Some(Value::Number(n)) => n.max(0.0) as usize,
            _ => 0,
        };
        let target = args.first().unwrap_or(&Value::Undefined);
        let mut out = String::with_capacity(128);
        if json_stringify_into(target, indent, 0, &mut out) {
            Ok(Value::str(out))
        } else {
            Ok(Value::Undefined)
        }
    });
    let parse = native("parse", 1, |vm, _this, args| {
        let s = value_as_cow_str(args.first().unwrap_or(&Value::Undefined));
        json_parse(vm, &s).map_err(|e| vm.make_error("SyntaxError", format!("JSON.parse: {e}")))
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
            0..=0x1f => None, // control char handled below
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

struct JsonParser<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
}

fn json_parse(vm: &Vm, s: &str) -> Result<Value, String> {
    let mut p = JsonParser {
        src: s,
        bytes: s.as_bytes(),
        pos: 0,
    };
    p.skip_ws();
    let v = p.parse_value(vm)?;
    p.skip_ws();
    if p.pos != p.bytes.len() {
        return Err("末尾に余分な文字があります".to_string());
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
                // 非 ASCII 空白へのフォールバック
                let ch = self.src[self.pos..].chars().next().unwrap();
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
    fn expect(&mut self, c: u8) -> Result<(), String> {
        if self.peek() == Some(c) {
            self.pos += 1;
            Ok(())
        } else {
            Err(format!(
                "'{}' を期待しました（位置 {}）",
                c as char, self.pos
            ))
        }
    }
    fn parse_value(&mut self, vm: &Vm) -> Result<Value, String> {
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
            _ => Err(format!("値を期待しました（位置 {}）", self.pos)),
        }
    }
    fn expect_word(&mut self, w: &[u8]) -> Result<(), String> {
        if self.bytes[self.pos..].starts_with(w) {
            self.pos += w.len();
            Ok(())
        } else {
            Err(format!(
                "'{}' を期待しました（位置 {}）",
                std::str::from_utf8(w).unwrap_or(""),
                self.pos
            ))
        }
    }
    fn parse_object(&mut self, vm: &Vm) -> Result<Value, String> {
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
                _ => return Err(format!("',' か '}}' を期待しました（位置 {}）", self.pos)),
            }
        }
        Ok(vm.alloc_obj(obj_data))
    }
    fn parse_array(&mut self, vm: &Vm) -> Result<Value, String> {
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
                _ => return Err(format!("',' か ']' を期待しました（位置 {}）", self.pos)),
            }
        }
        Ok(vm.alloc_array(items))
    }
    fn parse_string_rc(&mut self) -> Result<Rc<str>, String> {
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
                None => return Err("文字列が閉じられていません".to_string()),
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
                                return Err("不正な \\u エスケープです".to_string());
                            }
                            let hex = &self.src[self.pos + 1..self.pos + 5];
                            let code = u32::from_str_radix(hex, 16)
                                .map_err(|_| "不正な \\u エスケープです".to_string())?;
                            if let Some(c) = char::from_u32(code) {
                                out.push(c);
                            }
                            self.pos += 4;
                        }
                        _ => return Err("不正なエスケープです".to_string()),
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
    fn parse_number(&mut self) -> Result<Value, String> {
        let start = self.pos;
        let neg = if self.peek() == Some(b'-') {
            self.pos += 1;
            true
        } else {
            false
        };
        let int_start = self.pos;
        let mut int_acc: i64 = 0;
        let mut int_digits = 0usize;
        while let Some(&b) = self.bytes.get(self.pos) {
            if b.is_ascii_digit() {
                if int_digits < 15 {
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
        if !is_float && int_digits > 0 && int_digits <= 15 {
            let n = if neg {
                -(int_acc as f64)
            } else {
                int_acc as f64
            };
            return Ok(Value::Number(n));
        }
        let _ = int_start;
        self.src[start..self.pos]
            .parse::<f64>()
            .map(Value::Number)
            .map_err(|_| "不正な数値です".to_string())
    }
}

// =====================================================================
// Number / parseInt / parseFloat
// =====================================================================

fn install_number(vm: &mut Vm) {
    // `Number` はグローバル関数（`Number(x)`）でありつつ静的プロパティ
    // （`Number.isFinite` 等）も持つ。ネイティブ関数オブジェクトも普通の
    // オブジェクトとして props を持てるので、両方を同じ値に載せる。
    let number = native("Number", 1, |_vm, _this, args| {
        Ok(Value::Number(to_number(&arg(args, 0))))
    });
    let is_finite = native("isFinite", 1, |_vm, _this, args| {
        Ok(Value::Bool(
            matches!(arg(args, 0), Value::Number(n) if n.is_finite()),
        ))
    });
    if let Value::Object(o) = &number {
        o.borrow_mut()
            .props
            .set_value(Rc::from("isFinite"), is_finite);
        o.borrow_mut()
            .props
            .set_value(Rc::from("NaN"), Value::Number(f64::NAN));
        o.borrow_mut()
            .props
            .set_value(Rc::from("POSITIVE_INFINITY"), Value::Number(f64::INFINITY));
        o.borrow_mut().props.set_value(
            Rc::from("NEGATIVE_INFINITY"),
            Value::Number(f64::NEG_INFINITY),
        );
    }
    vm.define_global("Number", number);
    vm.define_global("NaN", Value::Number(f64::NAN));
    vm.define_global("Infinity", Value::Number(f64::INFINITY));
    vm.define_global("undefined", Value::Undefined);
    // `globalThis`: 素のプレーンオブジェクトとして用意する（グローバル変数
    // 表そのものとの双方向同期はしない — コーパスは `(globalThis as any).Deno`
    // のようにホストが注入した値を読み書きする用途にしか使わないので、
    // それで十分）。
    vm.define_global("globalThis", Value::new_plain_object());
}

fn install_globals_misc(vm: &mut Vm) {
    vm.register_native(
        "parseInt",
        2,
        Rc::new(|vm, _this, args| {
            let s = vm.to_display_string(&arg(args, 0));
            let radix = match arg(args, 1) {
                Value::Number(n) if n as u32 != 0 => n as u32,
                _ => 10,
            };
            let t = s.trim();
            let (neg, t) = if let Some(r) = t.strip_prefix('-') {
                (true, r)
            } else if let Some(r) = t.strip_prefix('+') {
                (false, r)
            } else {
                (false, t)
            };
            let t = if radix == 16 {
                t.strip_prefix("0x")
                    .or_else(|| t.strip_prefix("0X"))
                    .unwrap_or(t)
            } else {
                t
            };
            let end = t.chars().take_while(|c| c.is_digit(radix)).count();
            if end == 0 {
                return Ok(Value::Number(f64::NAN));
            }
            let n = i64::from_str_radix(&t[..end], radix).unwrap_or(0) as f64;
            Ok(Value::Number(if neg { -n } else { n }))
        }),
    );
    vm.register_native(
        "parseFloat",
        1,
        Rc::new(|vm, _this, args| {
            let s = vm.to_display_string(&arg(args, 0));
            let t = s.trim();
            let mut end = 0usize;
            let bytes: Vec<char> = t.chars().collect();
            if end < bytes.len() && (bytes[end] == '-' || bytes[end] == '+') {
                end += 1;
            }
            let mut seen_dot = false;
            let mut seen_digit = false;
            while end < bytes.len()
                && (bytes[end].is_ascii_digit() || (!seen_dot && bytes[end] == '.'))
            {
                if bytes[end] == '.' {
                    seen_dot = true;
                } else {
                    seen_digit = true;
                }
                end += 1;
            }
            if !seen_digit {
                return Ok(Value::Number(f64::NAN));
            }
            let n: String = bytes[..end].iter().collect();
            Ok(Value::Number(n.parse::<f64>().unwrap_or(f64::NAN)))
        }),
    );
    vm.register_native(
        "String",
        1,
        Rc::new(|vm, _this, args| Ok(Value::str(vm.to_display_string(&arg(args, 0))))),
    );
    vm.register_native(
        "Boolean",
        1,
        Rc::new(|_vm, _this, args| Ok(Value::Bool(arg(args, 0).to_bool()))),
    );
    vm.register_native(
        "isNaN",
        1,
        Rc::new(|_vm, _this, args| Ok(Value::Bool(to_number(&arg(args, 0)).is_nan()))),
    );
    let array_global = native("Array", 0, |_vm, _this, args| {
        if args.len() == 1 {
            if let Value::Number(n) = args[0] {
                return Ok(Value::new_array(vec![Value::Undefined; n as usize]));
            }
        }
        Ok(Value::new_array(args.to_vec()))
    });
    let array_is_array = native("isArray", 1, |_vm, _this, args| {
        Ok(Value::Bool(
            matches!(arg(args, 0), Value::Object(o) if matches!(o.borrow().kind, ObjKind::Array(_))),
        ))
    });
    if let Value::Object(o) = &array_global {
        o.borrow_mut()
            .props
            .set_value(Rc::from("isArray"), array_is_array);
    }
    vm.define_global("Array", array_global);

    // `new Function(...args, body)`: 実行時に自前パーサでコンパイルする。
    // スコープはグローバルのみを参照する（`compile_dynamic_function` 参照）。
    vm.register_native(
        "Function",
        0,
        Rc::new(|vm, _this, args| {
            let mut strs: Vec<String> = args.iter().map(|v| vm.to_display_string(v)).collect();
            let body = if strs.is_empty() {
                String::new()
            } else {
                strs.pop().unwrap()
            };
            match crate::compiler::compile_dynamic_function(&strs, &body) {
                Ok(chunk) => {
                    let fo = Rc::new(FunctionObj {
                        name: Rc::from("anonymous"),
                        chunk,
                        upvalues: Rc::new(Vec::new()),
                        is_arrow: false,
                        is_async: false,
                    });
                    Ok(Value::Object(Rc::new(RefCell::new(ObjData::new(
                        ObjKind::Function(fo),
                    )))))
                }
                Err(e) => Err(vm.make_error("SyntaxError", e.to_string())),
            }
        }),
    );
}

fn install_errors(vm: &mut Vm) {
    for name in [
        "Error",
        "TypeError",
        "RangeError",
        "SyntaxError",
        "ReferenceError",
    ] {
        let nm = name.to_string();
        vm.register_native(
            name,
            1,
            Rc::new(move |vm, _this, args| {
                let msg = if args.is_empty() {
                    String::new()
                } else {
                    vm.to_display_string(&args[0])
                };
                Ok(vm.make_error(&nm, msg))
            }),
        );
    }
}

// =====================================================================
// Map / Set コンストラクタ
// =====================================================================

fn install_map_set_ctors(vm: &mut Vm) {
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
            Ok(Value::Object(Rc::new(RefCell::new(ObjData::new(
                ObjKind::Map(data),
            )))))
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
            Ok(Value::Object(Rc::new(RefCell::new(ObjData::new(
                ObjKind::Set(data),
            )))))
        }),
    );
}

// =====================================================================
// Date（現在時刻はホスト注入。VM 自体は決定的に保つ）
// =====================================================================

fn install_date(vm: &mut Vm) {
    let proto: ObjRef = Rc::new(RefCell::new(ObjData::new(ObjKind::Plain)));
    let get_time = native("getTime", 0, |_vm, this, _args| {
        if let Value::Object(o) = &this {
            if let Some(PropSlot::Value(v)) = o.borrow().props.get("%time") {
                return Ok(v.clone());
            }
        }
        Ok(Value::Number(0.0))
    });
    let to_locale_string = native("toLocaleString", 0, |_vm, this, _args| {
        let t = if let Value::Object(o) = &this {
            match o.borrow().props.get("%time") {
                Some(PropSlot::Value(Value::Number(n))) => *n,
                _ => 0.0,
            }
        } else {
            0.0
        };
        Ok(Value::str(format_epoch_millis(t)))
    });
    proto
        .borrow_mut()
        .props
        .set_value(Rc::from("getTime"), get_time);
    proto
        .borrow_mut()
        .props
        .set_value(Rc::from("toLocaleString"), to_locale_string.clone());
    proto
        .borrow_mut()
        .props
        .set_value(Rc::from("toString"), to_locale_string);

    let proto_for_ctor = proto.clone();
    vm.register_native(
        "Date",
        0,
        Rc::new(move |vm, _this, args| {
            let t = if let Some(Value::Number(n)) = args.first() {
                *n
            } else if let Some(now) = vm.now_fn.clone() {
                now()
            } else {
                0.0
            };
            let inst = ObjData {
                kind: ObjKind::Plain,
                props: PropMap::new(),
                proto: Some(proto_for_ctor.clone()),
            };
            let rc = Rc::new(RefCell::new(inst));
            rc.borrow_mut()
                .props
                .set_value(Rc::from("%time"), Value::Number(t));
            Ok(Value::Object(rc))
        }),
    );
}

/// エポックミリ秒を `YYYY-MM-DD HH:MM:SS` へ変換する（グレゴリオ暦・UTC、
/// うるう秒は無視）。Howard Hinnant の civil_from_days アルゴリズムを使用。
fn format_epoch_millis(ms: f64) -> String {
    let total_seconds = (ms / 1000.0).floor() as i64;
    let days = total_seconds.div_euclid(86400);
    let secs_of_day = total_seconds.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    let hh = secs_of_day / 3600;
    let mm = (secs_of_day % 3600) / 60;
    let ss = secs_of_day % 60;
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}")
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// =====================================================================
// Promise（T3）
// =====================================================================

fn install_promise(vm: &mut Vm) {
    let ctor = native("Promise", 1, |vm, _this, args| {
        let executor = arg(args, 0);
        let (promise_val, promise_obj) = vm.new_pending_promise();
        let p_res = Rc::downgrade(&promise_obj);
        let resolve_fn = vm.alloc_native_with_captures(
            "",
            1,
            vec![promise_obj.clone()],
            Rc::new(move |vm, _this, args| {
                if let Some(p) = p_res.upgrade() {
                    vm.resolve_promise(p, arg(args, 0));
                }
                Ok(Value::Undefined)
            }),
        );
        let p_rej = Rc::downgrade(&promise_obj);
        let reject_fn = vm.alloc_native_with_captures(
            "",
            1,
            vec![promise_obj.clone()],
            Rc::new(move |vm, _this, args| {
                if let Some(p) = p_rej.upgrade() {
                    vm.settle_promise(p, Err(arg(args, 0)));
                }
                Ok(Value::Undefined)
            }),
        );
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

/// `Promise.race` 用: 自分（引数の値/例外）で `target` を確定させ、戻り値は
/// 捨てる（呼び出し元は使い捨ての `result_promise` にぶら下げる）。
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

fn promise_method(name: &str) -> Option<Value> {
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

fn call_promise_method(
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
            // finally のコールバックは引数無しで呼ばれ、戻り値は無視される
            // （投げた場合のみそれが新しい結果を上書きする）。元の値/例外は
            // そのまま透過する（`then`/`catch` と違い finally は値を変えない）。
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

// =====================================================================
// Array.prototype / String.prototype / Map.prototype / Set.prototype / RegExp
// =====================================================================

pub fn method_for(tag: KindTag, name: &str) -> Option<Value> {
    match tag {
        KindTag::Array => array_method(name),
        KindTag::Str => string_method(name),
        KindTag::Map => map_method(name),
        KindTag::Set => set_method(name),
        KindTag::Regex => regex_method(name),
        KindTag::Promise => promise_method(name),
    }
}

/// `CallMethod` 命令用の直接ディスパッチ。`NativeObj` を確保せず即座に実行する。
pub(crate) fn call_builtin_method(
    vm: &mut Vm,
    tag: KindTag,
    this: Value,
    name: &str,
    args: &[Value],
) -> Option<Result<Value, Value>> {
    match tag {
        KindTag::Array => call_array_method(vm, this, name, args),
        KindTag::Str => call_string_method(vm, this, name, args),
        KindTag::Map => call_map_method(vm, this, name, args),
        KindTag::Set => call_set_method(vm, this, name, args),
        KindTag::Regex => call_regex_method(vm, this, name, args),
        KindTag::Promise => call_promise_method(vm, this, name, args),
    }
}

fn with_array<T>(this: &Value, f: impl FnOnce(&mut Vec<Value>) -> T) -> Option<T> {
    if let Value::Object(o) = this {
        if let ObjKind::Array(items) = &mut o.borrow_mut().kind {
            return Some(f(items));
        }
    }
    None
}

fn array_method(name: &str) -> Option<Value> {
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

fn call_array_method(
    vm: &mut Vm,
    this: Value,
    name: &str,
    args: &[Value],
) -> Option<Result<Value, Value>> {
    Some(match name {
        "push" => {
            let len = with_array(&this, |items| {
                items.extend_from_slice(args);
                items.len()
            })
            .unwrap_or(0);
            Ok(Value::Number(len as f64))
        }
        "pop" => Ok(with_array(&this, |items| items.pop())
            .flatten()
            .unwrap_or(Value::Undefined)),
        "shift" => Ok(with_array(&this, |items| {
            if items.is_empty() {
                None
            } else {
                Some(items.remove(0))
            }
        })
        .flatten()
        .unwrap_or(Value::Undefined)),
        "unshift" => {
            let len = with_array(&this, |items| {
                for (i, a) in args.iter().enumerate() {
                    items.insert(i, a.clone());
                }
                items.len()
            })
            .unwrap_or(0);
            Ok(Value::Number(len as f64))
        }
        "slice" => {
            let out = if let Value::Object(o) = &this {
                if let ObjKind::Array(items) = &o.borrow().kind {
                    let len = items.len() as i64;
                    let start = norm_index(args.first(), 0, len);
                    let end = norm_index(args.get(1), len, len);
                    if start < end {
                        items[start as usize..end as usize].to_vec()
                    } else {
                        Vec::new()
                    }
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            };
            Ok(vm.alloc_array(out))
        }
        "splice" => {
            let removed = with_array(&this, |items| {
                let len = items.len() as i64;
                let start = norm_index(args.first(), 0, len).clamp(0, len) as usize;
                let del_count = match args.get(1) {
                    Some(Value::Number(n)) => (*n as i64).clamp(0, len - start as i64) as usize,
                    _ => items.len() - start,
                };
                let tail: Vec<Value> = items.split_off(start.min(items.len()));
                let (removed, mut rest) = {
                    let mut t = tail;
                    let removed: Vec<Value> = t.drain(0..del_count.min(t.len())).collect();
                    (removed, t)
                };
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
                Some(Value::Undefined) | None => std::borrow::Cow::Borrowed(","),
                Some(Value::Str(s)) => std::borrow::Cow::Borrowed(&**s),
                Some(v) => std::borrow::Cow::Owned(vm.to_display_string(v)),
            };
            let mut out = String::new();
            if let Value::Object(o) = &this {
                if let ObjKind::Array(items) = &o.borrow().kind {
                    for (i, it) in items.iter().enumerate() {
                        if i > 0 {
                            out.push_str(&sep);
                        }
                        if !matches!(it, Value::Undefined | Value::Null | Value::Uninit) {
                            crate::vm::to_string_into(it, &mut out);
                        }
                    }
                }
            }
            Ok(Value::str(out))
        }
        "indexOf" => {
            let target = args.first().unwrap_or(&Value::Undefined);
            let idx = if let Value::Object(o) = &this {
                if let ObjKind::Array(items) = &o.borrow().kind {
                    items
                        .iter()
                        .position(|v| crate::value::strict_eq(v, target))
                } else {
                    None
                }
            } else {
                None
            };
            Ok(Value::Number(idx.map(|i| i as f64).unwrap_or(-1.0)))
        }
        "includes" => {
            let target = args.first().unwrap_or(&Value::Undefined);
            let found = if let Value::Object(o) = &this {
                if let ObjKind::Array(items) = &o.borrow().kind {
                    items.iter().any(|v| same_value_zero(v, target))
                } else {
                    false
                }
            } else {
                false
            };
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

fn norm_index(v: Option<&Value>, default: i64, len: i64) -> i64 {
    match v {
        None | Some(Value::Undefined) => default,
        Some(x) => {
            let n = to_number(x);
            let n = if n.is_nan() { 0i64 } else { n as i64 };
            if n < 0 {
                (len + n).max(0)
            } else {
                n.min(len)
            }
        }
    }
}

// ---------------- String ----------------

pub fn string_get_prop(vm: &mut Vm, s: &Rc<str>, name: &str) -> Result<Value, Value> {
    if name == "length" {
        let len = if s.is_ascii() {
            s.len()
        } else {
            s.chars().count()
        };
        return Ok(Value::Number(len as f64));
    }
    if let Ok(idx) = name.parse::<usize>() {
        if s.is_ascii() {
            return Ok(s
                .as_bytes()
                .get(idx)
                .map(|&b| Value::Str(ascii_char_rc(b)))
                .unwrap_or(Value::Undefined));
        }
        return Ok(s
            .chars()
            .nth(idx)
            .map(|c| Value::str(c.to_string()))
            .unwrap_or(Value::Undefined));
    }
    let _ = vm;
    Ok(string_method(name).unwrap_or(Value::Undefined))
}

fn string_method(name: &str) -> Option<Value> {
    let (static_name, arity): (&'static str, usize) = match name {
        "slice" => ("slice", 2),
        "split" => ("split", 2),
        "trim" => ("trim", 0),
        "startsWith" => ("startsWith", 1),
        "endsWith" => ("endsWith", 1),
        "includes" => ("includes", 1),
        "indexOf" => ("indexOf", 1),
        "replace" => ("replace", 2),
        "replaceAll" => ("replaceAll", 2),
        "match" => ("match", 1),
        "search" => ("search", 1),
        "toLowerCase" => ("toLowerCase", 0),
        "toUpperCase" => ("toUpperCase", 0),
        "charCodeAt" => ("charCodeAt", 1),
        "codePointAt" => ("codePointAt", 1),
        "repeat" => ("repeat", 1),
        "padStart" => ("padStart", 2),
        _ => return None,
    };
    Some(native(static_name, arity, move |vm, this, args| {
        call_string_method(vm, this, static_name, args).unwrap()
    }))
}

fn call_string_method(
    vm: &mut Vm,
    this: Value,
    name: &str,
    args: &[Value],
) -> Option<Result<Value, Value>> {
    Some(match name {
        "slice" => {
            let s = value_as_cow_str(&this);
            if s.is_ascii() {
                let len = s.len() as i64;
                let start = norm_index(args.first(), 0, len) as usize;
                let end = norm_index(args.get(1), len, len) as usize;
                if start < end {
                    Ok(Value::str(&s[start..end]))
                } else {
                    Ok(Value::str(""))
                }
            } else {
                let char_len = s.chars().count() as i64;
                let start = norm_index(args.first(), 0, char_len) as usize;
                let end = norm_index(args.get(1), char_len, char_len) as usize;
                if start >= end {
                    Ok(Value::str(""))
                } else {
                    let mut byte_start = s.len();
                    let mut byte_end = s.len();
                    for (ci, (bi, _)) in s.char_indices().enumerate() {
                        if ci == start {
                            byte_start = bi;
                        }
                        if ci == end {
                            byte_end = bi;
                            break;
                        }
                    }
                    Ok(Value::str(&s[byte_start..byte_end]))
                }
            }
        }
        "split" => (|| {
            let s = value_as_cow_str(&this);
            let limit = match args.get(1) {
                None | Some(Value::Undefined) => usize::MAX,
                Some(v) => {
                    let n = to_number(v);
                    if n.is_nan() {
                        usize::MAX
                    } else {
                        n.max(0.0) as usize
                    }
                }
            };
            match args.first() {
                None | Some(Value::Undefined) => {
                    let mut out = if limit == 0 {
                        Vec::new()
                    } else if let Value::Str(rc) = &this {
                        vec![Value::Str(rc.clone())]
                    } else {
                        vec![Value::str(s.as_ref())]
                    };
                    out.truncate(limit);
                    Ok(vm.alloc_array(out))
                }
                Some(Value::Object(o)) if matches!(o.borrow().kind, ObjKind::Regex(_)) => {
                    let re = match &o.borrow().kind {
                        ObjKind::Regex(r) => r.clone(),
                        _ => unreachable!(),
                    };
                    let items = regex_split(vm, &re, &s, limit)?;
                    Ok(vm.alloc_array(items))
                }
                Some(sep) => {
                    let sep_s = value_as_cow_str(sep);
                    let out: Vec<Value> = if sep_s.is_empty() {
                        if s.is_ascii() {
                            s.as_bytes()
                                .iter()
                                .take(limit)
                                .map(|&b| Value::Str(ascii_char_rc(b)))
                                .collect()
                        } else {
                            s.chars()
                                .take(limit)
                                .map(|c| Value::str(c.to_string()))
                                .collect()
                        }
                    } else {
                        s.split(sep_s.as_ref())
                            .take(limit)
                            .map(Value::str)
                            .collect()
                    };
                    Ok(vm.alloc_array(out))
                }
            }
        })(),
        "trim" => {
            if let Value::Str(rc) = &this {
                let t = rc.trim();
                if t.len() == rc.len() {
                    Ok(Value::Str(rc.clone()))
                } else {
                    Ok(Value::str(t))
                }
            } else {
                let s = to_string_basic(&this);
                Ok(Value::str(s.trim()))
            }
        }
        "startsWith" => {
            let s = value_as_cow_str(&this);
            let p = value_as_cow_str(args.first().unwrap_or(&Value::Undefined));
            Ok(Value::Bool(s.starts_with(p.as_ref())))
        }
        "endsWith" => {
            let s = value_as_cow_str(&this);
            let p = value_as_cow_str(args.first().unwrap_or(&Value::Undefined));
            Ok(Value::Bool(s.ends_with(p.as_ref())))
        }
        "includes" => {
            let s = value_as_cow_str(&this);
            let p = value_as_cow_str(args.first().unwrap_or(&Value::Undefined));
            Ok(Value::Bool(s.contains(p.as_ref())))
        }
        "indexOf" => {
            let s = value_as_cow_str(&this);
            let p = value_as_cow_str(args.first().unwrap_or(&Value::Undefined));
            if p.is_empty() {
                Ok(Value::Number(0.0))
            } else if let Some(byte_idx) = s.find(p.as_ref()) {
                let char_idx = if s.is_ascii() {
                    byte_idx
                } else {
                    s[..byte_idx].chars().count()
                };
                Ok(Value::Number(char_idx as f64))
            } else {
                Ok(Value::Number(-1.0))
            }
        }
        "replace" => {
            let s = value_as_cow_str(&this);
            let pattern = args.first().unwrap_or(&Value::Undefined);
            let replacement = args.get(1).unwrap_or(&Value::Undefined);
            string_replace(vm, &s, pattern, replacement, false)
        }
        "replaceAll" => {
            let s = value_as_cow_str(&this);
            let pattern = args.first().unwrap_or(&Value::Undefined);
            let replacement = args.get(1).unwrap_or(&Value::Undefined);
            if let Value::Object(o) = pattern {
                if let ObjKind::Regex(re) = &o.borrow().kind {
                    if !re.global {
                        return Some(Err(vm.type_error(
                            "String.prototype.replaceAll に渡す正規表現には g フラグが必要です",
                        )));
                    }
                }
            }
            string_replace(vm, &s, pattern, replacement, true)
        }
        "match" => (|| {
            let s = value_as_cow_str(&this);
            let re = coerce_to_regex(vm, args.first().unwrap_or(&Value::Undefined))?;
            with_chars_buf(&s, |chars| {
                if re.global {
                    let mut out = vec![];
                    let mut from = 0usize;
                    with_regex_scratch(|scratch| -> Result<(), Value> {
                        loop {
                            if from > chars.len() {
                                break;
                            }
                            match re
                                .search_with_scratch(chars, from, scratch)
                                .map_err(|e| regex_err(vm, e))?
                            {
                                Some(mat) => {
                                    let mut piece = String::with_capacity(mat.end - mat.start);
                                    piece.extend(&chars[mat.start..mat.end]);
                                    out.push(Value::str(piece));
                                    from = if mat.end == mat.start {
                                        mat.end + 1
                                    } else {
                                        mat.end
                                    };
                                }
                                None => break,
                            }
                        }
                        Ok(())
                    })?;
                    if out.is_empty() {
                        Ok(Value::Null)
                    } else {
                        Ok(vm.alloc_array(out))
                    }
                } else {
                    match with_regex_scratch(|scratch| re.search_with_scratch(chars, 0, scratch))
                        .map_err(|e| regex_err(vm, e))?
                    {
                        Some(mat) => Ok(build_match_array(vm, chars, &mat, &s)),
                        None => Ok(Value::Null),
                    }
                }
            })
        })(),
        "search" => (|| {
            let s = value_as_cow_str(&this);
            let re = coerce_to_regex(vm, args.first().unwrap_or(&Value::Undefined))?;
            with_chars_buf(&s, |chars| {
                match with_regex_scratch(|scratch| re.search_with_scratch(chars, 0, scratch))
                    .map_err(|e| regex_err(vm, e))?
                {
                    Some(mat) => Ok(Value::Number(mat.start as f64)),
                    None => Ok(Value::Number(-1.0)),
                }
            })
        })(),
        "toLowerCase" => Ok(Value::str(value_as_cow_str(&this).to_lowercase())),
        "toUpperCase" => Ok(Value::str(value_as_cow_str(&this).to_uppercase())),
        "charCodeAt" => {
            let s = value_as_cow_str(&this);
            let idx = to_number(args.first().unwrap_or(&Value::Undefined)) as usize;
            let code = if s.is_ascii() {
                s.as_bytes().get(idx).map(|&b| b as f64)
            } else {
                s.chars().nth(idx).map(|c| c as u32 as f64)
            };
            Ok(Value::Number(code.unwrap_or(f64::NAN)))
        }
        "codePointAt" => {
            let s = value_as_cow_str(&this);
            let idx = to_number(args.first().unwrap_or(&Value::Undefined)) as usize;
            let code = if s.is_ascii() {
                s.as_bytes().get(idx).map(|&b| Value::Number(b as f64))
            } else {
                s.chars().nth(idx).map(|c| Value::Number(c as u32 as f64))
            };
            Ok(code.unwrap_or(Value::Undefined))
        }
        "repeat" => {
            let s = value_as_cow_str(&this);
            let n = to_number(args.first().unwrap_or(&Value::Undefined));
            if n < 0.0 || !n.is_finite() {
                Err(vm.range_error("repeat count は非負の有限数である必要があります"))
            } else {
                Ok(Value::str(s.repeat(n as usize)))
            }
        }
        "padStart" => {
            let s = value_as_cow_str(&this);
            let target_len = to_number(args.first().unwrap_or(&Value::Undefined)) as usize;
            let pad = match args.get(1) {
                Some(v) => value_as_cow_str(v),
                None => Cow::Borrowed(" "),
            };
            let cur_len = if s.is_ascii() {
                s.len()
            } else {
                s.chars().count()
            };
            if cur_len >= target_len || pad.is_empty() {
                if let Value::Str(rc) = &this {
                    Ok(Value::Str(rc.clone()))
                } else {
                    Ok(Value::str(s.as_ref()))
                }
            } else {
                let need = target_len - cur_len;
                let mut out = String::with_capacity(need + s.len());
                let pad_chars: Vec<char> = pad.chars().collect();
                let mut written = 0usize;
                while written < need {
                    let take = (need - written).min(pad_chars.len());
                    out.extend(&pad_chars[..take]);
                    written += take;
                }
                out.push_str(&s);
                Ok(Value::str(out))
            }
        }
        _ => return None,
    })
}

// ---------------- Map ----------------

fn map_method(name: &str) -> Option<Value> {
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

fn call_map_method(
    vm: &mut Vm,
    this: Value,
    name: &str,
    args: &[Value],
) -> Option<Result<Value, Value>> {
    Some(match name {
        "get" => {
            let key = args.first().unwrap_or(&Value::Undefined);
            if let Value::Object(o) = &this {
                if let ObjKind::Map(m) = &o.borrow().kind {
                    if let Some(i) = m.find(key) {
                        return Some(Ok(m.entries[i].1.clone()));
                    }
                }
            }
            Ok(Value::Undefined)
        }
        "set" => {
            let key = arg(args, 0);
            let val = arg(args, 1);
            if let Value::Object(o) = &this {
                if let ObjKind::Map(m) = &mut o.borrow_mut().kind {
                    m.set(key, val);
                }
            }
            Ok(this)
        }
        "has" => {
            let key = args.first().unwrap_or(&Value::Undefined);
            if let Value::Object(o) = &this {
                if let ObjKind::Map(m) = &o.borrow().kind {
                    return Some(Ok(Value::Bool(m.find(key).is_some())));
                }
            }
            Ok(Value::Bool(false))
        }
        "delete" => {
            let key = args.first().unwrap_or(&Value::Undefined);
            if let Value::Object(o) = &this {
                if let ObjKind::Map(m) = &mut o.borrow_mut().kind {
                    return Some(Ok(Value::Bool(m.delete(key))));
                }
            }
            Ok(Value::Bool(false))
        }
        "keys" => {
            if let Value::Object(o) = &this {
                if let ObjKind::Map(m) = &o.borrow().kind {
                    let ks = m.entries.iter().map(|(k, _)| k.clone()).collect();
                    return Some(Ok(vm.alloc_array(ks)));
                }
            }
            Ok(vm.alloc_array(Vec::new()))
        }
        "values" => {
            if let Value::Object(o) = &this {
                if let ObjKind::Map(m) = &o.borrow().kind {
                    let vs = m.entries.iter().map(|(_, v)| v.clone()).collect();
                    return Some(Ok(vm.alloc_array(vs)));
                }
            }
            Ok(vm.alloc_array(Vec::new()))
        }
        "entries" => {
            if let Value::Object(o) = &this {
                if let ObjKind::Map(m) = &o.borrow().kind {
                    let pairs: Vec<Value> = m
                        .entries
                        .iter()
                        .map(|(k, v)| vm.alloc_array(vec![k.clone(), v.clone()]))
                        .collect();
                    return Some(Ok(vm.alloc_array(pairs)));
                }
            }
            Ok(vm.alloc_array(Vec::new()))
        }
        "forEach" => (|| {
            let cb = arg(args, 0);
            let entries = if let Value::Object(o) = &this {
                if let ObjKind::Map(m) = &o.borrow().kind {
                    m.entries.clone()
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            };
            for (k, v) in entries {
                vm.call_value(cb.clone(), Value::Undefined, &[v, k, this.clone()])?;
            }
            Ok(Value::Undefined)
        })(),
        "clear" => {
            if let Value::Object(o) = &this {
                if let ObjKind::Map(m) = &mut o.borrow_mut().kind {
                    m.clear();
                }
            }
            Ok(Value::Undefined)
        }
        _ => return None,
    })
}

// ---------------- Set ----------------

fn set_method(name: &str) -> Option<Value> {
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

fn call_set_method(
    vm: &mut Vm,
    this: Value,
    name: &str,
    args: &[Value],
) -> Option<Result<Value, Value>> {
    Some(match name {
        "add" => {
            let v = arg(args, 0);
            if let Value::Object(o) = &this {
                if let ObjKind::Set(s) = &mut o.borrow_mut().kind {
                    s.add(v);
                }
            }
            Ok(this)
        }
        "has" => {
            let v = args.first().unwrap_or(&Value::Undefined);
            if let Value::Object(o) = &this {
                if let ObjKind::Set(s) = &o.borrow().kind {
                    return Some(Ok(Value::Bool(s.find(v).is_some())));
                }
            }
            Ok(Value::Bool(false))
        }
        "delete" => {
            let v = args.first().unwrap_or(&Value::Undefined);
            if let Value::Object(o) = &this {
                if let ObjKind::Set(s) = &mut o.borrow_mut().kind {
                    return Some(Ok(Value::Bool(s.delete(v))));
                }
            }
            Ok(Value::Bool(false))
        }
        "keys" | "values" => {
            if let Value::Object(o) = &this {
                if let ObjKind::Set(s) = &o.borrow().kind {
                    return Some(Ok(vm.alloc_array(s.values.clone())));
                }
            }
            Ok(vm.alloc_array(Vec::new()))
        }
        "forEach" => (|| {
            let cb = arg(args, 0);
            let values = if let Value::Object(o) = &this {
                if let ObjKind::Set(s) = &o.borrow().kind {
                    s.values.clone()
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            };
            for v in values {
                vm.call_value(cb.clone(), Value::Undefined, &[v.clone(), v, this.clone()])?;
            }
            Ok(Value::Undefined)
        })(),
        "clear" => {
            if let Value::Object(o) = &this {
                if let ObjKind::Set(s) = &mut o.borrow_mut().kind {
                    s.clear();
                }
            }
            Ok(Value::Undefined)
        }
        _ => return None,
    })
}

// ---------------- RegExp（T4 本実装） ----------------

/// `new RegExp(...)` / `RegExp(...)` の実装。文字列（と省略可能な flags）、
/// または既存の RegExp オブジェクト（flags 省略時はそれをそのまま踏襲、
/// 指定時は上書き）を受け取る。
fn install_regexp(vm: &mut Vm) {
    vm.register_native(
        "RegExp",
        2,
        Rc::new(|vm, _this, args| {
            let (pattern, default_flags) = match args.first() {
                Some(Value::Object(o)) if matches!(o.borrow().kind, ObjKind::Regex(_)) => {
                    let re = match &o.borrow().kind {
                        ObjKind::Regex(r) => r.clone(),
                        _ => unreachable!(),
                    };
                    (re.source.clone(), re.flags.clone())
                }
                Some(v) => (vm.to_display_string(v), String::new()),
                None => (String::new(), String::new()),
            };
            let flags = match args.get(1) {
                None | Some(Value::Undefined) => default_flags,
                Some(v) => vm.to_display_string(v),
            };
            match crate::regex::Regex::compile(&pattern, &flags) {
                Ok(re) => Ok(crate::vm::new_regex_value(Rc::new(re))),
                Err(e) => Err(vm.make_error("SyntaxError", format!("不正な正規表現です: {}", e.0))),
            }
        }),
    );
}

/// 文字列引数またはコンパイル済み正規表現を `Rc<Regex>` に統一する
/// （`String.prototype.match`/`search` は文字列を渡された場合、それを
/// パターンとして（フラグ無しで）コンパイルする、という JS 仕様に対応）。
fn coerce_to_regex(vm: &mut Vm, v: &Value) -> Result<Rc<crate::regex::Regex>, Value> {
    if let Value::Object(o) = v {
        if let ObjKind::Regex(re) = &o.borrow().kind {
            return Ok(re.clone());
        }
    }
    let pattern = vm.to_display_string(v);
    crate::regex::Regex::compile(&pattern, "")
        .map(Rc::new)
        .map_err(|e| vm.make_error("SyntaxError", format!("不正な正規表現です: {}", e.0)))
}

fn regex_err(vm: &Vm, e: crate::regex::RegexError) -> Value {
    vm.range_error(format!("正規表現の実行中にエラーが発生しました: {}", e.0))
}

fn regex_last_index(o: &ObjRef) -> usize {
    match o.borrow().props.get("lastIndex") {
        Some(PropSlot::Value(v)) => {
            let n = to_number(v);
            if n.is_finite() && n >= 0.0 {
                n as usize
            } else {
                0
            }
        }
        _ => 0,
    }
}

fn set_last_index(o: &ObjRef, n: usize) {
    let key = KEY_LAST_INDEX.with(|k| k.clone());
    o.borrow_mut().props.set_value(key, Value::Number(n as f64));
}

/// `RegExp.prototype.exec` の戻り値の配列（`index`/`input`/`groups` プロパティ
/// 付き）を組み立てる。
fn build_match_array(
    vm: &Vm,
    chars: &[char],
    mat: &crate::regex::RegexMatch,
    input: &str,
) -> Value {
    let mut whole = String::with_capacity(mat.end - mat.start);
    whole.extend(&chars[mat.start..mat.end]);
    let mut items = Vec::with_capacity(1 + mat.groups.len());
    items.push(Value::str(whole));
    for g in &mat.groups {
        items.push(match g {
            Some((a, b)) => {
                let mut gs = String::with_capacity(*b - *a);
                gs.extend(&chars[*a..*b]);
                Value::str(gs)
            }
            None => Value::Undefined,
        });
    }
    let mut obj = ObjData::new(ObjKind::Array(items));
    obj.props.set_value(
        KEY_INDEX.with(|k| k.clone()),
        Value::Number(mat.start as f64),
    );
    obj.props
        .set_value(KEY_INPUT.with(|k| k.clone()), Value::str(input));
    // 名前付きキャプチャグループは未対応のため常に undefined（実 JS の
    // 「名前付きグループが 1 つも無ければ undefined」という挙動と一致する）。
    obj.props
        .set_value(KEY_GROUPS.with(|k| k.clone()), Value::Undefined);
    vm.alloc_obj(obj)
}

fn regex_method(name: &str) -> Option<Value> {
    let (static_name, arity): (&'static str, usize) = match name {
        "test" => ("test", 1),
        "exec" => ("exec", 1),
        "toString" => ("toString", 0),
        _ => return None,
    };
    Some(native(static_name, arity, move |vm, this, args| {
        call_regex_method(vm, this, static_name, args).unwrap()
    }))
}

fn call_regex_method(
    vm: &mut Vm,
    this: Value,
    name: &str,
    args: &[Value],
) -> Option<Result<Value, Value>> {
    Some(match name {
        "test" => (|| {
            let s = value_as_cow_str(args.first().unwrap_or(&Value::Undefined));
            let Value::Object(o) = &this else {
                return Ok(Value::Bool(false));
            };
            let re = match &o.borrow().kind {
                ObjKind::Regex(r) => r.clone(),
                _ => return Ok(Value::Bool(false)),
            };
            with_chars_buf(&s, |chars| {
                let from = if re.global { regex_last_index(o) } else { 0 };
                if from > chars.len() {
                    if re.global {
                        set_last_index(o, 0);
                    }
                    return Ok(Value::Bool(false));
                }
                match with_regex_scratch(|scratch| re.search_with_scratch(chars, from, scratch))
                    .map_err(|e| regex_err(vm, e))?
                {
                    Some(mat) => {
                        if re.global {
                            set_last_index(o, mat.end);
                        }
                        Ok(Value::Bool(true))
                    }
                    None => {
                        if re.global {
                            set_last_index(o, 0);
                        }
                        Ok(Value::Bool(false))
                    }
                }
            })
        })(),
        "exec" => (|| {
            let s = value_as_cow_str(args.first().unwrap_or(&Value::Undefined));
            let Value::Object(o) = &this else {
                return Err(vm.type_error("RegExp ではありません"));
            };
            let re = match &o.borrow().kind {
                ObjKind::Regex(r) => r.clone(),
                _ => return Err(vm.type_error("RegExp ではありません")),
            };
            with_chars_buf(&s, |chars| {
                let from = if re.global { regex_last_index(o) } else { 0 };
                if from > chars.len() {
                    if re.global {
                        set_last_index(o, 0);
                    }
                    return Ok(Value::Null);
                }
                match with_regex_scratch(|scratch| re.search_with_scratch(chars, from, scratch))
                    .map_err(|e| regex_err(vm, e))?
                {
                    Some(mat) => {
                        if re.global {
                            set_last_index(o, mat.end);
                        }
                        Ok(build_match_array(vm, chars, &mat, &s))
                    }
                    None => {
                        if re.global {
                            set_last_index(o, 0);
                        }
                        Ok(Value::Null)
                    }
                }
            })
        })(),
        "toString" => Ok(Value::str(vm.to_display_string(&this))),
        _ => return None,
    })
}

// ---------------- String.prototype.replace(All) の置換テンプレート ----------------

/// `$&` `` $` `` `$'` `$1`〜 `$$` を `&mut String` へ直接展開する（正規表現マッチ版）。
/// `chars` スライスから直接書き込むことで、中間 `String`・`Vec<char>`・`groups` の
/// ヒープ確保を完全にゼロに抑える。
fn expand_regex_template_into(
    template: &str,
    chars: &[char],
    mat: &crate::regex::RegexMatch,
    out: &mut String,
) {
    let bytes = template.as_bytes();
    let mut i = 0usize;
    let mut chunk_start = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() {
            let next = bytes[i + 1];
            match next {
                b'$' => {
                    out.push_str(&template[chunk_start..i]);
                    out.push('$');
                    i += 2;
                    chunk_start = i;
                    continue;
                }
                b'&' => {
                    out.push_str(&template[chunk_start..i]);
                    out.extend(&chars[mat.start..mat.end]);
                    i += 2;
                    chunk_start = i;
                    continue;
                }
                b'`' => {
                    out.push_str(&template[chunk_start..i]);
                    out.extend(&chars[..mat.start]);
                    i += 2;
                    chunk_start = i;
                    continue;
                }
                b'\'' => {
                    out.push_str(&template[chunk_start..i]);
                    out.extend(&chars[mat.end..]);
                    i += 2;
                    chunk_start = i;
                    continue;
                }
                b'0'..=b'9' => {
                    let one = (next - b'0') as usize;
                    let two = if i + 2 < bytes.len() && bytes[i + 2].is_ascii_digit() {
                        Some(one * 10 + (bytes[i + 2] - b'0') as usize)
                    } else {
                        None
                    };
                    if let Some(n2) = two {
                        if n2 >= 1 && n2 <= mat.groups.len() {
                            out.push_str(&template[chunk_start..i]);
                            if let Some((a, b)) = mat.groups[n2 - 1] {
                                out.extend(&chars[a..b]);
                            }
                            i += 3;
                            chunk_start = i;
                            continue;
                        }
                    }
                    if one >= 1 && one <= mat.groups.len() {
                        out.push_str(&template[chunk_start..i]);
                        if let Some((a, b)) = mat.groups[one - 1] {
                            out.extend(&chars[a..b]);
                        }
                        i += 2;
                        chunk_start = i;
                        continue;
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    if chunk_start < bytes.len() {
        out.push_str(&template[chunk_start..]);
    }
}

/// 文字列パターン用の `$&` `` $` `` `$'` `$$` 展開を直接 `out` へ書き込む。
fn expand_str_template_into(
    template: &str,
    matched: &str,
    before: &str,
    after: &str,
    out: &mut String,
) {
    let bytes = template.as_bytes();
    let mut i = 0usize;
    let mut chunk_start = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'$' => {
                    out.push_str(&template[chunk_start..i]);
                    out.push('$');
                    i += 2;
                    chunk_start = i;
                    continue;
                }
                b'&' => {
                    out.push_str(&template[chunk_start..i]);
                    out.push_str(matched);
                    i += 2;
                    chunk_start = i;
                    continue;
                }
                b'`' => {
                    out.push_str(&template[chunk_start..i]);
                    out.push_str(before);
                    i += 2;
                    chunk_start = i;
                    continue;
                }
                b'\'' => {
                    out.push_str(&template[chunk_start..i]);
                    out.push_str(after);
                    i += 2;
                    chunk_start = i;
                    continue;
                }
                _ => {}
            }
        }
        i += 1;
    }
    if chunk_start < bytes.len() {
        out.push_str(&template[chunk_start..]);
    }
}

/// 正規表現マッチ 1 件分のコールバック置換を実行して `out` へ追記する。
fn call_regex_replace_cb_into(
    vm: &mut Vm,
    replacement: &Value,
    chars: &[char],
    mat: &crate::regex::RegexMatch,
    whole_input: &str,
    out: &mut String,
) -> Result<(), Value> {
    let mut matched = String::with_capacity(mat.end - mat.start);
    matched.extend(&chars[mat.start..mat.end]);
    let mut cb_args = Vec::with_capacity(3 + mat.groups.len());
    cb_args.push(Value::str(matched));
    for g in &mat.groups {
        cb_args.push(match g {
            Some((a, b)) => {
                let mut gs = String::with_capacity(*b - *a);
                gs.extend(&chars[*a..*b]);
                Value::str(gs)
            }
            None => Value::Undefined,
        });
    }
    cb_args.push(Value::Number(mat.start as f64));
    cb_args.push(Value::str(whole_input));
    let r = vm.call_value(replacement.clone(), Value::Undefined, &cb_args)?;
    crate::vm::to_string_into(&r, out);
    Ok(())
}

/// 文字列パターン（正規表現でない）の 1 件分の置換文字列を `out` へ追記する。
fn append_replacement_str(
    vm: &mut Vm,
    replacement: &Value,
    matched: &str,
    before: &str,
    after: &str,
    whole_input: &str,
    out: &mut String,
) -> Result<(), Value> {
    if is_callable(replacement) {
        let idx = if before.is_ascii() {
            before.len()
        } else {
            before.chars().count()
        };
        let r = vm.call_value(
            replacement.clone(),
            Value::Undefined,
            &[
                Value::str(matched),
                Value::Number(idx as f64),
                Value::str(whole_input),
            ],
        )?;
        crate::vm::to_string_into(&r, out);
        Ok(())
    } else {
        let template = value_as_cow_str(replacement);
        if template.contains('$') {
            expand_str_template_into(&template, matched, before, after, out);
        } else {
            out.push_str(&template);
        }
        Ok(())
    }
}

/// `String.prototype.replace`/`replaceAll` 本体。`pattern` が正規表現なら
/// `force_all || pattern.global` の時に全置換、そうでなければ最初の 1 件のみ。
/// `pattern` が文字列ならリテラル部分文字列として扱う（`force_all` の時のみ
/// 全置換、それ以外は最初の 1 件のみ — JS 仕様通り、文字列パターンの
/// `replace` はフラグを持てないので常に 1 件のみ）。
fn string_replace(
    vm: &mut Vm,
    s: &str,
    pattern: &Value,
    replacement: &Value,
    force_all: bool,
) -> Result<Value, Value> {
    let is_regex =
        matches!(pattern, Value::Object(o) if matches!(o.borrow().kind, ObjKind::Regex(_)));
    if is_regex {
        let Value::Object(o) = pattern else {
            unreachable!()
        };
        let re = match &o.borrow().kind {
            ObjKind::Regex(r) => r.clone(),
            _ => unreachable!(),
        };
        let global = force_all || re.global;
        let callable = is_callable(replacement);
        if !callable {
            let template = value_as_cow_str(replacement);
            let has_dollar = template.contains('$');
            return with_chars_buf(s, |chars| {
                let mut out = String::with_capacity(s.len());
                let mut pos = 0usize;
                let mut search_from = 0usize;
                with_regex_scratch(|scratch| -> Result<(), Value> {
                    loop {
                        if search_from > chars.len() {
                            break;
                        }
                        let Some(mat) = re
                            .search_with_scratch(chars, search_from, scratch)
                            .map_err(|e| regex_err(vm, e))?
                        else {
                            break;
                        };
                        out.extend(&chars[pos..mat.start]);
                        if has_dollar {
                            expand_regex_template_into(&template, chars, &mat, &mut out);
                        } else {
                            out.push_str(&template);
                        }
                        pos = mat.end;
                        search_from = if mat.end == mat.start {
                            mat.end + 1
                        } else {
                            mat.end
                        };
                        if !global {
                            break;
                        }
                    }
                    Ok(())
                })?;
                out.extend(&chars[pos..]);
                Ok(Value::str(out))
            });
        }
        // コールバック関数は再入し得るためローカル `Vec<char>` と `RegexScratch` を使う
        let chars: Vec<char> = s.chars().collect();
        let mut scratch = RegexScratch::default();
        let mut out = String::with_capacity(s.len());
        let mut pos = 0usize;
        let mut search_from = 0usize;
        loop {
            if search_from > chars.len() {
                break;
            }
            let Some(mat) = re
                .search_with_scratch(&chars, search_from, &mut scratch)
                .map_err(|e| regex_err(vm, e))?
            else {
                break;
            };
            out.extend(&chars[pos..mat.start]);
            call_regex_replace_cb_into(vm, replacement, &chars, &mat, s, &mut out)?;
            pos = mat.end;
            search_from = if mat.end == mat.start {
                mat.end + 1
            } else {
                mat.end
            };
            if !global {
                break;
            }
        }
        out.extend(&chars[pos..]);
        Ok(Value::str(out))
    } else {
        let needle = value_as_cow_str(pattern);
        // 最も頻出する「文字列リテラル置換かつ `$` 展開なし」の高速パス
        if !is_callable(replacement) {
            let template = value_as_cow_str(replacement);
            if !template.contains('$') {
                if force_all {
                    if needle.is_empty() {
                        let mut out =
                            String::with_capacity(s.len() + template.len() * (s.len() + 1));
                        for c in s.chars() {
                            out.push_str(&template);
                            out.push(c);
                        }
                        out.push_str(&template);
                        return Ok(Value::str(out));
                    }
                    return Ok(Value::str(s.replace(needle.as_ref(), template.as_ref())));
                } else {
                    return match s.find(needle.as_ref()) {
                        Some(idx) => {
                            let mut out =
                                String::with_capacity(s.len() - needle.len() + template.len());
                            out.push_str(&s[..idx]);
                            out.push_str(&template);
                            out.push_str(&s[idx + needle.len()..]);
                            Ok(Value::str(out))
                        }
                        None => Ok(Value::str(s)),
                    };
                }
            }
        }
        if force_all {
            if needle.is_empty() {
                // 空文字列パターンの replaceAll は各文字の間（先頭・末尾含む）に挿入する。
                let mut out = String::with_capacity(s.len() * 2);
                for (byte_i, c) in s.char_indices() {
                    let before = &s[..byte_i];
                    let after = &s[byte_i..];
                    append_replacement_str(vm, replacement, "", before, after, s, &mut out)?;
                    out.push(c);
                }
                append_replacement_str(vm, replacement, "", s, "", s, &mut out)?;
                return Ok(Value::str(out));
            }
            let mut out = String::with_capacity(s.len());
            let mut pos = 0usize;
            while let Some(rel) = s[pos..].find(needle.as_ref()) {
                let abs = pos + rel;
                out.push_str(&s[pos..abs]);
                let before = &s[..abs];
                let after = &s[abs + needle.len()..];
                append_replacement_str(vm, replacement, &needle, before, after, s, &mut out)?;
                pos = abs + needle.len();
            }
            out.push_str(&s[pos..]);
            Ok(Value::str(out))
        } else {
            match s.find(needle.as_ref()) {
                Some(idx) => {
                    let before = &s[..idx];
                    let after = &s[idx + needle.len()..];
                    let mut out = String::with_capacity(s.len());
                    out.push_str(before);
                    append_replacement_str(vm, replacement, &needle, before, after, s, &mut out)?;
                    out.push_str(after);
                    Ok(Value::str(out))
                }
                None => Ok(Value::str(s)),
            }
        }
    }
}

/// `String.prototype.split(regex, limit)` 本体。捕捉されたキャプチャ
/// グループも（`undefined` を挟みつつ）結果配列に含める（JS 仕様通り）。
fn regex_split(
    vm: &mut Vm,
    re: &crate::regex::Regex,
    s: &str,
    limit: usize,
) -> Result<Vec<Value>, Value> {
    with_chars_buf(s, |chars| {
        if chars.is_empty() {
            let mut out = vec![];
            if with_regex_scratch(|scratch| re.search_with_scratch(chars, 0, scratch))
                .map_err(|e| regex_err(vm, e))?
                .is_none()
            {
                out.push(Value::str(""));
            }
            out.truncate(limit);
            return Ok(out);
        }
        let mut out: Vec<Value> = vec![];
        let mut p = 0usize;
        let mut search_from = 0usize;
        with_regex_scratch(|scratch| -> Result<(), Value> {
            loop {
                if out.len() >= limit || search_from > chars.len() {
                    break;
                }
                let Some(mat) = re
                    .search_with_scratch(chars, search_from, scratch)
                    .map_err(|e| regex_err(vm, e))?
                else {
                    break;
                };
                if mat.start >= chars.len() {
                    break;
                }
                if mat.end == mat.start && mat.start == p {
                    search_from = mat.start + 1;
                    continue;
                }
                let mut piece = String::with_capacity(mat.start - p);
                piece.extend(&chars[p..mat.start]);
                out.push(Value::str(piece));
                for g in &mat.groups {
                    if out.len() >= limit {
                        break;
                    }
                    out.push(match g {
                        Some((a, b)) => {
                            let mut gs = String::with_capacity(*b - *a);
                            gs.extend(&chars[*a..*b]);
                            Value::str(gs)
                        }
                        None => Value::Undefined,
                    });
                }
                p = mat.end;
                search_from = if mat.end == mat.start {
                    mat.end + 1
                } else {
                    mat.end
                };
            }
            Ok(())
        })?;
        if out.len() < limit {
            let mut tail = String::with_capacity(chars.len() - p);
            tail.extend(&chars[p..]);
            out.push(Value::str(tail));
        }
        out.truncate(limit);
        Ok(out)
    })
}
