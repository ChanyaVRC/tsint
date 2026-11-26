//! `String.prototype` プロパティ・メソッド群。

use std::borrow::Cow;
use std::rc::Rc;

use super::regex::{
    as_regex, build_match_array_from_scratch, coerce_to_regex, next_search_pos, regex_err,
    regex_split, string_replace,
};
use super::{
    lookup_method, norm_index, slice_to_val, value_as_cow_str, with_chars_buf, with_regex_scratch,
};
use crate::value::*;
use crate::vm::{ascii_char_rc, to_number, to_string_basic, with_str_buf_rc, Vm, MAX_STRING_BYTES};

const STRING_METHODS: &[(&str, usize)] = &[
    ("slice", 2),
    ("split", 2),
    ("trim", 0),
    ("startsWith", 1),
    ("endsWith", 1),
    ("includes", 1),
    ("indexOf", 1),
    ("replace", 2),
    ("replaceAll", 2),
    ("match", 1),
    ("search", 1),
    ("toLowerCase", 0),
    ("toUpperCase", 0),
    ("charCodeAt", 1),
    ("codePointAt", 1),
    ("repeat", 1),
    ("padStart", 2),
    ("padEnd", 2),
];

#[inline]
fn str_char_len(s: &str) -> usize {
    if s.is_ascii() {
        s.len()
    } else {
        s.chars().count()
    }
}

#[inline]
fn char_pos_to_byte_offset(s: &str, char_pos: usize) -> usize {
    if s.is_ascii() {
        char_pos.min(s.len())
    } else {
        s.char_indices().nth(char_pos).map_or(s.len(), |(bi, _)| bi)
    }
}

#[inline]
fn clamp_str_pos(arg: Option<&Value>, default: usize, max: usize) -> usize {
    match arg {
        None | Some(Value::Undefined) => default,
        Some(v) => {
            let n = to_number(v);
            if n.is_nan() || n <= 0.0 {
                0
            } else if n >= max as f64 {
                max
            } else {
                n as usize
            }
        }
    }
}

#[inline]
fn str_code_at(s: &str, idx: usize) -> Option<f64> {
    if s.is_ascii() {
        s.as_bytes().get(idx).map(|&b| b as f64)
    } else {
        s.chars().nth(idx).map(|c| c as u32 as f64)
    }
}

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

pub(super) fn string_method(name: &str) -> Option<Value> {
    lookup_method(STRING_METHODS, name, call_string_method)
}

