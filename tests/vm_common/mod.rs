//! T2 (VM) テスト共通ヘルパー。
//!
//! 複数のテストバイナリ（vm_basics / vm_corpus / vm_programs）から
//! `mod.rs` として個別に取り込まれるため、バイナリごとに使わない関数が
//! あっても dead_code 警告にしない。

#![allow(dead_code)]

use std::cell::RefCell;
use std::rc::Rc;

use tsint::value::Value;
use tsint::Interpreter;

/// ソースを実行し、export された値の一覧を返す（失敗したら panic）。
pub fn run_ok(src: &str) -> Vec<(Rc<str>, Value)> {
    let mut interp = Interpreter::new();
    match interp.run_module(src, "test.ts") {
        Ok(exports) => exports,
        Err(e) => panic!("実行エラー:\n{e}\n--- ソース ---\n{src}"),
    }
}

/// 実行が失敗する（未捕捉の例外・パース/コンパイルエラー）ことを確認する。
pub fn run_err(src: &str) -> String {
    let mut interp = Interpreter::new();
    match interp.run_module(src, "test.ts") {
        Ok(_) => panic!("エラーになるはずが成功しました:\n{src}"),
        Err(e) => e.to_string(),
    }
}

/// console.log の出力行を集めながら実行する。
pub fn run_capture(src: &str) -> Vec<String> {
    let mut interp = Interpreter::new();
    let lines: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let lines2 = lines.clone();
    interp.set_console_hook(move |s: &str| lines2.borrow_mut().push(s.to_string()));
    match interp.run_module(src, "test.ts") {
        Ok(_) => {}
        Err(e) => panic!("実行エラー:\n{e}\n--- ソース ---\n{src}"),
    }
    // `interp`（＝ `on_log` クロージャ）がまだ `lines` の Rc を握っているので、
    // ここで明示的に破棄してから `try_unwrap` する。
    drop(interp);
    Rc::try_unwrap(lines).unwrap().into_inner()
}

pub fn export(exports: &[(Rc<str>, Value)], name: &str) -> Value {
    exports
        .iter()
        .find(|(n, _)| &**n == name)
        .unwrap_or_else(|| panic!("export '{name}' が見つかりません"))
        .1
        .clone()
}

pub fn as_num(v: &Value) -> f64 {
    match v {
        Value::Number(n) => *n,
        other => panic!("数値ではありません: {other:?}"),
    }
}

pub fn as_str(v: &Value) -> String {
    match v {
        Value::Str(s) => s.to_string(),
        other => panic!("文字列ではありません: {other:?}"),
    }
}

pub fn as_bool(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        other => panic!("真偽値ではありません: {other:?}"),
    }
}

pub fn is_undefined(v: &Value) -> bool {
    matches!(v, Value::Undefined)
}

pub fn is_null(v: &Value) -> bool {
    matches!(v, Value::Null)
}

/// `export const result = expr;` の形の 1 行を実行し、数値として取り出す。
pub fn eval_num(expr: &str) -> f64 {
    let src = format!("export const result = ({expr});");
    as_num(&export(&run_ok(&src), "result"))
}

pub fn eval_str(expr: &str) -> String {
    let src = format!("export const result = ({expr});");
    as_str(&export(&run_ok(&src), "result"))
}

pub fn eval_bool(expr: &str) -> bool {
    let src = format!("export const result = ({expr});");
    as_bool(&export(&run_ok(&src), "result"))
}

/// 配列 export を `Vec<f64>` として取り出す。
pub fn as_num_vec(v: &Value) -> Vec<f64> {
    match v {
        Value::Object(o) => match &o.borrow().kind {
            tsint::value::ObjKind::Array(items) => items.iter().map(as_num).collect(),
            _ => panic!("配列ではありません"),
        },
        other => panic!("配列ではありません: {other:?}"),
    }
}

pub fn as_str_vec(v: &Value) -> Vec<String> {
    match v {
        Value::Object(o) => match &o.borrow().kind {
            tsint::value::ObjKind::Array(items) => items.iter().map(as_str).collect(),
            _ => panic!("配列ではありません"),
        },
        other => panic!("配列ではありません: {other:?}"),
    }
}
