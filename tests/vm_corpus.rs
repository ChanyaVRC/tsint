//! Noiria コーパス実行テスト（T2 の最重要項目）。
//!
//! `noiria/runtime/kag/parser.ts` と `noiria/runtime/kag/vars.ts` は
//! import を持たない純粋モジュールなので、tsint でそのまま実行できる。
//! ここでのアサーションは、Noiria の V8 統合テスト
//! （`noiria/crates/noiria-app/src/script/mod.rs` の `js_tests`）と
//! 同一の入力・同一の期待値を用いて、同じ結果になることを確認する。
//!
//! `../noiria` が存在しない環境ではテストを skip する。

#[path = "common/mod.rs"]
mod common;

use std::rc::Rc;

use tsint::value::{ObjKind, PropSlot, Value};
use tsint::vm::array_values;
use tsint::Interpreter;

fn field(v: &Value, name: &str) -> Value {
    if let Value::Object(o) = v {
        if let Some(PropSlot::Value(val)) = o.borrow().props.get(name) {
            return val.clone();
        }
    }
    Value::Undefined
}

fn as_str(v: &Value) -> String {
    match v {
        Value::Str(s) => s.to_string(),
        Value::Undefined => "undefined".to_string(),
        Value::Null => "null".to_string(),
        other => format!("{other:?}"),
    }
}

fn as_num(v: &Value) -> f64 {
    match v {
        Value::Number(n) => *n,
        other => panic!("数値ではありません: {other:?}"),
    }
}

fn map_entries(v: &Value) -> Vec<(Value, Value)> {
    if let Value::Object(o) = v {
        if let ObjKind::Map(m) = &o.borrow().kind {
            return m.entries.clone();
        }
    }
    panic!("Map ではありません");
}

fn read_corpus(rel: &str) -> Option<String> {
    let path = common::noiria_root().join(rel);
    std::fs::read_to_string(path).ok()
}

#[test]
fn kag_parser_parses_fixture_same_as_v8_test() {
    let Some(src) = read_corpus("runtime/kag/parser.ts") else {
        eprintln!("../noiria が見つからないため skip します");
        return;
    };
    let mut interp = Interpreter::new();
    let exports = interp
        .run_module(&src, "runtime/kag/parser.ts")
        .expect("parser.ts の実行に失敗");
    let parse_scenario = exports
        .iter()
        .find(|(n, _)| &**n == "parseScenario")
        .expect("parseScenario が export されていません")
        .1
        .clone();

    let fixture = "; コメント行は無視\n\
*start|はじまり\n\
@playbgm storage=main.wav loop=true\n\
こんにちは[l][r]\n\
[image layer=base storage=\"部屋 の絵.png\" left=10]\n\
[emb exp=\"f.x + 1\"]\n\
[[エスケープ]も本文\n\
[link target=*end]選択肢[endlink]\n\
*end\n";

    let vm = interp.vm();
    let sc = vm
        .call_value(
            parse_scenario,
            Value::Undefined,
            &[Value::str(fixture), Value::str("test.ks")],
        )
        .expect("parseScenario の呼び出しに失敗");

    // ラベル一覧: [...sc.labels.keys()] === ["start", "end"]
    let labels = map_entries(&field(&sc, "labels"));
    let label_names: Vec<String> = labels.iter().map(|(k, _)| as_str(k)).collect();
    assert_eq!(label_names, vec!["start", "end"], "ラベル一覧");
    assert_eq!(as_num(&labels[0].1), 0.0, "start の位置");

    // コマンド列
    let commands = array_values(&field(&sc, "commands"));
    let kinds: Vec<String> = commands
        .iter()
        .map(|c| {
            let ty = as_str(&field(c, "type"));
            match ty.as_str() {
                "tag" => format!("tag:{}", as_str(&field(c, "name"))),
                "label" => format!("label:{}", as_str(&field(c, "name"))),
                _ => format!("text:{}", as_str(&field(c, "text"))),
            }
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            "label:start",
            "tag:playbgm",
            "text:こんにちは",
            "tag:l",
            "tag:r",
            "tag:image",
            "tag:emb",
            "text:[エスケープ]も本文",
            "tag:link",
            "text:選択肢",
            "tag:endlink",
            "label:end",
        ],
        "コマンド列"
    );

    let playbgm_attrs = field(&commands[1], "attrs");
    assert_eq!(as_str(&field(&playbgm_attrs, "storage")), "main.wav");
    assert_eq!(as_str(&field(&playbgm_attrs, "loop")), "true");

    let image_attrs = field(&commands[5], "attrs");
    assert_eq!(
        as_str(&field(&image_attrs, "storage")),
        "部屋 の絵.png",
        "クォート内の空白を保持"
    );
    assert_eq!(as_str(&field(&image_attrs, "left")), "10", "裸の属性値");

    let emb_attrs = field(&commands[6], "attrs");
    assert_eq!(as_str(&field(&emb_attrs, "exp")), "f.x + 1", "式属性");

    assert_eq!(
        as_str(&field(&commands[0], "display")),
        "はじまり",
        "ラベル表示名"
    );
}

