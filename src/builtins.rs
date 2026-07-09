//! 組み込みオブジェクト・グローバル関数群。
//!
//! Noiria コーパス（`runtime/kag/parser.ts` / `runtime/kag/vars.ts`）が
//! 使う分＋要求仕様に挙げられた分だけを実装する。汎用 JS エンジンを
//! 目指すものではないため、対応外の呼び出しは「明確なエラー」を返す
//! （黙って undefined を返したりしない）方針を優先する。

use std::cell::RefCell;
use std::rc::Rc;

use crate::value::*;
use crate::vm::{array_values, number_to_js_string, to_number, KindTag, Vm};

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
// Array.prototype / String.prototype / Map.prototype / Set.prototype / RegExp
// =====================================================================

pub fn method_for(tag: KindTag, name: &str) -> Option<Value> {
    match tag {
        KindTag::Array => array_method(name),
        KindTag::Str => string_method(name),
        KindTag::Map => map_method(name),
        KindTag::Set => set_method(name),
        KindTag::Regex => regex_method(name),
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
        "split" => native("split", 1, |vm, this, args| {
            let s = vm.to_display_string(&this);
            match args.first() {
                None | Some(Value::Undefined) => Ok(Value::new_array(vec![Value::str(s)])),
                Some(Value::Object(o)) if matches!(o.borrow().kind, ObjKind::Regex(_)) => {
                    let re = match &o.borrow().kind {
                        ObjKind::Regex(r) => r.clone(),
                        _ => unreachable!(),
                    };
                    let parts = re.split(&s);
                    Ok(Value::new_array(
                        parts.into_iter().map(Value::str).collect(),
                    ))
                }
                Some(sep) => {
                    let sep_s = vm.to_display_string(sep);
                    if sep_s.is_empty() {
                        return Ok(Value::new_array(
                            s.chars().map(|c| Value::str(c.to_string())).collect(),
                        ));
                    }
                    Ok(Value::new_array(
                        s.split(sep_s.as_str()).map(Value::str).collect(),
                    ))
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
            let from = vm.to_display_string(&arg(args, 0));
            let to = vm.to_display_string(&arg(args, 1));
            Ok(Value::str(s.replacen(&from, &to, 1)))
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
        "test" => return regex_method("test"),
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

// ---------------- RegExp（最小サブセット） ----------------

fn regex_method(name: &str) -> Option<Value> {
    Some(match name {
        "test" => native("test", 1, |vm, this, args| {
            let s = vm.to_display_string(&arg(args, 0));
            if let Value::Object(o) = &this {
                if let ObjKind::Regex(re) = &o.borrow().kind {
                    return Ok(Value::Bool(re.test(&s)));
                }
            }
            Ok(Value::Bool(false))
        }),
        _ => return None,
    })
}
