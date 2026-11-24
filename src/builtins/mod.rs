//! 組み込みオブジェクト・グローバル関数群。
//!
//! Noiria コーパス（`runtime/kag/parser.ts` / `runtime/kag/vars.ts`）が
//! 使う分＋要求仕様に挙げられた分だけを実装する。汎用 JS エンジンを
//! 目指すものではないため、対応外の呼び出しは「明確なエラー」を返す
//! （黙って undefined を返したりしない）方針を優先する。

mod array;
mod collections;
mod globals;
mod json;
mod promise;
mod regex;
mod string;

use std::borrow::Cow;
use std::cell::RefCell;
use std::rc::Rc;

use crate::regex::RegexScratch;
use crate::value::Value;
use crate::vm::{to_number, to_string_basic, KindTag, Vm};

pub use string::string_get_prop;

thread_local! {
    static REGEX_SCRATCH: RefCell<RegexScratch> = RefCell::new(RegexScratch::default());
    static CHARS_SCRATCH: RefCell<Vec<char>> = const { RefCell::new(Vec::new()) };
}

#[inline]
pub(super) fn value_as_cow_str(v: &Value) -> Cow<'_, str> {
    match v {
        Value::Str(s) => Cow::Borrowed(s),
        other => Cow::Owned(to_string_basic(other)),
    }
}

pub(super) fn with_regex_scratch<R>(f: impl FnOnce(&mut RegexScratch) -> R) -> R {
    REGEX_SCRATCH.with(|cell| {
        if let Ok(mut guard) = cell.try_borrow_mut() {
            f(&mut guard)
        } else {
            let mut local = RegexScratch::default();
            f(&mut local)
        }
    })
}

pub(super) fn with_chars_buf<R>(s: &str, f: impl FnOnce(&[char]) -> R) -> R {
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

#[inline]
pub(super) fn arg(args: &[Value], i: usize) -> Value {
    args.get(i).cloned().unwrap_or(Value::Undefined)
}

#[inline]
pub(super) fn arg_ref(args: &[Value], i: usize) -> &Value {
    args.get(i).unwrap_or(&Value::Undefined)
}

pub(super) fn native(
    name: &str,
    arity: usize,
    f: impl Fn(&mut Vm, Value, &[Value]) -> Result<Value, Value> + 'static,
) -> Value {
    Value::native_fn(name, arity, Rc::new(f))
}

#[inline]
pub(super) fn norm_index(v: Option<&Value>, default: i64, len: i64) -> i64 {
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

#[inline]
pub(super) fn chars_slice_to_val(chars: &[char]) -> Value {
    Value::Str(crate::vm::with_str_buf_rc(|buf| buf.extend(chars)))
}

#[inline]
pub(super) fn slice_to_val(src: &str, chars: &[char], start: usize, end: usize) -> Value {
    if src.is_ascii() {
        Value::str(&src[start..end])
    } else {
        chars_slice_to_val(&chars[start..end])
    }
}

pub fn install(vm: &mut Vm) {
    globals::install_console(vm);
    globals::install_object(vm);
    globals::install_math(vm);
    json::install_json(vm);
    globals::install_number(vm);
    globals::install_globals_misc(vm);
    collections::install_map_set_ctors(vm);
    globals::install_errors(vm);
    globals::install_date(vm);
    promise::install_promise(vm);
    regex::install_regexp(vm);
}

pub fn method_for(tag: KindTag, name: &str) -> Option<Value> {
    match tag {
        KindTag::Array => array::array_method(name),
        KindTag::Str => string::string_method(name),
        KindTag::Map => collections::map_method(name),
        KindTag::Set => collections::set_method(name),
        KindTag::Regex => regex::regex_method(name),
        KindTag::Promise => promise::promise_method(name),
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
        KindTag::Array => array::call_array_method(vm, this, name, args),
        KindTag::Str => string::call_string_method(vm, this, name, args),
        KindTag::Map => collections::call_map_method(vm, this, name, args),
        KindTag::Set => collections::call_set_method(vm, this, name, args),
        KindTag::Regex => regex::call_regex_method(vm, this, name, args),
        KindTag::Promise => promise::call_promise_method(vm, this, name, args),
    }
}