#[test]
fn kag_parser_valueless_and_at_tags_same_as_v8_test() {
    let Some(src) = read_corpus("runtime/kag/parser.ts") else {
        eprintln!("../noiria が見つからないため skip します");
        return;
    };
    let mut interp = Interpreter::new();
    let exports = interp
        .run_module(&src, "runtime/kag/parser.ts")
        .expect("parser.ts の実行に失敗");
    let parse_scenario = exports
        .iter()
        .find(|(n, _)| &**n == "parseScenario")
        .unwrap()
        .1
        .clone();

    let fixture = "@cm\n@wait time=300\n[nowait][endnowait]\n";
    let vm = interp.vm();
    let sc = vm
        .call_value(
            parse_scenario,
            Value::Undefined,
            &[Value::str(fixture), Value::str("t.ks")],
        )
        .unwrap();
    let commands = array_values(&field(&sc, "commands"));
    let kinds: Vec<String> = commands
        .iter()
        .map(|c| {
            let ty = as_str(&field(c, "type"));
            let name = field(c, "name");
            let name = if matches!(name, Value::Undefined) {
                String::new()
            } else {
                as_str(&name)
            };
            format!("{ty}:{name}")
        })
        .collect();
    assert_eq!(
        kinds,
        vec!["tag:cm", "tag:wait", "tag:nowait", "tag:endnowait"],
        "行タグとインラインタグ"
    );

    let wait_attrs = field(&commands[1], "attrs");
    assert_eq!(as_str(&field(&wait_attrs, "time")), "300", "@タグの属性");
}

#[test]
fn kag_vars_eval_same_as_v8_test() {
    let Some(src) = read_corpus("runtime/kag/vars.ts") else {
        eprintln!("../noiria が見つからないため skip します");
        return;
    };
    let mut interp = Interpreter::new();
    let exports = interp
        .run_module(&src, "runtime/kag/vars.ts")
        .expect("vars.ts の実行に失敗");
    let get = |name: &str| {
        exports
            .iter()
            .find(|(n, _)| &**n == name)
            .unwrap()
            .1
            .clone()
    };
    let f = get("f");
    let eval_expr = get("evalExpr");
    let eval_stmt = get("evalStmt");
    let replace_f = get("replaceF");

    let vm = interp.vm();

    vm.call_value(
        eval_stmt.clone(),
        Value::Undefined,
        &[Value::str("f.x = 2 + 3"), Value::Null],
    )
    .unwrap();
    let r = vm
        .call_value(
            eval_expr.clone(),
            Value::Undefined,
            &[Value::str("f.x * 2"), Value::Null],
        )
        .unwrap();
    assert_eq!(as_num(&r), 10.0, "eval 結果");
    assert_eq!(as_num(&field(&f, "x")), 5.0, "f へ書き込まれる");

    vm.call_value(
        eval_stmt.clone(),
        Value::Undefined,
        &[Value::str("f.name = 'アリス'"), Value::Null],
    )
    .unwrap();
    let r = vm
        .call_value(
            eval_expr.clone(),
            Value::Undefined,
            &[Value::str("f.name + 'さん'"), Value::Null],
        )
        .unwrap();
    assert_eq!(as_str(&r), "アリスさん", "文字列式");

    let new_data = Value::new_plain_object();
    if let Value::Object(o) = &new_data {
        o.borrow_mut()
            .props
            .set_value(Rc::from("y"), Value::Number(1.0));
    }
    vm.call_value(replace_f, Value::Undefined, &[new_data])
        .unwrap();
    assert!(
        matches!(field(&f, "x"), Value::Undefined),
        "replaceF が古いキーを消す"
    );
    assert_eq!(as_num(&field(&f, "y")), 1.0, "replaceF が新しい値を入れる");

    let sys = Value::new_plain_object();
    if let Value::Object(o) = &sys {
        o.borrow_mut()
            .props
            .set_value(Rc::from("answer"), Value::Number(42.0));
    }
    let r = vm
        .call_value(
            eval_expr,
            Value::Undefined,
            &[Value::str("sys.answer"), sys],
        )
        .unwrap();
    assert_eq!(as_num(&r), 42.0, "sys 参照");
}

/// import 文のあるファイルは T2 では明確なエラーになることの確認
/// （`runtime/kag/player.ts` は他モジュールを import する側の代表例。
/// モジュール連携（T5）が無い T2 では、黙って壊れるのではなく
/// コンパイル時にはっきりしたエラーを返す）。
#[test]
fn files_with_imports_are_clear_errors_not_silent_misbehavior() {
    let Some(src) = read_corpus("runtime/kag/player.ts") else {
        eprintln!("../noiria が見つからないため skip します");
        return;
    };
    let mut interp = Interpreter::new();
    let err = interp
        .run_module(&src, "runtime/kag/player.ts")
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("import"),
        "import 関連の明確なエラーであるべき: {msg}"
    );
}
