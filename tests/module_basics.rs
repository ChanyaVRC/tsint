//! T5a: モジュールシステム（`src/module.rs`）のユニットテスト。
//!
//! Noiria の実ファイルを使う「完走テスト」は `tests/kag_full.rs`。ここでは
//! `ModuleHost` を単純なインメモリマップで実装し、import/export の各形式・
//! キャッシュ（1 回だけ評価）・循環 import の簡易挙動・エントリモジュールの
//! トップレベル await が保留されたまま `run_event_loop` で駆動を続けられる
//! ことを確認する。

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use tsint::module::{ModuleError, ModuleHost, ModuleOutcome};
use tsint::value::{PropSlot, Value};
use tsint::vm::{HostPromise, PumpState};
use tsint::Interpreter;

struct MapHost(HashMap<&'static str, &'static str>);

impl ModuleHost for MapHost {
    fn resolve(&self, specifier: &str, referrer: &str) -> Result<String, String> {
        if self.0.contains_key(specifier) {
            Ok(specifier.to_string())
        } else {
            Err(format!("解決できません: '{specifier}' (from {referrer})"))
        }
    }
    fn load(&self, resolved: &str) -> Result<String, String> {
        self.0
            .get(resolved)
            .map(|s| s.to_string())
            .ok_or_else(|| format!("見つかりません: {resolved}"))
    }
}

fn field(v: &Value, name: &str) -> Value {
    if let Value::Object(o) = v {
        if let Some(PropSlot::Value(val)) = o.borrow().props.get(name) {
            return val.clone();
        }
    }
    Value::Undefined
}

fn capture_lines(interp: &mut Interpreter) -> Rc<RefCell<Vec<String>>> {
    let lines: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let lines2 = lines.clone();
    interp.set_console_hook(move |s: &str| lines2.borrow_mut().push(s.to_string()));
    lines
}

fn settle(outcome: ModuleOutcome) -> Vec<(Rc<str>, Value)> {
    match outcome {
        ModuleOutcome::Settled(exports) => exports,
        ModuleOutcome::Pending => panic!("Settled を期待しましたが Pending でした"),
    }
}

#[test]
fn named_default_and_namespace_imports_resolve_across_modules() {
    let host = MapHost(HashMap::from([
        (
            "lib",
            r#"
            export const answer = 42;
            export default function greet(name: string): string {
                return `hello, ${name}`;
            }
            "#,
        ),
        (
            "entry",
            r#"
            import greet, { answer } from "lib";
            import * as lib from "lib";
            export const combined = `${greet("world")} ${answer} ${lib.answer}`;
            "#,
        ),
    ]));
    let mut interp = Interpreter::new();
    let exports = settle(
        interp
            .run_module_graph("entry", &host)
            .expect("モジュールグラフの評価に失敗"),
    );
    let combined = exports.iter().find(|(n, _)| &**n == "combined").unwrap();
    match &combined.1 {
        Value::Str(s) => assert_eq!(&**s, "hello, world 42 42"),
        other => panic!("文字列を期待: {other:?}"),
    }
}

