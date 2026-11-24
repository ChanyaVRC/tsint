//! `RegExp` コンストラクタ・プロトタイプメソッド・文字列正規表現操作（replace / split / match 等）。

use std::rc::Rc;

use super::{native, slice_to_val, value_as_cow_str, with_chars_buf, with_regex_scratch};
use crate::regex::RegexScratch;
use crate::value::*;
use crate::vm::{is_callable, to_number, with_str_buf_rc, Vm};

thread_local! {
    static KEY_LAST_INDEX: Rc<str> = Rc::from("lastIndex");
    static KEY_INDEX: Rc<str> = Rc::from("index");
    static KEY_INPUT: Rc<str> = Rc::from("input");
    static KEY_GROUPS: Rc<str> = Rc::from("groups");
}

/// `Value` が `ObjKind::Regex` であればコンパイル済み正規表現の `Rc` を返す。
#[inline]
pub(super) fn as_regex(v: &Value) -> Option<Rc<crate::regex::Regex>> {
    if let Value::Object(o) = v {
        if let ObjKind::Regex(r) = &o.borrow().kind {
            return Some(r.clone());
        }
    }
    None
}

/// `new RegExp(...)` / `RegExp(...)` の実装。
pub(super) fn install_regexp(vm: &mut Vm) {
    vm.register_native(
        "RegExp",
        2,
        Rc::new(|vm, _this, args| {
            let (pattern, default_flags) = match args.first() {
                Some(v) => match as_regex(v) {
                    Some(re) => (re.source.to_string(), re.flags.to_string()),
                    None => (vm.to_display_string(v), String::new()),
                },
                None => (String::new(), String::new()),
            };
            let flags = match args.get(1) {
                None | Some(Value::Undefined) => default_flags,
                Some(v) => vm.to_display_string(v),
            };
            match crate::regex::Regex::compile(&pattern, &flags) {
                Ok(re) => Ok(vm.alloc_regex_value(Rc::new(re))),
                Err(e) => Err(vm.syntax_error(format!("不正な正規表現です: {e}"))),
            }
        }),
    );
}

/// 文字列引数またはコンパイル済み正規表現を `Rc<Regex>` に統一する。
pub(super) fn coerce_to_regex(vm: &mut Vm, v: &Value) -> Result<Rc<crate::regex::Regex>, Value> {
    if let Some(re) = as_regex(v) {
        return Ok(re);
    }
    let pattern = vm.to_display_string(v);
    crate::regex::Regex::compile(&pattern, "")
        .map(Rc::new)
        .map_err(|e| vm.syntax_error(format!("不正な正規表現です: {e}")))
}

pub(super) fn regex_err(vm: &Vm, e: crate::regex::RegexError) -> Value {
    vm.range_error(format!("正規表現の実行中にエラーが発生しました: {e}"))
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
    let mut b = o.borrow_mut();
    if n == 0 && b.props.is_empty() {
        return;
    }
    let key = KEY_LAST_INDEX.with(|k| k.clone());
    b.props.set_value(key, Value::Number(n as f64));
}

/// `RegExp.prototype.exec` の戻り値の配列（`index`/`input`/`groups` プロパティ付き）を組み立てる。
pub(super) fn build_match_array_from_scratch(
    vm: &Vm,
    input: &str,
    chars: &[char],
    start: usize,
    end: usize,
    ncaptures: usize,
    scratch: &RegexScratch,
) -> Value {
    let mut items = Vec::with_capacity(1 + ncaptures);
    items.push(slice_to_val(input, chars, start, end));
    for gi in 1..=ncaptures {
        items.push(match scratch.group(gi) {
            Some((a, b)) => slice_to_val(input, chars, a, b),
            None => Value::Undefined,
        });
    }
    let mut obj = ObjData::with_props_cap(ObjKind::Array(items), 3);
    obj.props
        .set_value(KEY_INDEX.with(|k| k.clone()), Value::Number(start as f64));
    obj.props
        .set_value(KEY_INPUT.with(|k| k.clone()), Value::str(input));
    obj.props
        .set_value(KEY_GROUPS.with(|k| k.clone()), Value::Undefined);
    vm.alloc_obj(obj)
}

