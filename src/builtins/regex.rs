//! `RegExp` コンストラクタ・プロトタイプメソッド・文字列正規表現操作（replace / split / match 等）。

use std::rc::Rc;

use super::{chars_slice_to_val, native, value_as_cow_str, with_chars_buf, with_regex_scratch};
use crate::regex::{RegexMatch, RegexScratch};
use crate::value::*;
use crate::vm::{is_callable, to_number, Vm};

thread_local! {
    static KEY_LAST_INDEX: Rc<str> = Rc::from("lastIndex");
    static KEY_INDEX: Rc<str> = Rc::from("index");
    static KEY_INPUT: Rc<str> = Rc::from("input");
    static KEY_GROUPS: Rc<str> = Rc::from("groups");
}

/// `new RegExp(...)` / `RegExp(...)` の実装。
pub(super) fn install_regexp(vm: &mut Vm) {
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

/// 文字列引数またはコンパイル済み正規表現を `Rc<Regex>` に統一する。
pub(super) fn coerce_to_regex(vm: &mut Vm, v: &Value) -> Result<Rc<crate::regex::Regex>, Value> {
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

pub(super) fn regex_err(vm: &Vm, e: crate::regex::RegexError) -> Value {
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

/// `RegExp.prototype.exec` の戻り値の配列（`index`/`input`/`groups` プロパティ付き）を組み立てる。
pub(super) fn build_match_array(vm: &Vm, chars: &[char], mat: &RegexMatch, input: &str) -> Value {
    let mut items = Vec::with_capacity(1 + mat.groups.len());
    items.push(chars_slice_to_val(&chars[mat.start..mat.end]));
    for g in &mat.groups {
        items.push(match g {
            Some((a, b)) => chars_slice_to_val(&chars[*a..*b]),
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
    on_result: impl FnOnce(&[char], Option<RegexMatch>) -> R,
) -> Result<R, Value> {
    with_chars_buf(s, |chars| {
        let from = if re.global { regex_last_index(o) } else { 0 };
        if from > chars.len() {
            if re.global {
                set_last_index(o, 0);
            }
            return Ok(on_result(chars, None));
        }
        let mat = with_regex_scratch(|scratch| re.search_with_scratch(chars, from, scratch))
            .map_err(|e| regex_err(vm, e))?;
        if re.global {
            set_last_index(o, mat.as_ref().map_or(0, |m| m.end));
        }
        Ok(on_result(chars, mat))
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
            exec_regex_stateful(vm, o, &re, &s, |_, mat| Value::Bool(mat.is_some()))
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
            exec_regex_stateful(vm, o, &re, &s, |chars, mat| match mat {
                Some(m) => build_match_array(vm, chars, &m, &s),
                None => Value::Null,
            })
        })(),
        "toString" => Ok(Value::str(vm.to_display_string(&this))),
        _ => return None,
    })
}

// ---------------- String.prototype.replace(All) の置換テンプレート ----------------

/// `$&` `` $` `` `$'` `$1`〜 `$$` を `&mut String` へ直接展開する（正規表現マッチ版）。
fn expand_regex_template_into(template: &str, chars: &[char], mat: &RegexMatch, out: &mut String) {
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
    mat: &RegexMatch,
    whole_input: &str,
    out: &mut String,
) -> Result<(), Value> {
    let mut cb_args = Vec::with_capacity(3 + mat.groups.len());
    cb_args.push(chars_slice_to_val(&chars[mat.start..mat.end]));
    for g in &mat.groups {
        cb_args.push(match g {
            Some((a, b)) => chars_slice_to_val(&chars[*a..*b]),
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

/// `String.prototype.replace`/`replaceAll` 本体。
pub(super) fn string_replace(
    vm: &mut Vm,
    s: &str,
    pattern: &Value,
    replacement: &Value,
    force_all: bool,
) -> Result<Value, Value> {
    if let Value::Object(o) = pattern {
        let maybe_re = match &o.borrow().kind {
            ObjKind::Regex(r) => Some(r.clone()),
            _ => None,
        };
        if let Some(re) = maybe_re {
            return string_replace_regex(vm, s, &re, replacement, force_all || re.global);
        }
    }
    string_replace_literal(vm, s, pattern, replacement, force_all)
}

/// ゼロ幅一致時に無限ループを避けつつ次の検索開始位置を返す。
#[inline]
pub(super) fn next_search_pos(mat: &RegexMatch) -> usize {
    if mat.end == mat.start {
        mat.end + 1
    } else {
        mat.end
    }
}

fn run_regex_replace_loop(
    vm: &mut Vm,
    re: &crate::regex::Regex,
    chars: &[char],
    scratch: &mut RegexScratch,
    global: bool,
    out: &mut String,
    mut on_match: impl FnMut(&mut Vm, &RegexMatch, &mut String) -> Result<(), Value>,
) -> Result<(), Value> {
    let mut pos = 0usize;
    let mut search_from = 0usize;
    while search_from <= chars.len() {
        let Some(mat) = re
            .search_with_scratch(chars, search_from, scratch)
            .map_err(|e| regex_err(vm, e))?
        else {
            break;
        };
        out.extend(&chars[pos..mat.start]);
        on_match(vm, &mat, out)?;
        pos = mat.end;
        search_from = next_search_pos(&mat);
        if !global {
            break;
        }
    }
    out.extend(&chars[pos..]);
    Ok(())
}

fn string_replace_regex(
    vm: &mut Vm,
    s: &str,
    re: &crate::regex::Regex,
    replacement: &Value,
    global: bool,
) -> Result<Value, Value> {
    if !is_callable(replacement) {
        let template = value_as_cow_str(replacement);
        let has_dollar = template.contains('$');
        return with_chars_buf(s, |chars| {
            let mut out = String::with_capacity(s.len());
            with_regex_scratch(|scratch| {
                run_regex_replace_loop(vm, re, chars, scratch, global, &mut out, |_, mat, dst| {
                    if has_dollar {
                        expand_regex_template_into(&template, chars, mat, dst);
                    } else {
                        dst.push_str(&template);
                    }
                    Ok(())
                })
            })?;
            Ok(Value::str(out))
        });
    }
    // コールバック関数は再入し得るためローカル `Vec<char>` と `RegexScratch` を使う
    let chars: Vec<char> = s.chars().collect();
    let mut scratch = RegexScratch::default();
    let mut out = String::with_capacity(s.len());
    run_regex_replace_loop(
        vm,
        re,
        &chars,
        &mut scratch,
        global,
        &mut out,
        |vm, mat, dst| call_regex_replace_cb_into(vm, replacement, &chars, mat, s, dst),
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
                    let mut out = String::with_capacity(s.len() + template.len() * (s.len() + 1));
                    for c in s.chars() {
                        out.push_str(&template);
                        out.push(c);
                    }
                    out.push_str(&template);
                    return Ok(Value::str(out));
                }
                return Ok(Value::str(s.replace(needle.as_ref(), template.as_ref())));
            }
            return match s.find(needle.as_ref()) {
                Some(idx) => {
                    let mut out = String::with_capacity(s.len() - needle.len() + template.len());
                    out.push_str(&s[..idx]);
                    out.push_str(&template);
                    out.push_str(&s[idx + needle.len()..]);
                    Ok(Value::str(out))
                }
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
            while out.len() < limit && search_from <= chars.len() {
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
                out.push(chars_slice_to_val(&chars[p..mat.start]));
                for g in &mat.groups {
                    if out.len() >= limit {
                        break;
                    }
                    out.push(match g {
                        Some((a, b)) => chars_slice_to_val(&chars[*a..*b]),
                        None => Value::Undefined,
                    });
                }
                p = mat.end;
                search_from = next_search_pos(&mat);
            }
            Ok(())
        })?;
        if out.len() < limit {
            out.push(chars_slice_to_val(&chars[p..]));
        }
        out.truncate(limit);
        Ok(out)
    })
}