#[test]
fn named_reexport_from_forwards_value() {
    let host = MapHost(HashMap::from([
        ("base", r#"export const x = 1;"#),
        ("mid", r#"export { x as y } from "base";"#),
        (
            "entry",
            r#"
            import { y } from "mid";
            export const seen = y;
            "#,
        ),
    ]));
    let mut interp = Interpreter::new();
    let exports = settle(interp.run_module_graph("entry", &host).unwrap());
    let seen = &exports.iter().find(|(n, _)| &**n == "seen").unwrap().1;
    assert!(matches!(seen, Value::Number(n) if *n == 1.0));
}

#[test]
fn export_star_as_namespace_forwards_whole_module() {
    let host = MapHost(HashMap::from([
        ("base", r#"export const a = 1; export const b = 2;"#),
        ("mid", r#"export * as ns from "base";"#),
        (
            "entry",
            r#"
            import { ns } from "mid";
            export const sum = ns.a + ns.b;
            "#,
        ),
    ]));
    let mut interp = Interpreter::new();
    let exports = settle(interp.run_module_graph("entry", &host).unwrap());
    let sum = &exports.iter().find(|(n, _)| &**n == "sum").unwrap().1;
    assert!(matches!(sum, Value::Number(n) if *n == 3.0));
}

#[test]
fn export_star_bare_forwards_all_named_exports_excluding_default() {
    let host = MapHost(HashMap::from([
        (
            "base",
            r#"export const a = 1; export const b = 2; export default 99;"#,
        ),
        ("mid", r#"export * from "base";"#),
        (
            "entry",
            r#"
            import { a, b } from "mid";
            export const sum = a + b;
            "#,
        ),
    ]));
    let mut interp = Interpreter::new();
    let exports = settle(interp.run_module_graph("entry", &host).unwrap());
    let sum = &exports.iter().find(|(n, _)| &**n == "sum").unwrap().1;
    assert!(matches!(sum, Value::Number(n) if *n == 3.0));
}

#[test]
fn import_type_only_does_not_require_module_to_exist() {
    let host = MapHost(HashMap::from([(
        "entry",
        r#"
        import type { Nope } from "does-not-exist";
        export const ok = 1;
        "#,
    )]));
    let mut interp = Interpreter::new();
    let exports = settle(interp.run_module_graph("entry", &host).unwrap());
    let ok = &exports.iter().find(|(n, _)| &**n == "ok").unwrap().1;
    assert!(matches!(ok, Value::Number(n) if *n == 1.0));
}

#[test]
fn export_type_only_from_does_not_require_module_to_exist() {
    let host = MapHost(HashMap::from([(
        "entry",
        r#"
        export type { Nope } from "does-not-exist";
        export const ok = 2;
        "#,
    )]));
    let mut interp = Interpreter::new();
    let exports = settle(interp.run_module_graph("entry", &host).unwrap());
    let ok = &exports.iter().find(|(n, _)| &**n == "ok").unwrap().1;
    assert!(matches!(ok, Value::Number(n) if *n == 2.0));
}

#[test]
fn mixed_value_and_type_named_imports_ignore_the_undefined_type_bindings() {
    // player.ts の実例と同じ形: `import { real, TypeOnlyThing } from "mod"`
    // （`TypeOnlyThing` は実際には interface で、型注釈でしか使われない）。
    let host = MapHost(HashMap::from([
        (
            "mod",
            r#"
            export interface TypeOnlyThing { x: number }
            export function real(): number { return 7; }
            "#,
        ),
        (
            "entry",
            r#"
            import { real, TypeOnlyThing } from "mod";
            function useType(v: TypeOnlyThing): number { return v.x; }
            export const result = real();
            "#,
        ),
    ]));
    let mut interp = Interpreter::new();
    let exports = settle(interp.run_module_graph("entry", &host).unwrap());
    let result = &exports.iter().find(|(n, _)| &**n == "result").unwrap().1;
    assert!(matches!(result, Value::Number(n) if *n == 7.0));
}

#[test]
fn dependency_module_is_evaluated_exactly_once_even_with_diamond_imports() {
    let host = MapHost(HashMap::from([
        (
            "shared",
            r#"
            console.log("evaluating shared");
            export const value = 10;
            "#,
        ),
        ("left", r#"export { value } from "shared";"#),
        ("right", r#"export { value as value2 } from "shared";"#),
        (
            "entry",
            r#"
            import { value } from "left";
            import { value2 } from "right";
            export const sum = value + value2;
            "#,
        ),
    ]));
    let mut interp = Interpreter::new();
    let lines = capture_lines(&mut interp);
    let exports = settle(interp.run_module_graph("entry", &host).unwrap());
    let sum = &exports.iter().find(|(n, _)| &**n == "sum").unwrap().1;
    assert!(matches!(sum, Value::Number(n) if *n == 20.0));
    assert_eq!(&*lines.borrow(), &["evaluating shared"]);
}

#[test]
fn circular_value_import_sees_undefined_for_not_yet_populated_export() {
    // a.ts が b.ts を import した時点ではまだ b.ts は評価中で、その時点の
    // 名前空間オブジェクトは空。「簡易挙動」として undefined になることを
    // 確認する（クラッシュしない・無限ループしないことが主眼）。
    let host = MapHost(HashMap::from([
        (
            "a",
            r#"
            import { bValue } from "b";
            export const aSeenFromA = bValue;
            export const aValue = 1;
            "#,
        ),
        (
            "b",
            r#"
            import { aValue } from "a";
            export const bSeenFromB = aValue;
            export const bValue = 2;
            "#,
        ),
        (
            "entry",
            r#"
            import { aSeenFromA, aValue } from "a";
            import { bSeenFromB, bValue } from "b";
            export const results = [aSeenFromA, aValue, bSeenFromB, bValue];
            "#,
        ),
    ]));
    let mut interp = Interpreter::new();
    let exports = settle(interp.run_module_graph("entry", &host).unwrap());
    let results = &exports.iter().find(|(n, _)| &**n == "results").unwrap().1;
    // entry は a を先に読み込む -> a は b を読み込む -> b は (循環につき)
    // まだ空の a の名前空間を見る -> bSeenFromB は undefined。b は評価完了して
    // bValue=2 を返す -> a の aSeenFromA は 2 になる。
    let items = tsint::vm::array_values(results);
    assert!(
        matches!(&items[0], Value::Number(n) if *n == 2.0),
        "aSeenFromA"
    );
    assert!(matches!(&items[1], Value::Number(n) if *n == 1.0), "aValue");
    assert!(matches!(&items[2], Value::Undefined), "bSeenFromB");
    assert!(matches!(&items[3], Value::Number(n) if *n == 2.0), "bValue");
}

#[test]
fn unresolvable_specifier_is_a_clear_module_error() {
    let host = MapHost(HashMap::from([(
        "entry",
        r#"import { x } from "totally-missing";"#,
    )]));
    let mut interp = Interpreter::new();
    let err = interp.run_module_graph("entry", &host).unwrap_err();
    match err {
        ModuleError::Resolve { specifier, .. } => assert_eq!(specifier, "totally-missing"),
        other => panic!("Resolve エラーを期待: {other}"),
    }
}

#[test]
fn runtime_error_in_dependency_is_reported_with_its_specifier() {
    let host = MapHost(HashMap::from([
        ("broken", r#"throw new Error("boom");"#),
        ("entry", r#"import "broken";"#),
    ]));
    let mut interp = Interpreter::new();
    let err = interp.run_module_graph("entry", &host).unwrap_err();
    match err {
        ModuleError::Runtime { resolved, message } => {
            assert_eq!(resolved, "broken");
            assert!(message.contains("boom"), "{message}");
        }
        other => panic!("Runtime エラーを期待: {other}"),
    }
}

#[test]
fn entry_module_top_level_await_can_stay_pending_and_is_driven_by_run_event_loop() {
    let waiter: Rc<RefCell<Option<HostPromise>>> = Rc::new(RefCell::new(None));
    let waiter2 = waiter.clone();
    let mut interp = Interpreter::new();
    interp.register_native_async("hostWait", 0, move |_vm, _this, _args, handle| {
        *waiter2.borrow_mut() = Some(handle);
    });
    let lines = capture_lines(&mut interp);

    let host = MapHost(HashMap::from([(
        "entry",
        r#"
        console.log("before");
        await hostWait();
        console.log("after");
        "#,
    )]));
    let outcome = interp.run_module_graph("entry", &host).unwrap();
    assert!(matches!(outcome, ModuleOutcome::Pending));
    assert_eq!(&*lines.borrow(), &["before"]);
    assert_eq!(interp.run_event_loop(), PumpState::Waiting);

    let handle = waiter
        .borrow_mut()
        .take()
        .expect("hostWait が呼ばれているはず");
    handle.resolve(interp.vm(), Value::Undefined);
    assert_eq!(interp.run_event_loop(), PumpState::Idle);
    assert_eq!(&*lines.borrow(), &["before", "after"]);
}

#[test]
fn field_helper_smoke_test_for_object_props() {
    // field() ヘルパー自体の健全性チェック（他テストで多用するため）。
    let obj = Value::new_plain_object();
    if let Value::Object(o) = &obj {
        o.borrow_mut()
            .props
            .set_value(Rc::from("k"), Value::Number(5.0));
    }
    assert!(matches!(field(&obj, "k"), Value::Number(n) if n == 5.0));
    assert!(matches!(field(&obj, "missing"), Value::Undefined));
}
