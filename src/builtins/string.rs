//! `String.prototype` プロパティ・メソッド群。

use std::borrow::Cow;
use std::rc::Rc;

use super::regex::{
    as_regex, build_match_array_from_scratch, coerce_to_regex, next_search_pos, regex_err,
    regex_split, string_replace,
};
use super::{
    native, norm_index, slice_to_val, value_as_cow_str, with_chars_buf, with_regex_scratch,
};
use crate::value::*;
use crate::vm::{ascii_char_rc, to_number, to_string_basic, with_str_buf_rc, Vm};

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
                let count = n as usize;
                Ok(Value::Str(with_str_buf_rc(|out| {
                    out.reserve(s.len().saturating_mul(count));
                    for _ in 0..count {
                        out.push_str(&s);
                    }
                })))
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
                let pad_chars: Vec<char> = pad.chars().collect();
                Ok(Value::Str(with_str_buf_rc(|out| {
                    out.reserve(need + s.len());
                    let mut written = 0usize;
                    while written < need {
                        let take = (need - written).min(pad_chars.len());
                        out.extend(&pad_chars[..take]);
                        written += take;
                    }
                    out.push_str(&s);
                })))
            }
        }
        _ => return None,
    })
}