pub(super) fn call_string_method(
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
                Some(sep) => {
                    if let Some(re) = as_regex(sep) {
                        let items = regex_split(vm, &re, &s, limit)?;
                        return Ok(vm.alloc_array(items));
                    }
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
            let matched = match args.get(1) {
                None | Some(Value::Undefined) => s.starts_with(p.as_ref()),
                pos_arg => {
                    let len = str_char_len(&s);
                    let start_char = clamp_str_pos(pos_arg, 0, len);
                    let byte_start = char_pos_to_byte_offset(&s, start_char);
                    s[byte_start..].starts_with(p.as_ref())
                }
            };
            Ok(Value::Bool(matched))
        }
        "endsWith" => {
            let s = value_as_cow_str(&this);
            let p = value_as_cow_str(args.first().unwrap_or(&Value::Undefined));
            let matched = match args.get(1) {
                None | Some(Value::Undefined) => s.ends_with(p.as_ref()),
                pos_arg => {
                    let len = str_char_len(&s);
                    let end_char = clamp_str_pos(pos_arg, len, len);
                    let byte_end = char_pos_to_byte_offset(&s, end_char);
                    s[..byte_end].ends_with(p.as_ref())
                }
            };
            Ok(Value::Bool(matched))
        }
        "includes" => {
            let s = value_as_cow_str(&this);
            let p = value_as_cow_str(args.first().unwrap_or(&Value::Undefined));
            let matched = match args.get(1) {
                None | Some(Value::Undefined) => s.contains(p.as_ref()),
                pos_arg => {
                    let len = str_char_len(&s);
                    let start_char = clamp_str_pos(pos_arg, 0, len);
                    let byte_start = char_pos_to_byte_offset(&s, start_char);
                    s[byte_start..].contains(p.as_ref())
                }
            };
            Ok(Value::Bool(matched))
        }
        "indexOf" => {
            let s = value_as_cow_str(&this);
            let p = value_as_cow_str(args.first().unwrap_or(&Value::Undefined));
            let (start_char, byte_start) = match args.get(1) {
                None | Some(Value::Undefined) => (0usize, 0usize),
                pos_arg => {
                    let len = str_char_len(&s);
                    let sc = clamp_str_pos(pos_arg, 0, len);
                    (sc, char_pos_to_byte_offset(&s, sc))
                }
            };
            if p.is_empty() {
                Ok(Value::Number(start_char as f64))
            } else if let Some(rel_byte) = s[byte_start..].find(p.as_ref()) {
                let char_off = if s.is_ascii() {
                    rel_byte
                } else {
                    s[byte_start..byte_start + rel_byte].chars().count()
                };
                Ok(Value::Number((start_char + char_off) as f64))
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
                        while from <= chars.len() {
                            match re
                                .find_with_scratch(chars, from, scratch)
                                .map_err(|e| regex_err(vm, e))?
                            {
                                Some((start, end)) => {
                                    out.push(slice_to_val(&s, chars, start, end));
                                    from = next_search_pos(start, end);
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
                    with_regex_scratch(|scratch| {
                        match re
                            .find_with_scratch(chars, 0, scratch)
                            .map_err(|e| regex_err(vm, e))?
                        {
                            Some((start, end)) => Ok(build_match_array_from_scratch(
                                vm,
                                &s,
                                chars,
                                start,
                                end,
                                re.ncaptures(),
                                scratch,
                            )),
                            None => Ok(Value::Null),
                        }
                    })
                }
            })
        })(),
        "search" => (|| {
            let s = value_as_cow_str(&this);
            let re = coerce_to_regex(vm, args.first().unwrap_or(&Value::Undefined))?;
            with_chars_buf(&s, |chars| {
                match with_regex_scratch(|scratch| re.find_with_scratch(chars, 0, scratch))
                    .map_err(|e| regex_err(vm, e))?
                {
                    Some((start, _)) => Ok(Value::Number(start as f64)),
                    None => Ok(Value::Number(-1.0)),
                }
            })
        })(),
        "toLowerCase" => Ok(Value::str(value_as_cow_str(&this).to_lowercase())),
        "toUpperCase" => Ok(Value::str(value_as_cow_str(&this).to_uppercase())),
        "charCodeAt" | "codePointAt" => {
            let s = value_as_cow_str(&this);
            let idx = to_number(args.first().unwrap_or(&Value::Undefined)) as usize;
            let code = str_code_at(&s, idx);
            Ok(match (name == "codePointAt", code) {
                (_, Some(n)) => Value::Number(n),
                (true, None) => Value::Undefined,
                (false, None) => Value::Number(f64::NAN),
            })
        }
        "repeat" => {
            let s = value_as_cow_str(&this);
            let n = to_number(args.first().unwrap_or(&Value::Undefined));
            if n < 0.0 || n.is_infinite() {
                Err(vm.range_error("repeat count は非負の有限数である必要があります"))
            } else {
                let count = if n.is_nan() {
                    0usize
                } else if n > (MAX_STRING_BYTES as f64) {
                    if s.is_empty() {
                        0usize
                    } else {
                        return Some(Err(vm.range_error(format!(
                            "repeat の結果文字列長が上限 ({MAX_STRING_BYTES}B) を超えています"
                        ))));
                    }
                } else {
                    n as usize
                };
                let total_bytes = s.len().saturating_mul(count);
                if total_bytes > MAX_STRING_BYTES {
                    Err(vm.range_error(format!(
                        "repeat の結果文字列長が上限 ({MAX_STRING_BYTES}B) を超えています"
                    )))
                } else {
                    Ok(Value::Str(with_str_buf_rc(|out| {
                        out.reserve(total_bytes);
                        for _ in 0..count {
                            out.push_str(&s);
                        }
                    })))
                }
            }
        }
        "padStart" | "padEnd" => {
            let is_start = name == "padStart";
            let s = value_as_cow_str(&this);
            let target_n = to_number(args.first().unwrap_or(&Value::Undefined));
            let pad = match args.get(1) {
                Some(Value::Undefined) | None => Cow::Borrowed(" "),
                Some(v) => value_as_cow_str(v),
            };
            let cur_len = str_char_len(&s);
            if target_n.is_nan() || target_n <= cur_len as f64 || pad.is_empty() {
                if let Value::Str(rc) = &this {
                    Ok(Value::Str(rc.clone()))
                } else {
                    Ok(Value::str(s.as_ref()))
                }
            } else if target_n.is_infinite() || target_n > (MAX_STRING_BYTES as f64) {
                Err(vm.range_error(format!(
                    "{name} の結果文字列長が上限 ({MAX_STRING_BYTES}B) を超えています"
                )))
            } else {
                let target_len = target_n as usize;
                let need = target_len - cur_len;
                let pad_chars: Vec<char> = pad.chars().collect();
                let full_reps = need / pad_chars.len();
                let rem_chars = need % pad_chars.len();
                let rem_bytes: usize = pad_chars[..rem_chars].iter().map(|c| c.len_utf8()).sum();
                let pad_total_bytes = full_reps
                    .saturating_mul(pad.len())
                    .saturating_add(rem_bytes);
                let result_bytes = s.len().saturating_add(pad_total_bytes);
                if result_bytes > MAX_STRING_BYTES {
                    Err(vm.range_error(format!(
                        "{name} の結果文字列長が上限 ({MAX_STRING_BYTES}B) を超えています"
                    )))
                } else {
                    Ok(Value::Str(with_str_buf_rc(|out| {
                        out.reserve(result_bytes);
                        if !is_start {
                            out.push_str(&s);
                        }
                        for _ in 0..full_reps {
                            out.push_str(&pad);
                        }
                        if rem_chars > 0 {
                            out.extend(&pad_chars[..rem_chars]);
                        }
                        if is_start {
                            out.push_str(&s);
                        }
                    })))
                }
            }
        }
        _ => return None,
    })
}
