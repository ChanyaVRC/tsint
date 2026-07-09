//! 組み込みオブジェクト・グローバル関数群。
//!
//! Noiria コーパス（`runtime/kag/parser.ts` / `runtime/kag/vars.ts`）が
//! 使う分＋要求仕様に挙げられた分だけを実装する。汎用 JS エンジンを
//! 目指すものではないため、対応外の呼び出しは「明確なエラー」を返す
//! （黙って undefined を返したりしない）方針を優先する。

use std::cell::RefCell;
use std::rc::Rc;

use crate::value::*;
use crate::vm::{array_values, is_callable, number_to_js_string, to_number, KindTag, Vm};

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
        let indent = match arg(args, 2) {
            Value::Number(n) => n.max(0.0) as usize,
            _ => 0,
        };
        match json_stringify(&arg(args, 0), indent, 0) {
            Some(s) => Ok(Value::str(s)),
            None => Ok(Value::Undefined),
        }
    });
    let parse = native("parse", 1, |vm, _this, args| {
        let s = vm.to_display_string(&arg(args, 0));
        json_parse(&s).map_err(|e| vm.make_error("SyntaxError", format!("JSON.parse: {e}")))
    });
    if let Value::Object(o) = &json {
        let mut b = o.borrow_mut();
        b.props.set_value(Rc::from("stringify"), stringify);
        b.props.set_value(Rc::from("parse"), parse);
    }
    vm.define_global("JSON", json);
}

fn json_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn json_stringify(v: &Value, indent: usize, depth: usize) -> Option<String> {
    let pad = |d: usize| " ".repeat(indent * d);
    match v {
        Value::Undefined | Value::Uninit => None,
        Value::Null => Some("null".to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(if n.is_finite() {
            number_to_js_string(*n)
        } else {
            "null".to_string()
        }),
        Value::Str(s) => Some(json_quote(s)),
        Value::Object(o) => {
            let is_array = matches!(o.borrow().kind, ObjKind::Array(_));
            let is_callable = matches!(
                o.borrow().kind,
                ObjKind::Function(_) | ObjKind::Native(_) | ObjKind::Class(_)
            );
            if is_callable {
                return None;
            }
            if is_array {
                let items = array_values(v);
                let parts: Vec<String> = items
                    .iter()
                    .map(|it| {
                        json_stringify(it, indent, depth + 1).unwrap_or_else(|| "null".to_string())
                    })
                    .collect();
                if indent == 0 || parts.is_empty() {
                    return Some(format!("[{}]", parts.join(",")));
                }
                let inner = parts
                    .iter()
                    .map(|p| format!("{}{}", pad(depth + 1), p))
                    .collect::<Vec<_>>()
                    .join(",\n");
                return Some(format!("[\n{}\n{}]", inner, pad(depth)));
            }
            let entries: Vec<(Rc<str>, Value)> = o
                .borrow()
                .props
                .iter()
                .filter_map(|(k, slot)| match slot {
                    PropSlot::Value(val) => Some((k.clone(), val.clone())),
                    PropSlot::Accessor { .. } => None,
                })
                .collect();
            let mut parts = Vec::new();
            for (k, val) in entries {
                if let Some(s) = json_stringify(&val, indent, depth + 1) {
                    parts.push(format!("{}:{}", json_quote(&k), s));
                }
            }
            if indent == 0 || parts.is_empty() {
                return Some(format!("{{{}}}", parts.join(",")));
            }
            let inner = parts
                .iter()
                .map(|p| format!("{}{}", pad(depth + 1), p))
                .collect::<Vec<_>>()
                .join(",\n");
            Some(format!("{{\n{}\n{}}}", inner, pad(depth)))
        }
    }
}

struct JsonParser<'a> {
    chars: Vec<char>,
    pos: usize,
    _src: &'a str,
}

fn json_parse(s: &str) -> Result<Value, String> {
    let mut p = JsonParser {
        chars: s.chars().collect(),
        pos: 0,
        _src: s,
    };
    p.skip_ws();
    let v = p.parse_value()?;
    p.skip_ws();
    if p.pos != p.chars.len() {
        return Err("末尾に余分な文字があります".to_string());
    }
    Ok(v)
}