/// `RegExp.prototype.test` / `exec` 共通のステートフル検索 (`lastIndex` 更新込み)。
fn exec_regex_stateful<R>(
    vm: &Vm,
    o: &ObjRef,
    re: &crate::regex::Regex,
    s: &str,
    on_result: impl FnOnce(&[char], Option<(usize, usize)>, &RegexScratch) -> R,
) -> Result<R, Value> {
    with_chars_buf(s, |chars| {
        let from = if re.global { regex_last_index(o) } else { 0 };
        with_regex_scratch(|scratch| {
            if from > chars.len() {
                if re.global {
                    set_last_index(o, 0);
                }
                return Ok(on_result(chars, None, scratch));
            }
            let mat = re
                .find_with_scratch(chars, from, scratch)
                .map_err(|e| regex_err(vm, e))?;
            if re.global {
                set_last_index(o, mat.map_or(0, |(_, end)| end));
            }
            Ok(on_result(chars, mat, scratch))
        })
    })
}

pub(super) fn regex_method(name: &str) -> Option<Value> {
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

pub(super) fn call_regex_method(
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
            exec_regex_stateful(vm, o, &re, &s, |_, mat, _| Value::Bool(mat.is_some()))
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
            exec_regex_stateful(vm, o, &re, &s, |chars, mat, scratch| match mat {
                Some((start, end)) => build_match_array_from_scratch(
                    vm,
                    &s,
                    chars,
                    start,
                    end,
                    re.ncaptures(),
                    scratch,
                ),
                None => Value::Null,
            })
        })(),
        "toString" => Ok(Value::str(vm.to_display_string(&this))),
        _ => return None,
    })
}

// ---------------- String.prototype.replace(All) の置換テンプレート ----------------

/// `src` の文字範囲 `[a..b]` を `out` へ追記する（ASCII は直接スライスコピー）。
#[inline]
fn push_char_range(out: &mut String, src: &str, chars: &[char], a: usize, b: usize) {
    if src.is_ascii() {
        out.push_str(&src[a..b]);
    } else {
        out.extend(&chars[a..b]);
    }
}

