//! グローバルオブジェクト・基本組み込みコンストラクタ（console / Object / Math / Number / Date / Error 等）。

use std::cell::RefCell;
use std::rc::Rc;

use super::{arg, native};
use crate::value::*;
use crate::vm::{own_enumerable_keys, to_number, Vm};

// =====================================================================
// console
// =====================================================================

pub(super) fn install_console(vm: &mut Vm) {
    let console = Value::new_plain_object();
    let log = native("log", 0, |vm, _this, args| {
        let parts: Vec<String> = args.iter().map(|a| vm.to_display_string(a)).collect();
        let line = parts.join(" ");
        (vm.on_log)(&line);
        Ok(Value::Undefined)
    });
    if let Value::Object(o) = &console {
        let mut b = o.borrow_mut();
        b.props.set_value(Rc::from("log"), log.clone());
        b.props.set_value(Rc::from("error"), log.clone());
        b.props.set_value(Rc::from("warn"), log.clone());
        b.props.set_value(Rc::from("info"), log);
    }
    vm.define_global("console", console);
}

// =====================================================================
// Object
// =====================================================================

pub(super) fn install_object(vm: &mut Vm) {
    let object_global = Value::new_plain_object();
    let keys = native("keys", 1, |_vm, _this, args| {
        Ok(Value::new_array(own_enumerable_keys(&arg(args, 0))))
    });
    let values = native("values", 1, |_vm, _this, args| {
        Ok(Value::new_array(own_enumerable_values(&arg(args, 0))))
    });
    let entries = native("entries", 1, |vm, _this, args| {
        let obj = arg(args, 0);
        let pairs = own_enumerable_entries(vm, &obj);
        Ok(vm.alloc_array(pairs))
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

fn own_enumerable_entries(vm: &Vm, v: &Value) -> Vec<Value> {
    match v {
        Value::Object(o) => {
            let b = o.borrow();
            match &b.kind {
                ObjKind::Array(items) => items
                    .iter()
                    .enumerate()
                    .map(|(i, val)| vm.alloc_array(vec![Value::str(i.to_string()), val.clone()]))
                    .collect(),
                _ => b
                    .props
                    .iter()
                    .filter_map(|(k, s)| match s {
                        PropSlot::Value(val) => {
                            Some(vm.alloc_array(vec![Value::Str(k.clone()), val.clone()]))
                        }
                        PropSlot::Accessor { .. } => None,
                    })
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

pub(super) fn install_math(vm: &mut Vm) {
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
// Number / parseInt / parseFloat / Array / Function / Error
// =====================================================================

pub(super) fn install_number(vm: &mut Vm) {
    let number = native("Number", 1, |_vm, _this, args| {
        Ok(Value::Number(to_number(&arg(args, 0))))
    });
    let is_finite = native("isFinite", 1, |_vm, _this, args| {
        Ok(Value::Bool(
            matches!(arg(args, 0), Value::Number(n) if n.is_finite()),
        ))
    });
    if let Value::Object(o) = &number {
        let mut b = o.borrow_mut();
        b.props.set_value(Rc::from("isFinite"), is_finite);
        b.props.set_value(Rc::from("NaN"), Value::Number(f64::NAN));
        b.props
            .set_value(Rc::from("POSITIVE_INFINITY"), Value::Number(f64::INFINITY));
        b.props.set_value(
            Rc::from("NEGATIVE_INFINITY"),
            Value::Number(f64::NEG_INFINITY),
        );
    }
    vm.define_global("Number", number);
    vm.define_global("NaN", Value::Number(f64::NAN));
    vm.define_global("Infinity", Value::Number(f64::INFINITY));
    vm.define_global("undefined", Value::Undefined);
    vm.define_global("globalThis", Value::new_plain_object());
}

pub(super) fn install_globals_misc(vm: &mut Vm) {
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

pub(super) fn install_errors(vm: &mut Vm) {
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
// Date（現在時刻はホスト注入。VM 自体は決定的に保つ）
// =====================================================================

pub(super) fn install_date(vm: &mut Vm) {
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
    {
        let mut b = proto.borrow_mut();
        b.props.set_value(Rc::from("getTime"), get_time);
        b.props
            .set_value(Rc::from("toLocaleString"), to_locale_string.clone());
        b.props.set_value(Rc::from("toString"), to_locale_string);
    }

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
            let mut inst = ObjData {
                kind: ObjKind::Plain,
                props: PropMap::new(),
                proto: Some(proto_for_ctor.clone()),
            };
            inst.props.set_value(Rc::from("%time"), Value::Number(t));
            Ok(Value::Object(Rc::new(RefCell::new(inst))))
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