impl<'a> JsonParser<'a> {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }
    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(c) if c.is_whitespace()) {
            self.pos += 1;
        }
    }
    fn expect(&mut self, c: char) -> Result<(), String> {
        if self.peek() == Some(c) {
            self.pos += 1;
            Ok(())
        } else {
            Err(format!("'{c}' を期待しました（位置 {}）", self.pos))
        }
    }
    fn parse_value(&mut self) -> Result<Value, String> {
        self.skip_ws();
        match self.peek() {
            Some('{') => self.parse_object(),
            Some('[') => self.parse_array(),
            Some('"') => Ok(Value::str(self.parse_string()?)),
            Some('t') => {
                self.expect_word("true")?;
                Ok(Value::Bool(true))
            }
            Some('f') => {
                self.expect_word("false")?;
                Ok(Value::Bool(false))
            }
            Some('n') => {
                self.expect_word("null")?;
                Ok(Value::Null)
            }
            Some(c) if c == '-' || c.is_ascii_digit() => self.parse_number(),
            _ => Err(format!("値を期待しました（位置 {}）", self.pos)),
        }
    }
    fn expect_word(&mut self, w: &str) -> Result<(), String> {
        for c in w.chars() {
            self.expect(c)?;
        }
        Ok(())
    }
    fn parse_object(&mut self) -> Result<Value, String> {
        self.expect('{')?;
        let obj = Value::new_plain_object();
        self.skip_ws();
        if self.peek() == Some('}') {
            self.pos += 1;
            return Ok(obj);
        }
        loop {
            self.skip_ws();
            let key = self.parse_string()?;
            self.skip_ws();
            self.expect(':')?;
            let val = self.parse_value()?;
            if let Value::Object(o) = &obj {
                o.borrow_mut().props.set_value(Rc::from(key.as_str()), val);
            }
            self.skip_ws();
            match self.peek() {
                Some(',') => {
                    self.pos += 1;
                }
                Some('}') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(format!("',' か '}}' を期待しました（位置 {}）", self.pos)),
            }
        }
        Ok(obj)
    }
    fn parse_array(&mut self) -> Result<Value, String> {
        self.expect('[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(']') {
            self.pos += 1;
            return Ok(Value::new_array(items));
        }
        loop {
            let v = self.parse_value()?;
            items.push(v);
            self.skip_ws();
            match self.peek() {
                Some(',') => {
                    self.pos += 1;
                }
                Some(']') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(format!("',' か ']' を期待しました（位置 {}）", self.pos)),
            }
        }
        Ok(Value::new_array(items))
    }
    fn parse_string(&mut self) -> Result<String, String> {
        self.expect('"')?;
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return Err("文字列が閉じられていません".to_string()),
                Some('"') => {
                    self.pos += 1;
                    break;
                }
                Some('\\') => {
                    self.pos += 1;
                    match self.peek() {
                        Some('"') => out.push('"'),
                        Some('\\') => out.push('\\'),
                        Some('/') => out.push('/'),
                        Some('n') => out.push('\n'),
                        Some('t') => out.push('\t'),
                        Some('r') => out.push('\r'),
                        Some('b') => out.push('\u{8}'),
                        Some('f') => out.push('\u{c}'),
                        Some('u') => {
                            let hex: String =
                                self.chars[self.pos + 1..self.pos + 5].iter().collect();
                            let code = u32::from_str_radix(&hex, 16)
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
                Some(c) => {
                    out.push(c);
                    self.pos += 1;
                }
            }
        }
        Ok(out)
    }
    fn parse_number(&mut self) -> Result<Value, String> {
        let start = self.pos;
        if self.peek() == Some('-') {
            self.pos += 1;
        }
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.pos += 1;
        }
        if self.peek() == Some('.') {
            self.pos += 1;
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some('e') | Some('E')) {
            self.pos += 1;
            if matches!(self.peek(), Some('+') | Some('-')) {
                self.pos += 1;
            }
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        let s: String = self.chars[start..self.pos].iter().collect();
        s.parse::<f64>()
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
                        if let Some(idx) = data.find(&k) {
                            data.entries[idx].1 = val;
                        } else {
                            data.entries.push((k, val));
                        }
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
                        if data.find(&v).is_none() {
                            data.values.push(v);
                        }
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
        let p_res = promise_obj.clone();
        let resolve_fn = Value::native_fn(
            "",
            1,
            Rc::new(move |vm, _this, args| {
                vm.resolve_promise(p_res.clone(), arg(args, 0));
                Ok(Value::Undefined)
            }),
        );
        let p_rej = promise_obj.clone();
        let reject_fn = Value::native_fn(
            "",
            1,
            Rc::new(move |vm, _this, args| {
                vm.settle_promise(p_rej.clone(), Err(arg(args, 0)));
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
            let on_fulfilled = settle_forward_fn(result_obj.clone(), false);
            let on_rejected = settle_forward_fn(result_obj.clone(), true);
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
fn settle_forward_fn(target: ObjRef, is_reject: bool) -> Value {
    native("", 1, move |vm, _this, args| {
        let v = arg(args, 0);
        if is_reject {
            vm.settle_promise(target.clone(), Err(v));
        } else {
            vm.resolve_promise(target.clone(), v);
        }
        Ok(Value::Undefined)
    })
}

fn promise_method(name: &str) -> Option<Value> {
    Some(match name {
        "then" => native("then", 2, |vm, this, args| {
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
        }),
        "catch" => native("catch", 1, |vm, this, args| {
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
        }),
        "finally" => native("finally", 1, |vm, this, args| {
            let Value::Object(p) = &this else {
                return Err(vm.type_error("Promise ではありません"));
            };
            let cb = arg(args, 0);
            let (result_val, result_obj) = vm.new_pending_promise();
            let cb_f = cb.clone();
            // finally のコールバックは引数無しで呼ばれ、戻り値は無視される
            // （投げた場合のみそれが新しい結果を上書きする）。元の値/例外は
            // そのまま透過する（`then`/`catch` と違い finally は値を変えない）。
            let on_fulfilled = native("", 1, move |vm, _this, args| {
                let v = arg(args, 0);
                vm.call_value(cb_f.clone(), Value::Undefined, &[])?;
                Ok(v)
            });
            let on_rejected = native("", 1, move |vm, _this, args| {
                let e = arg(args, 0);
                vm.call_value(cb.clone(), Value::Undefined, &[])?;
                Err(e)
            });
            vm.add_promise_reaction(
                p,
                PromiseReaction::Then {
                    on_fulfilled: Some(on_fulfilled),
                    on_rejected: Some(on_rejected),
                    result_promise: result_obj,
                },
            );
            Ok(result_val)
        }),
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

fn with_array<T>(this: &Value, f: impl FnOnce(&mut Vec<Value>) -> T) -> Option<T> {
    if let Value::Object(o) = this {
        if let ObjKind::Array(items) = &mut o.borrow_mut().kind {
            return Some(f(items));
        }
    }
    None
}

fn array_method(name: &str) -> Option<Value> {
    Some(match name {
        "push" => native("push", 1, |_vm, this, args| {
            let len = with_array(&this, |items| {
                items.extend_from_slice(args);
                items.len()
            })
            .unwrap_or(0);
            Ok(Value::Number(len as f64))
        }),
        "pop" => native("pop", 0, |_vm, this, _args| {
            Ok(with_array(&this, |items| items.pop())
                .flatten()
                .unwrap_or(Value::Undefined))
        }),
        "shift" => native("shift", 0, |_vm, this, _args| {
            Ok(with_array(&this, |items| {
                if items.is_empty() {
                    None
                } else {
                    Some(items.remove(0))
                }
            })
            .flatten()
            .unwrap_or(Value::Undefined))
        }),
        "unshift" => native("unshift", 1, |_vm, this, args| {
            let len = with_array(&this, |items| {
                for (i, a) in args.iter().enumerate() {
                    items.insert(i, a.clone());
                }
                items.len()
            })
            .unwrap_or(0);
            Ok(Value::Number(len as f64))
        }),
        "slice" => native("slice", 2, |_vm, this, args| {
            let items = array_values(&this);
            let len = items.len() as i64;
            let start = norm_index(args.first(), 0, len);
            let end = norm_index(args.get(1), len, len);
            let out = if start < end {
                items[start as usize..end as usize].to_vec()
            } else {
                Vec::new()
            };
            Ok(Value::new_array(out))
        }),
        "splice" => native("splice", 2, |_vm, this, args| {
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
            Ok(Value::new_array(removed))
        }),
        "join" => native("join", 1, |vm, this, args| {
            let sep = match args.first() {
                Some(Value::Undefined) | None => ",".to_string(),
                Some(v) => vm.to_display_string(v),
            };
            let items = array_values(&this);
            let parts: Vec<String> = items
                .iter()
                .map(|it| match it {
                    Value::Undefined | Value::Null => String::new(),
                    other => vm.to_display_string(other),
                })
                .collect();
            Ok(Value::str(parts.join(&sep)))
        }),
        "indexOf" => native("indexOf", 1, |_vm, this, args| {
            let items = array_values(&this);
            let target = arg(args, 0);
            let idx = items
                .iter()
                .position(|v| crate::value::strict_eq(v, &target));
            Ok(Value::Number(idx.map(|i| i as f64).unwrap_or(-1.0)))
        }),
        "includes" => native("includes", 1, |_vm, this, args| {
            let items = array_values(&this);
            let target = arg(args, 0);
            Ok(Value::Bool(
                items.iter().any(|v| same_value_zero(v, &target)),
            ))
        }),
        "concat" => native("concat", 1, |_vm, this, args| {
            let mut out = array_values(&this);
            for a in args {
                if let Value::Object(o) = a {
                    if matches!(o.borrow().kind, ObjKind::Array(_)) {
                        out.extend(array_values(a));
                        continue;
                    }
                }
                out.push(a.clone());
            }
            Ok(Value::new_array(out))
        }),
        "map" => native("map", 1, |vm, this, args| {
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
            Ok(Value::new_array(out))
        }),
        "filter" => native("filter", 1, |vm, this, args| {
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
            Ok(Value::new_array(out))
        }),
        "forEach" => native("forEach", 1, |vm, this, args| {
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
        }),
        "find" => native("find", 1, |vm, this, args| {
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
        }),
        "flatMap" => native("flatMap", 1, |vm, this, args| {
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
                    if matches!(o.borrow().kind, ObjKind::Array(_)) {
                        out.extend(array_values(&r));
                        continue;
                    }
                }
                out.push(r);
            }
            Ok(Value::new_array(out))
        }),
        "some" => native("some", 1, |vm, this, args| {
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
        }),
        "every" => native("every", 1, |vm, this, args| {
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
        }),
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
        return Ok(Value::Number(s.chars().count() as f64));
    }
    if let Ok(idx) = name.parse::<usize>() {
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
    Some(match name {
        "slice" => native("slice", 2, |vm, this, args| {
            let s = vm.to_display_string(&this);
            let chars: Vec<char> = s.chars().collect();
            let len = chars.len() as i64;
            let start = norm_index(args.first(), 0, len);
            let end = norm_index(args.get(1), len, len);
            let out: String = if start < end {
                chars[start as usize..end as usize].iter().collect()
            } else {
                String::new()
            };
            Ok(Value::str(out))
        }),
        "split" => native("split", 2, |vm, this, args| {
            let s = vm.to_display_string(&this);
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
                    let mut out = vec![Value::str(s)];
                    out.truncate(limit);
                    Ok(Value::new_array(out))
                }
                Some(Value::Object(o)) if matches!(o.borrow().kind, ObjKind::Regex(_)) => {
                    let re = match &o.borrow().kind {
                        ObjKind::Regex(r) => r.clone(),
                        _ => unreachable!(),
                    };
                    Ok(Value::new_array(regex_split(vm, &re, &s, limit)?))
                }
                Some(sep) => {
                    let sep_s = vm.to_display_string(sep);
                    let mut out: Vec<Value> = if sep_s.is_empty() {
                        s.chars().map(|c| Value::str(c.to_string())).collect()
                    } else {
                        s.split(sep_s.as_str()).map(Value::str).collect()
                    };
                    out.truncate(limit);
                    Ok(Value::new_array(out))
                }
            }
        }),
        "trim" => native("trim", 0, |vm, this, _args| {
            Ok(Value::str(vm.to_display_string(&this).trim().to_string()))
        }),
        "startsWith" => native("startsWith", 1, |vm, this, args| {
            let s = vm.to_display_string(&this);
            let p = vm.to_display_string(&arg(args, 0));
            Ok(Value::Bool(s.starts_with(&p)))
        }),
        "endsWith" => native("endsWith", 1, |vm, this, args| {
            let s = vm.to_display_string(&this);
            let p = vm.to_display_string(&arg(args, 0));
            Ok(Value::Bool(s.ends_with(&p)))
        }),
        "includes" => native("includes", 1, |vm, this, args| {
            let s = vm.to_display_string(&this);
            let p = vm.to_display_string(&arg(args, 0));
            Ok(Value::Bool(s.contains(&p)))
        }),
        "indexOf" => native("indexOf", 1, |vm, this, args| {
            let s = vm.to_display_string(&this);
            let p = vm.to_display_string(&arg(args, 0));
            let chars: Vec<char> = s.chars().collect();
            let needle: Vec<char> = p.chars().collect();
            if needle.is_empty() {
                return Ok(Value::Number(0.0));
            }
            for i in 0..=chars.len().saturating_sub(needle.len()) {
                if chars[i..i + needle.len()] == needle[..] {
                    return Ok(Value::Number(i as f64));
                }
            }
            Ok(Value::Number(-1.0))
        }),
        "replace" => native("replace", 2, |vm, this, args| {
            let s = vm.to_display_string(&this);
            let pattern = arg(args, 0);
            let replacement = arg(args, 1);
            string_replace(vm, &s, &pattern, &replacement, false)
        }),
        "replaceAll" => native("replaceAll", 2, |vm, this, args| {
            let s = vm.to_display_string(&this);
            let pattern = arg(args, 0);
            let replacement = arg(args, 1);
            if let Value::Object(o) = &pattern {
                if let ObjKind::Regex(re) = &o.borrow().kind {
                    if !re.global {
                        return Err(vm.type_error(
                            "String.prototype.replaceAll に渡す正規表現には g フラグが必要です",
                        ));
                    }
                }
            }
            string_replace(vm, &s, &pattern, &replacement, true)
        }),
        "match" => native("match", 1, |vm, this, args| {
            let s = vm.to_display_string(&this);
            let re = coerce_to_regex(vm, &arg(args, 0))?;
            let chars: Vec<char> = s.chars().collect();
            if re.global {
                let mut out = vec![];
                let mut from = 0usize;
                loop {
                    if from > chars.len() {
                        break;
                    }
                    match re.search(&chars, from).map_err(|e| regex_err(vm, e))? {
                        Some(mat) => {
                            out.push(Value::str(
                                chars[mat.start..mat.end].iter().collect::<String>(),
                            ));
                            from = if mat.end == mat.start {
                                mat.end + 1
                            } else {
                                mat.end
                            };
                        }
                        None => break,
                    }
                }
                if out.is_empty() {
                    Ok(Value::Null)
                } else {
                    Ok(Value::new_array(out))
                }
            } else {
                match re.search(&chars, 0).map_err(|e| regex_err(vm, e))? {
                    Some(mat) => Ok(build_match_array(&chars, &mat, &s)),
                    None => Ok(Value::Null),
                }
            }
        }),
        "search" => native("search", 1, |vm, this, args| {
            let s = vm.to_display_string(&this);
            let re = coerce_to_regex(vm, &arg(args, 0))?;
            let chars: Vec<char> = s.chars().collect();
            match re.search(&chars, 0).map_err(|e| regex_err(vm, e))? {
                Some(mat) => Ok(Value::Number(mat.start as f64)),
                None => Ok(Value::Number(-1.0)),
            }
        }),
        "toLowerCase" => native("toLowerCase", 0, |vm, this, _args| {
            Ok(Value::str(vm.to_display_string(&this).to_lowercase()))
        }),
        "toUpperCase" => native("toUpperCase", 0, |vm, this, _args| {
            Ok(Value::str(vm.to_display_string(&this).to_uppercase()))
        }),
        "charCodeAt" => native("charCodeAt", 1, |vm, this, args| {
            let s = vm.to_display_string(&this);
            let idx = to_number(&arg(args, 0)) as usize;
            Ok(s.chars()
                .nth(idx)
                .map(|c| Value::Number(c as u32 as f64))
                .unwrap_or(Value::Number(f64::NAN)))
        }),
        "codePointAt" => native("codePointAt", 1, |vm, this, args| {
            let s = vm.to_display_string(&this);
            let idx = to_number(&arg(args, 0)) as usize;
            Ok(s.chars()
                .nth(idx)
                .map(|c| Value::Number(c as u32 as f64))
                .unwrap_or(Value::Undefined))
        }),
        "repeat" => native("repeat", 1, |vm, this, args| {
            let s = vm.to_display_string(&this);
            let n = to_number(&arg(args, 0));
            if n < 0.0 || !n.is_finite() {
                return Err(vm.range_error("repeat count は非負の有限数である必要があります"));
            }
            Ok(Value::str(s.repeat(n as usize)))
        }),
        "padStart" => native("padStart", 2, |vm, this, args| {
            let s = vm.to_display_string(&this);
            let target_len = to_number(&arg(args, 0)) as usize;
            let pad = match args.get(1) {
                Some(v) => vm.to_display_string(v),
                None => " ".to_string(),
            };
            let cur_len = s.chars().count();
            if cur_len >= target_len || pad.is_empty() {
                return Ok(Value::str(s));
            }
            let mut prefix = String::new();
            let pad_chars: Vec<char> = pad.chars().collect();
            while prefix.chars().count() < target_len - cur_len {
                prefix.extend(pad_chars.iter());
            }
            let prefix: String = prefix.chars().take(target_len - cur_len).collect();
            Ok(Value::str(format!("{prefix}{s}")))
        }),
        _ => return None,
    })
}

// ---------------- Map ----------------

fn map_method(name: &str) -> Option<Value> {
    Some(match name {
        "get" => native("get", 1, |_vm, this, args| {
            let key = arg(args, 0);
            if let Value::Object(o) = &this {
                if let ObjKind::Map(m) = &o.borrow().kind {
                    if let Some(i) = m.find(&key) {
                        return Ok(m.entries[i].1.clone());
                    }
                }
            }
            Ok(Value::Undefined)
        }),
        "set" => native("set", 2, |_vm, this, args| {
            let key = arg(args, 0);
            let val = arg(args, 1);
            if let Value::Object(o) = &this {
                if let ObjKind::Map(m) = &mut o.borrow_mut().kind {
                    if let Some(i) = m.find(&key) {
                        m.entries[i].1 = val;
                    } else {
                        m.entries.push((key, val));
                    }
                }
            }
            Ok(this)
        }),
        "has" => native("has", 1, |_vm, this, args| {
            let key = arg(args, 0);
            if let Value::Object(o) = &this {
                if let ObjKind::Map(m) = &o.borrow().kind {
                    return Ok(Value::Bool(m.find(&key).is_some()));
                }
            }
            Ok(Value::Bool(false))
        }),
        "delete" => native("delete", 1, |_vm, this, args| {
            let key = arg(args, 0);
            if let Value::Object(o) = &this {
                if let ObjKind::Map(m) = &mut o.borrow_mut().kind {
                    if let Some(i) = m.find(&key) {
                        m.entries.remove(i);
                        return Ok(Value::Bool(true));
                    }
                }
            }
            Ok(Value::Bool(false))
        }),
        "keys" => native("keys", 0, |_vm, this, _args| {
            if let Value::Object(o) = &this {
                if let ObjKind::Map(m) = &o.borrow().kind {
                    return Ok(Value::new_array(
                        m.entries.iter().map(|(k, _)| k.clone()).collect(),
                    ));
                }
            }
            Ok(Value::new_array(Vec::new()))
        }),
        "values" => native("values", 0, |_vm, this, _args| {
            if let Value::Object(o) = &this {
                if let ObjKind::Map(m) = &o.borrow().kind {
                    return Ok(Value::new_array(
                        m.entries.iter().map(|(_, v)| v.clone()).collect(),
                    ));
                }
            }
            Ok(Value::new_array(Vec::new()))
        }),
        "entries" => native("entries", 0, |_vm, this, _args| {
            if let Value::Object(o) = &this {
                if let ObjKind::Map(m) = &o.borrow().kind {
                    return Ok(Value::new_array(
                        m.entries
                            .iter()
                            .map(|(k, v)| Value::new_array(vec![k.clone(), v.clone()]))
                            .collect(),
                    ));
                }
            }
            Ok(Value::new_array(Vec::new()))
        }),
        "forEach" => native("forEach", 1, |vm, this, args| {
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
        }),
        "clear" => native("clear", 0, |_vm, this, _args| {
            if let Value::Object(o) = &this {
                if let ObjKind::Map(m) = &mut o.borrow_mut().kind {
                    m.entries.clear();
                }
            }
            Ok(Value::Undefined)
        }),
        _ => return None,
    })
}

// ---------------- Set ----------------

fn set_method(name: &str) -> Option<Value> {
    Some(match name {
        "add" => native("add", 1, |_vm, this, args| {
            let v = arg(args, 0);
            if let Value::Object(o) = &this {
                if let ObjKind::Set(s) = &mut o.borrow_mut().kind {
                    if s.find(&v).is_none() {
                        s.values.push(v);
                    }
                }
            }
            Ok(this)
        }),
        "has" => native("has", 1, |_vm, this, args| {
            let v = arg(args, 0);
            if let Value::Object(o) = &this {
                if let ObjKind::Set(s) = &o.borrow().kind {
                    return Ok(Value::Bool(s.find(&v).is_some()));
                }
            }
            Ok(Value::Bool(false))
        }),
        "delete" => native("delete", 1, |_vm, this, args| {
            let v = arg(args, 0);
            if let Value::Object(o) = &this {
                if let ObjKind::Set(s) = &mut o.borrow_mut().kind {
                    if let Some(i) = s.find(&v) {
                        s.values.remove(i);
                        return Ok(Value::Bool(true));
                    }
                }
            }
            Ok(Value::Bool(false))
        }),
        "keys" | "values" => native("values", 0, |_vm, this, _args| {
            if let Value::Object(o) = &this {
                if let ObjKind::Set(s) = &o.borrow().kind {
                    return Ok(Value::new_array(s.values.clone()));
                }
            }
            Ok(Value::new_array(Vec::new()))
        }),
        "forEach" => native("forEach", 1, |vm, this, args| {
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
        }),
        "clear" => native("clear", 0, |_vm, this, _args| {
            if let Value::Object(o) = &this {
                if let ObjKind::Set(s) = &mut o.borrow_mut().kind {
                    s.values.clear();
                }
            }
            Ok(Value::Undefined)
        }),
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
    o.borrow_mut()
        .props
        .set_value(Rc::from("lastIndex"), Value::Number(n as f64));
}

/// `RegExp.prototype.exec` の戻り値の配列（`index`/`input`/`groups` プロパティ
/// 付き）を組み立てる。
fn build_match_array(chars: &[char], mat: &crate::regex::RegexMatch, input: &str) -> Value {
    let whole: String = chars[mat.start..mat.end].iter().collect();
    let mut items = vec![Value::str(whole)];
    for g in &mat.groups {
        items.push(match g {
            Some((a, b)) => Value::str(chars[*a..*b].iter().collect::<String>()),
            None => Value::Undefined,
        });
    }
    let arr = Value::new_array(items);
    if let Value::Object(o) = &arr {
        let mut b = o.borrow_mut();
        b.props
            .set_value(Rc::from("index"), Value::Number(mat.start as f64));
        b.props
            .set_value(Rc::from("input"), Value::str(input.to_string()));
        // 名前付きキャプチャグループは未対応のため常に undefined（実 JS の
        // 「名前付きグループが 1 つも無ければ undefined」という挙動と一致する）。
        b.props.set_value(Rc::from("groups"), Value::Undefined);
    }
    arr
}

fn regex_method(name: &str) -> Option<Value> {
    Some(match name {
        "test" => native("test", 1, |vm, this, args| {
            let s = vm.to_display_string(&arg(args, 0));
            let Value::Object(o) = &this else {
                return Ok(Value::Bool(false));
            };
            let re = match &o.borrow().kind {
                ObjKind::Regex(r) => r.clone(),
                _ => return Ok(Value::Bool(false)),
            };
            let chars: Vec<char> = s.chars().collect();
            let from = if re.global { regex_last_index(o) } else { 0 };
            if from > chars.len() {
                if re.global {
                    set_last_index(o, 0);
                }
                return Ok(Value::Bool(false));
            }
            match re.search(&chars, from).map_err(|e| regex_err(vm, e))? {
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
        }),
        "exec" => native("exec", 1, |vm, this, args| {
            let s = vm.to_display_string(&arg(args, 0));
            let Value::Object(o) = &this else {
                return Err(vm.type_error("RegExp ではありません"));
            };
            let re = match &o.borrow().kind {
                ObjKind::Regex(r) => r.clone(),
                _ => return Err(vm.type_error("RegExp ではありません")),
            };
            let chars: Vec<char> = s.chars().collect();
            let from = if re.global { regex_last_index(o) } else { 0 };
            if from > chars.len() {
                if re.global {
                    set_last_index(o, 0);
                }
                return Ok(Value::Null);
            }
            match re.search(&chars, from).map_err(|e| regex_err(vm, e))? {
                Some(mat) => {
                    if re.global {
                        set_last_index(o, mat.end);
                    }
                    Ok(build_match_array(&chars, &mat, &s))
                }
                None => {
                    if re.global {
                        set_last_index(o, 0);
                    }
                    Ok(Value::Null)
                }
            }
        }),
        "toString" => native("toString", 0, |vm, this, _args| {
            Ok(Value::str(vm.to_display_string(&this)))
        }),
        _ => return None,
    })
}

// ---------------- String.prototype.replace(All) の置換テンプレート ----------------

/// `$&` `` $` `` `$'` `$1`〜 `$$` を展開する（コールバック関数でない、
/// 文字列置換の場合）。`groups` は 1 番から順のキャプチャ文字列
/// （`None` は未参加）。
fn expand_replacement_template(
    template: &str,
    matched: &str,
    before: &str,
    after: &str,
    groups: &[Option<String>],
) -> String {
    let tchars: Vec<char> = template.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < tchars.len() {
        if tchars[i] == '$' && i + 1 < tchars.len() {
            match tchars[i + 1] {
                '$' => {
                    out.push('$');
                    i += 2;
                    continue;
                }
                '&' => {
                    out.push_str(matched);
                    i += 2;
                    continue;
                }
                '`' => {
                    out.push_str(before);
                    i += 2;
                    continue;
                }
                '\'' => {
                    out.push_str(after);
                    i += 2;
                    continue;
                }
                d if d.is_ascii_digit() => {
                    let one = d.to_digit(10).unwrap() as usize;
                    let two = if i + 2 < tchars.len() && tchars[i + 2].is_ascii_digit() {
                        Some(one * 10 + tchars[i + 2].to_digit(10).unwrap() as usize)
                    } else {
                        None
                    };
                    // 2桁の番号が有効なキャプチャ番号ならそちらを優先する（JS 仕様）。
                    if let Some(n2) = two {
                        if n2 >= 1 && n2 <= groups.len() {
                            if let Some(g) = groups.get(n2 - 1).and_then(|x| x.as_ref()) {
                                out.push_str(g);
                            }
                            i += 3;
                            continue;
                        }
                    }
                    if one >= 1 && one <= groups.len() {
                        if let Some(g) = groups.get(one - 1).and_then(|x| x.as_ref()) {
                            out.push_str(g);
                        }
                        i += 2;
                        continue;
                    }
                    out.push(tchars[i]);
                    i += 1;
                }
                _ => {
                    out.push(tchars[i]);
                    i += 1;
                }
            }
        } else {
            out.push(tchars[i]);
            i += 1;
        }
    }
    out
}

/// 正規表現マッチ 1 件分の置換文字列を計算する（コールバック関数 or `$` 展開）。
fn compute_replacement(
    vm: &mut Vm,
    replacement: &Value,
    chars: &[char],
    mat: &crate::regex::RegexMatch,
    matched: &str,
    whole_input: &str,
) -> Result<String, Value> {
    if is_callable(replacement) {
        let mut cb_args = vec![Value::str(matched.to_string())];
        for g in &mat.groups {
            cb_args.push(match g {
                Some((a, b)) => Value::str(chars[*a..*b].iter().collect::<String>()),
                None => Value::Undefined,
            });
        }
        cb_args.push(Value::Number(mat.start as f64));
        cb_args.push(Value::str(whole_input.to_string()));
        let r = vm.call_value(replacement.clone(), Value::Undefined, &cb_args)?;
        Ok(vm.to_display_string(&r))
    } else {
        let template = vm.to_display_string(replacement);
        let before: String = chars[..mat.start].iter().collect();
        let after: String = chars[mat.end..].iter().collect();
        let groups: Vec<Option<String>> = mat
            .groups
            .iter()
            .map(|g| g.map(|(a, b)| chars[a..b].iter().collect()))
            .collect();
        Ok(expand_replacement_template(
            &template, matched, &before, &after, &groups,
        ))
    }
}

/// 文字列パターン（正規表現でない）の 1 件分の置換文字列を計算する
/// （キャプチャグループは無い）。
fn compute_replacement_str(
    vm: &mut Vm,
    replacement: &Value,
    matched: &str,
    before: &str,
    after: &str,
    whole_input: &str,
) -> Result<String, Value> {
    if is_callable(replacement) {
        let idx = before.chars().count();
        let r = vm.call_value(
            replacement.clone(),
            Value::Undefined,
            &[
                Value::str(matched.to_string()),
                Value::Number(idx as f64),
                Value::str(whole_input.to_string()),
            ],
        )?;
        Ok(vm.to_display_string(&r))
    } else {
        let template = vm.to_display_string(replacement);
        Ok(expand_replacement_template(
            &template,
            matched,
            before,
            after,
            &[],
        ))
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
        let chars: Vec<char> = s.chars().collect();
        let mut out = String::new();
        let mut pos = 0usize;
        let mut search_from = 0usize;
        loop {
            if search_from > chars.len() {
                break;
            }
            let Some(mat) = re
                .search(&chars, search_from)
                .map_err(|e| regex_err(vm, e))?
            else {
                break;
            };
            out.push_str(&chars[pos..mat.start].iter().collect::<String>());
            let matched: String = chars[mat.start..mat.end].iter().collect();
            let rep = compute_replacement(vm, replacement, &chars, &mat, &matched, s)?;
            out.push_str(&rep);
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
        out.push_str(&chars[pos..].iter().collect::<String>());
        Ok(Value::str(out))
    } else {
        let needle = vm.to_display_string(pattern);
        if force_all {
            if needle.is_empty() {
                // 空文字列パターンの replaceAll は各文字の間（先頭・末尾含む）に挿入する。
                let mut out = String::new();
                let chars: Vec<char> = s.chars().collect();
                for (i, c) in chars.iter().enumerate() {
                    let before: String = chars[..i].iter().collect();
                    let after: String = chars[i..].iter().collect();
                    out.push_str(&compute_replacement_str(
                        vm,
                        replacement,
                        "",
                        &before,
                        &after,
                        s,
                    )?);
                    out.push(*c);
                }
                let before: String = chars.iter().collect();
                out.push_str(&compute_replacement_str(
                    vm,
                    replacement,
                    "",
                    &before,
                    "",
                    s,
                )?);
                return Ok(Value::str(out));
            }
            let mut out = String::new();
            let mut rest = s;
            let mut consumed = String::new();
            loop {
                match rest.find(&needle) {
                    Some(idx) => {
                        out.push_str(&rest[..idx]);
                        consumed.push_str(&rest[..idx]);
                        let after = &rest[idx + needle.len()..];
                        let rep =
                            compute_replacement_str(vm, replacement, &needle, &consumed, after, s)?;
                        out.push_str(&rep);
                        consumed.push_str(&rest[idx..idx + needle.len()]);
                        rest = after;
                    }
                    None => {
                        out.push_str(rest);
                        break;
                    }
                }
            }
            Ok(Value::str(out))
        } else {
            match s.find(&needle) {
                Some(idx) => {
                    let before = &s[..idx];
                    let after = &s[idx + needle.len()..];
                    let rep = compute_replacement_str(vm, replacement, &needle, before, after, s)?;
                    Ok(Value::str(format!("{before}{rep}{after}")))
                }
                None => Ok(Value::str(s.to_string())),
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
    let chars: Vec<char> = s.chars().collect();
    if chars.is_empty() {
        let mut out = vec![];
        if re
            .search(&chars, 0)
            .map_err(|e| regex_err(vm, e))?
            .is_none()
        {
            out.push(Value::str(String::new()));
        }
        out.truncate(limit);
        return Ok(out);
    }
    let mut out: Vec<Value> = vec![];
    let mut p = 0usize;
    let mut search_from = 0usize;
    loop {
        if out.len() >= limit || search_from > chars.len() {
            break;
        }
        let Some(mat) = re
            .search(&chars, search_from)
            .map_err(|e| regex_err(vm, e))?
        else {
            break;
        };
        if mat.start >= chars.len() {
            break;
        }
        if mat.end == mat.start && mat.start == p {
            // 直前の分割位置と同じ場所での空幅マッチはスキップする
            // （そうしないと同じ位置で無限に分割してしまう）。
            search_from = mat.start + 1;
            continue;
        }
        out.push(Value::str(chars[p..mat.start].iter().collect::<String>()));
        for g in &mat.groups {
            if out.len() >= limit {
                break;
            }
            out.push(match g {
                Some((a, b)) => Value::str(chars[*a..*b].iter().collect::<String>()),
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
    if out.len() < limit {
        out.push(Value::str(chars[p..].iter().collect::<String>()));
    }
    out.truncate(limit);
    Ok(out)
}