/// 置換テンプレート中の `$&` `` $` `` `$'` `$1`〜 `$$` を `&mut String` へ直接展開する共通実装。
fn expand_template_into(
    template: &str,
    ncaptures: usize,
    mut write_matched: impl FnMut(&mut String),
    mut write_before: impl FnMut(&mut String),
    mut write_after: impl FnMut(&mut String),
    mut write_group: impl FnMut(usize, &mut String),
    out: &mut String,
) {
    let bytes = template.as_bytes();
    let mut i = 0usize;
    let mut chunk_start = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() {
            let next = bytes[i + 1];
            match next {
                b'$' | b'&' | b'`' | b'\'' => {
                    out.push_str(&template[chunk_start..i]);
                    match next {
                        b'$' => out.push('$'),
                        b'&' => write_matched(out),
                        b'`' => write_before(out),
                        b'\'' => write_after(out),
                        _ => unreachable!(),
                    }
                    i += 2;
                    chunk_start = i;
                    continue;
                }
                b'0'..=b'9' if ncaptures > 0 => {
                    let one = (next - b'0') as usize;
                    let two = if i + 2 < bytes.len() && bytes[i + 2].is_ascii_digit() {
                        Some(one * 10 + (bytes[i + 2] - b'0') as usize)
                    } else {
                        None
                    };
                    if let Some(n2) = two.filter(|&n| n >= 1 && n <= ncaptures) {
                        out.push_str(&template[chunk_start..i]);
                        write_group(n2, out);
                        i += 3;
                        chunk_start = i;
                        continue;
                    }
                    if one >= 1 && one <= ncaptures {
                        out.push_str(&template[chunk_start..i]);
                        write_group(one, out);
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

/// 正規表現マッチ 1 件分のコールバック置換を実行して `out` へ追記する。
#[allow(clippy::too_many_arguments)]
fn call_regex_replace_cb_into(
    vm: &mut Vm,
    replacement: &Value,
    whole_input: &str,
    chars: &[char],
    start: usize,
    end: usize,
    ncaptures: usize,
    scratch: &RegexScratch,
    out: &mut String,
) -> Result<(), Value> {
    let mut cb_args = Vec::with_capacity(3 + ncaptures);
    cb_args.push(slice_to_val(whole_input, chars, start, end));
    for gi in 1..=ncaptures {
        cb_args.push(match scratch.group(gi) {
            Some((a, b)) => slice_to_val(whole_input, chars, a, b),
            None => Value::Undefined,
        });
    }
    cb_args.push(Value::Number(start as f64));
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
            expand_template_into(
                &template,
                0,
                |dst| dst.push_str(matched),
                |dst| dst.push_str(before),
                |dst| dst.push_str(after),
                |_, _| {},
                out,
            );
        } else {
            out.push_str(&template);
        }
        Ok(())
    }
}

/// `String.prototype.replace`/`replaceAll` 本体。
pub(super) fn string_replace(
    vm: &mut Vm,
    s: &str,
    pattern: &Value,
    replacement: &Value,
    force_all: bool,
) -> Result<Value, Value> {
    if let Some(re) = as_regex(pattern) {
        return string_replace_regex(vm, s, &re, replacement, force_all || re.global);
    }
    string_replace_literal(vm, s, pattern, replacement, force_all)
}

/// ゼロ幅一致時に無限ループを避けつつ次の検索開始位置を返す。
#[inline]
pub(super) fn next_search_pos(start: usize, end: usize) -> usize {
    if end == start {
        end + 1
    } else {
        end
    }
}

#[allow(clippy::too_many_arguments)]
fn run_regex_replace_loop(
    vm: &mut Vm,
    re: &crate::regex::Regex,
    src: &str,
    chars: &[char],
    scratch: &mut RegexScratch,
    global: bool,
    out: &mut String,
    mut on_match: impl FnMut(&mut Vm, usize, usize, &RegexScratch, &mut String) -> Result<(), Value>,
) -> Result<(), Value> {
    let mut pos = 0usize;
    let mut search_from = 0usize;
    while search_from <= chars.len() {
        let Some((start, end)) = re
            .find_with_scratch(chars, search_from, scratch)
            .map_err(|e| regex_err(vm, e))?
        else {
            break;
        };
        push_char_range(out, src, chars, pos, start);
        on_match(vm, start, end, scratch, out)?;
        pos = end;
        search_from = next_search_pos(start, end);
        if !global {
            break;
        }
    }
    push_char_range(out, src, chars, pos, chars.len());
    Ok(())
}

fn string_replace_regex(
    vm: &mut Vm,
    s: &str,
    re: &crate::regex::Regex,
    replacement: &Value,
    global: bool,
) -> Result<Value, Value> {
    let ncap = re.ncaptures();
    if !is_callable(replacement) {
        let template = value_as_cow_str(replacement);
        let has_dollar = template.contains('$');
        return with_chars_buf(s, |chars| {
            let mut err_res: Result<(), Value> = Ok(());
            let rc = with_str_buf_rc(|out| {
                out.reserve(s.len());
                err_res = with_regex_scratch(|scratch| {
                    run_regex_replace_loop(
                        vm,
                        re,
                        s,
                        chars,
                        scratch,
                        global,
                        out,
                        |_, start, end, sc, dst| {
                            if has_dollar {
                                expand_template_into(
                                    &template,
                                    ncap,
                                    |d| push_char_range(d, s, chars, start, end),
                                    |d| push_char_range(d, s, chars, 0, start),
                                    |d| push_char_range(d, s, chars, end, chars.len()),
                                    |gi, d| {
                                        if let Some((a, b)) = sc.group(gi) {
                                            push_char_range(d, s, chars, a, b);
                                        }
                                    },
                                    dst,
                                );
                            } else {
                                dst.push_str(&template);
                            }
                            Ok(())
                        },
                    )
                });
            });
            err_res?;
            Ok(Value::Str(rc))
        });
    }
    // コールバック関数は再入し得るためローカル `Vec<char>` と `RegexScratch` を使う
    let chars: Vec<char> = s.chars().collect();
    let mut scratch = RegexScratch::default();
    let mut out = String::with_capacity(s.len());
    run_regex_replace_loop(
        vm,
        re,
        s,
        &chars,
        &mut scratch,
        global,
        &mut out,
        |vm, start, end, sc, dst| {
            call_regex_replace_cb_into(vm, replacement, s, &chars, start, end, ncap, sc, dst)
        },
    )?;
    Ok(Value::str(out))
}

fn string_replace_literal(
    vm: &mut Vm,
    s: &str,
    pattern: &Value,
    replacement: &Value,
    force_all: bool,
) -> Result<Value, Value> {
    let needle = value_as_cow_str(pattern);
    // 最も頻出する「文字列リテラル置換かつ `$` 展開なし」の高速パス
    if !is_callable(replacement) {
        let template = value_as_cow_str(replacement);
        if !template.contains('$') {
            if force_all {
                if needle.is_empty() {
                    return Ok(Value::Str(with_str_buf_rc(|out| {
                        out.reserve(s.len() + template.len() * (s.len() + 1));
                        for c in s.chars() {
                            out.push_str(&template);
                            out.push(c);
                        }
                        out.push_str(&template);
                    })));
                }
                return Ok(Value::Str(with_str_buf_rc(|out| {
                    out.reserve(s.len());
                    let mut pos = 0usize;
                    while let Some(rel) = s[pos..].find(needle.as_ref()) {
                        let abs = pos + rel;
                        out.push_str(&s[pos..abs]);
                        out.push_str(&template);
                        pos = abs + needle.len();
                    }
                    out.push_str(&s[pos..]);
                })));
            }
            return match s.find(needle.as_ref()) {
                Some(idx) => Ok(Value::Str(with_str_buf_rc(|out| {
                    out.reserve(s.len() - needle.len() + template.len());
                    out.push_str(&s[..idx]);
                    out.push_str(&template);
                    out.push_str(&s[idx + needle.len()..]);
                }))),
                None => Ok(Value::str(s)),
            };
        }
    }
    if force_all {
        if needle.is_empty() {
            let mut out = String::with_capacity(s.len() * 2);
            for (byte_i, c) in s.char_indices() {
                append_replacement_str(
                    vm,
                    replacement,
                    "",
                    &s[..byte_i],
                    &s[byte_i..],
                    s,
                    &mut out,
                )?;
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
            append_replacement_str(
                vm,
                replacement,
                &needle,
                &s[..abs],
                &s[abs + needle.len()..],
                s,
                &mut out,
            )?;
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

/// `String.prototype.split(regex, limit)` 本体。
pub(super) fn regex_split(
    vm: &mut Vm,
    re: &crate::regex::Regex,
    s: &str,
    limit: usize,
) -> Result<Vec<Value>, Value> {
    let ncap = re.ncaptures();
    with_chars_buf(s, |chars| {
        if chars.is_empty() {
            let mut out = vec![];
            if with_regex_scratch(|scratch| re.find_with_scratch(chars, 0, scratch))
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
            while out.len() < limit && search_from <= chars.len() {
                let Some((start, end)) = re
                    .find_with_scratch(chars, search_from, scratch)
                    .map_err(|e| regex_err(vm, e))?
                else {
                    break;
                };
                if start >= chars.len() {
                    break;
                }
                if end == start && start == p {
                    search_from = start + 1;
                    continue;
                }
                out.push(slice_to_val(s, chars, p, start));
                for gi in 1..=ncap {
                    if out.len() >= limit {
                        break;
                    }
                    out.push(match scratch.group(gi) {
                        Some((a, b)) => slice_to_val(s, chars, a, b),
                        None => Value::Undefined,
                    });
                }
                p = end;
                search_from = next_search_pos(start, end);
            }
            Ok(())
        })?;
        if out.len() < limit {
            out.push(slice_to_val(s, chars, p, chars.len()));
        }
        out.truncate(limit);
        Ok(out)
    })
}
