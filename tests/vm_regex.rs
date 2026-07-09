//! T4（正規表現エンジン本実装）のテスト。
//!
//! - 構文別ユニットテスト（量指定子・クラス・選択・アンカー・\b・後方参照・
//!   先読み・{n,m}・ネストしたキャプチャ番号）は `src/regex.rs` の
//!   `#[cfg(test)]` にある（エンジン内部の Rust API を直接叩く）。
//! - ここでは実際に tsint の VM（パーサ + コンパイラ + VM + builtins）を
//!   通して、JS の `RegExp`/`String.prototype.{match,replace,split,search}`
//!   としての挙動をテストする。「Node での実行結果」とコメントしてある値は
//!   実際に Node.js で実行して確認した値。

#[path = "vm_common/mod.rs"]
mod vm_common;
use vm_common::*;

// ============================== RegExp オブジェクト基本 ==============================

#[test]
fn regex_literal_test_basic() {
    assert!(eval_bool(r#"/abc/.test("xxabcyy")"#));
    assert!(!eval_bool(r#"/abc/.test("xyz")"#));
}

#[test]
fn regex_source_and_flags_properties() {
    // Node: `/foo/.source` === "foo"、`/foo/gim.flags` === "gim"
    assert_eq!(eval_str("/foo/.source"), "foo");
    assert_eq!(eval_str("/foo/gim.flags"), "gim");
}

#[test]
fn regex_flag_boolean_properties() {
    assert!(eval_bool("/foo/gim.global"));
    assert!(eval_bool("/foo/gim.ignoreCase"));
    assert!(eval_bool("/foo/gim.multiline"));
    assert!(!eval_bool("/foo/gim.dotAll"));
    assert!(!eval_bool("/foo/.global"));
}

#[test]
fn regex_to_string() {
    assert_eq!(eval_str("String(/ab+c/i)"), "/ab+c/i");
}

// ============================== new RegExp / RegExp(...) ==============================

#[test]
fn new_regexp_from_string_and_flags() {
    assert!(eval_bool(r#"new RegExp("a+", "i").test("AAA")"#));
}

#[test]
fn regexp_constructor_without_new_also_works() {
    assert!(eval_bool(r#"RegExp("^x").test("xyz")"#));
}

#[test]
fn new_regexp_from_existing_regex_copies_source() {
    // Node: `new RegExp(/a+/, "i").source` === "a+"
    assert_eq!(eval_str(r#"new RegExp(/a+/, "i").source"#), "a+");
}

#[test]
fn new_regexp_from_existing_regex_without_flags_keeps_original_flags() {
    assert_eq!(eval_str("new RegExp(/a+/gi).flags"), "gi");
}

#[test]
fn invalid_regex_pattern_throws_syntax_error() {
    let err = run_err(r#"export const r = new RegExp("[a");"#);
    assert!(
        err.contains("正規表現") || err.contains("SyntaxError"),
        "{err}"
    );
}

// ============================== 量指定子 ==============================

#[test]
fn star_greedy_via_match0() {
    assert_eq!(eval_str(r#""aaab".match(/a*/)[0]"#), "aaa");
}

#[test]
fn star_lazy_via_match0() {
    assert_eq!(eval_str(r#""aaab".match(/a*?/)[0]"#), "");
}

#[test]
fn plus_and_question_mark() {
    assert!(!eval_bool(r#"/a+/.test("b")"#));
    assert!(eval_bool(r#"/colou?r/.test("color")"#));
    assert!(eval_bool(r#"/colou?r/.test("colour")"#));
}

#[test]
fn brace_quantifiers() {
    assert_eq!(eval_str(r#""aaaaa".match(/a{2,4}/)[0]"#), "aaaa");
    assert_eq!(eval_str(r#""aaaaa".match(/a{2,4}?/)[0]"#), "aa");
    assert!(!eval_bool(r#"/^a{3}$/.test("aa")"#));
}

// ============================== 文字クラス・エスケープ ==============================

#[test]
fn digit_word_space_shorthands() {
    assert_eq!(eval_str(r#""ab123cd".match(/\d+/)[0]"#), "123");
    assert!(eval_bool(r#"/\s/.test(" ")"#));
    assert!(!eval_bool(r#"/\s/.test("x")"#));
    // JS の \w は ASCII のみ（Unicode の英字は対象外）。
    assert_eq!(eval_str(r#""あabあ".match(/\w+/)[0]"#), "ab");
}

#[test]
fn char_class_range_and_negate() {
    assert_eq!(eval_str(r#""xxabcxx".match(/[a-c]+/)[0]"#), "abc");
    assert_eq!(eval_str(r#""abcXYZ".match(/[^a-c]+/)[0]"#), "XYZ");
}

// ============================== 選択・アンカー・\b・後方参照・先読み ==============================

#[test]
fn alternation_tries_left_to_right() {
    // Node: /a|ab/.exec("ab")[0] === "a"（最長一致ではなく左から順）
    assert_eq!(eval_str(r#""ab".match(/a|ab/)[0]"#), "a");
}

#[test]
fn anchors_without_multiline() {
    assert!(eval_bool(r#"/^abc$/.test("abc")"#));
    assert!(!eval_bool(r#"/^abc$/.test("xabc")"#));
    assert!(!eval_bool("/^foo$/.test(\"foo\\n\")"));
}

#[test]
fn anchors_with_multiline() {
    assert!(eval_bool("/^b/m.test(\"a\\nb\")"));
}

#[test]
fn word_boundary() {
    assert_eq!(eval_str(r#""a cat sat".match(/\bcat\b/)[0]"#), "cat");
    assert!(!eval_bool(r#"/\bcat\b/.test("concatenate")"#));
}

#[test]
fn backreference_js() {
    assert!(eval_bool(r#"/(\w+) \1/.test("hello hello")"#));
    assert!(!eval_bool(r#"/(\w+) \1/.test("hello world")"#));
}

#[test]
fn lookahead_positive_negative() {
    assert!(eval_bool(r#"/foo(?=bar)/.test("foobar")"#));
    assert!(!eval_bool(r#"/foo(?=bar)/.test("foobaz")"#));
    assert!(eval_bool(r#"/foo(?!bar)/.test("foobaz")"#));
    assert!(!eval_bool(r#"/foo(?!bar)/.test("foobar")"#));
}

#[test]
fn lookbehind_and_named_group_are_clear_compile_errors() {
    let err = run_err(r#"export const r = /(?<=x)y/;"#);
    assert!(
        err.contains("後読み") || err.contains("lookbehind"),
        "{err}"
    );
    let err2 = run_err(r#"export const r = /(?<name>x)/;"#);
    assert!(err2.contains("名前付き"), "{err2}");
}

// ============================== キャプチャグループ ==============================

#[test]
fn exec_undefined_capture_for_non_participating_group() {
    // Node: /(a)(b)?/.exec("a") === [ 'a', 'a', undefined, index: 0, ... ]
    let src = r#"
        const m = /(a)(b)?/.exec("a");
        export const g0 = m[0];
        export const g1 = m[1];
        export const g2isUndef = m[2] === undefined;
        export const idx = m.index;
    "#;
    let exports = run_ok(src);
    assert_eq!(as_str(&export(&exports, "g0")), "a");
    assert_eq!(as_str(&export(&exports, "g1")), "a");
    assert!(as_bool(&export(&exports, "g2isUndef")));
    assert_eq!(as_num(&export(&exports, "idx")), 0.0);
}

#[test]
fn nested_capture_group_numbering() {
    let src = r#"
        const m = /((a)(b))(c)/.exec("abc");
        export const g1 = m[1];
        export const g2 = m[2];
        export const g3 = m[3];
        export const g4 = m[4];
    "#;
    let exports = run_ok(src);
    assert_eq!(as_str(&export(&exports, "g1")), "ab");
    assert_eq!(as_str(&export(&exports, "g2")), "a");
    assert_eq!(as_str(&export(&exports, "g3")), "b");
    assert_eq!(as_str(&export(&exports, "g4")), "c");
}

// ============================== ネストした量指定子のバックトラック（回帰） ==============================
//
// 実装過程で「量指定子の本体をサブルーチンとして1回だけ呼ぶ」設計だと、外側の
// 継続が失敗した時に本体の消費文字数を正しく再試行できない不具合があった
// （catastrophic backtracking のステップ上限テストが誤って「一瞬で完了」して
// しまうことで発覚した）。ここではその修正が効いていることを確認する。

#[test]
fn nested_quantifier_backtracking_splits_correctly() {
    // Node: /^(a+)(a+)$/.exec("aaa") === [ 'aaa', 'aa', 'a', index: 0, ... ]
    // （最初の a+ が貪欲に全部取った後、2番目の a+ が最低1文字必要なので
    // 1文字だけバックトラックする）。
    let src = r#"
        const m = /^(a+)(a+)$/.exec("aaa");
        export const g1 = m[1];
        export const g2 = m[2];
    "#;
    let exports = run_ok(src);
    assert_eq!(as_str(&export(&exports, "g1")), "aa");
    assert_eq!(as_str(&export(&exports, "g2")), "a");
}

#[test]
fn classic_backtracking_alternation_and_repeat_combo() {
    // Node: /^(a|ab)(c|bcd)(d*)$/.exec("abcd")
    //   === [ 'abcd', 'a', 'bcd', '', index: 0, ... ]
    let src = r#"
        const m = /^(a|ab)(c|bcd)(d*)$/.exec("abcd");
        export const g1 = m[1];
        export const g2 = m[2];
        export const g3 = m[3];
    "#;
    let exports = run_ok(src);
    assert_eq!(as_str(&export(&exports, "g1")), "a");
    assert_eq!(as_str(&export(&exports, "g2")), "bcd");
    assert_eq!(as_str(&export(&exports, "g3")), "");
}

#[test]
fn nullable_repeat_does_not_hang() {
    // `(a?)*` は空文字列にもマッチでき、無限ループしないことを確認する。
    assert_eq!(eval_str(r#""".match(/(a?)*/)[0]"#), "");
}

// ============================== lastIndex / g フラグの状態管理 ==============================

#[test]
fn global_exec_advances_last_index_like_node() {
    // Node での実行結果:
    // [["a",1,2],["a",3,4],["a",5,6]]
    let src = r#"
        const re = /a/g;
        const out = [];
        let m;
        while ((m = re.exec("banana")) !== null) {
            out.push(m[0] + ":" + m.index + ":" + re.lastIndex);
        }
        export const result = out.join(",");
    "#;
    let exports = run_ok(src);
    assert_eq!(as_str(&export(&exports, "result")), "a:1:2,a:3:4,a:5:6");
}

#[test]
fn non_global_exec_does_not_touch_last_index() {
    let src = r#"
        const re = /a/;
        re.exec("banana");
        export const li = re.lastIndex;
    "#;
    let exports = run_ok(src);
    assert_eq!(as_num(&export(&exports, "li")), 0.0);
}

#[test]
fn test_with_global_flag_is_stateful() {
    let src = r#"
        const re = /a/g;
        const r1 = re.test("aa");
        const idx1 = re.lastIndex;
        const r2 = re.test("aa");
        const idx2 = re.lastIndex;
        const r3 = re.test("aa"); // lastIndex(2) と len(2) が一致 -> false かつ lastIndex は 0 にリセット
        const idx3 = re.lastIndex;
        export const summary = [r1, idx1, r2, idx2, r3, idx3].join(",");
    "#;
    let exports = run_ok(src);
    assert_eq!(
        as_str(&export(&exports, "summary")),
        "true,1,true,2,false,0"
    );
}

// ============================== String.prototype.match/replace/split/search ==============================

#[test]
fn replace_all_a_with_b_global_flag() {
    // Node: "aaa".replace(/a/g, "b") === "bbb"
    assert_eq!(eval_str(r#""aaa".replace(/a/g, "b")"#), "bbb");
}

#[test]
fn replace_without_global_replaces_first_only() {
    assert_eq!(eval_str(r#""aaa".replace(/a/, "b")"#), "baa");
}

#[test]
fn split_by_digit_regex() {
    // Node: "a1b2".split(/\d/) === [ 'a', 'b', '' ]
    assert_eq!(
        as_str_vec(&eval_export(r#""a1b2".split(/\d/)"#)),
        vec!["a", "b", ""]
    );
}

#[test]
fn split_with_capturing_group_includes_captures() {
    // Node: "a1b2".split(/(\d)/) === [ 'a', '1', 'b', '2', '' ]
    assert_eq!(
        as_str_vec(&eval_export(r#""a1b2".split(/(\d)/)"#)),
        vec!["a", "1", "b", "2", ""]
    );
}

#[test]
fn split_empty_string_with_matching_and_nonmatching_pattern() {
    // Node: "".split(/x*/) === []、"ab".split(/x*/) === ["a","b"]
    assert_eq!(
        as_str_vec(&eval_export(r#""".split(/x*/)"#)),
        Vec::<String>::new()
    );
    assert_eq!(
        as_str_vec(&eval_export(r#""ab".split(/x*/)"#)),
        vec!["a", "b"]
    );
}

#[test]
fn split_with_limit() {
    assert_eq!(
        as_str_vec(&eval_export(r#""a,b,c,d".split(",", 2)"#)),
        vec!["a", "b"]
    );
}

#[test]
fn match_global_returns_whole_matches_only() {
    // Node: "abc".match(/(.)/g) === [ 'a', 'b', 'c' ]（キャプチャは含まれない）
    assert_eq!(
        as_str_vec(&eval_export(r#""abc".match(/(.)/g)"#)),
        vec!["a", "b", "c"]
    );
}

#[test]
fn match_non_global_returns_exec_like_array() {
    let src = r#"
        const m = "xabcx".match(/b./);
        export const whole = m[0];
        export const idx = m.index;
    "#;
    let exports = run_ok(src);
    assert_eq!(as_str(&export(&exports, "whole")), "bc");
    assert_eq!(as_num(&export(&exports, "idx")), 2.0);
}

#[test]
fn match_with_no_hits_returns_null() {
    assert!(is_null(&eval_export(r#""abc".match(/z/)"#)));
}

#[test]
fn search_returns_index_or_minus_one() {
    assert_eq!(eval_num(r#""xyz".search(/y/)"#), 1.0);
    assert_eq!(eval_num(r#""xyz".search(/q/)"#), -1.0);
}

#[test]
fn replace_with_callback_function() {
    // Node: "foo".replace(/o/g, (m,offset,s)=>m.toUpperCase()+offset) === "fO1O2"
    assert_eq!(
        eval_str(r#""foo".replace(/o/g, (m, offset) => m.toUpperCase() + offset)"#),
        "fO1O2"
    );
}

#[test]
fn replace_with_dollar_group_references() {
    // Node: "2024-01-02".replace(/(\d+)-(\d+)-(\d+)/, "$3/$2/$1") === "02/01/2024"
    assert_eq!(
        eval_str(r#""2024-01-02".replace(/(\d+)-(\d+)-(\d+)/, "$3/$2/$1")"#),
        "02/01/2024"
    );
}

#[test]
fn replace_dollar_ampersand_and_dollar_dollar() {
    assert_eq!(eval_str(r#""abc".replace("b", "[$&]")"#), "a[b]c");
    assert_eq!(eval_str(r#""abc".replace("b", "$$")"#), "a$c");
}

#[test]
fn replace_string_pattern_ignores_flags_semantics_replaces_first_only() {
    assert_eq!(eval_str(r#""aaa".replace("a", "b")"#), "baa");
}

#[test]
fn replace_all_requires_global_flag_for_regex() {
    let err = run_err(r#""a".replaceAll(/a/, "b");"#);
    assert!(
        err.contains("g フラグ") || err.contains("TypeError"),
        "{err}"
    );
}

#[test]
fn replace_all_with_string_pattern_replaces_every_occurrence() {
    assert_eq!(eval_str(r#""a-b-c".replaceAll("-", "_")"#), "a_b_c");
}

#[test]
fn replace_all_with_global_regex() {
    assert_eq!(eval_str(r#""a1b2c3".replaceAll(/\d/g, "X")"#), "aXbXcX");
}

#[test]
fn ignore_case_flag_via_string_replace() {
    // Node: "color".replace(/colou?r/i, "X") === "X"
    assert_eq!(eval_str(r#""color".replace(/colou?r/i, "X")"#), "X");
}

// ============================== ステップ上限（壊滅的バックトラック対策） ==============================

#[test]
fn catastrophic_backtracking_pattern_is_a_clear_error_not_a_hang() {
    let err = run_err(
        r#"
        const re = /^(a+)+$/;
        re.test("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa!");
    "#,
    );
    assert!(
        err.contains("ステップ数") || err.contains("再帰の深さ"),
        "壊滅的バックトラックが明確なエラーになるべき: {err}"
    );
}

// ============================== Noiria コーパス由来の正規表現 ==============================
//
// `noiria/runtime/kag/parser.ts` / `player.ts` で実際に使われている正規表現
// （grep 結果）を、そのままの形で再現してテストする。
//
// - parser.ts:27  `source.split(/\r\n|\n|\r/)`
// - parser.ts:111 `/\s/.test(body[i])`
// - parser.ts:115 `!/\s/.test(body[i])`
// - parser.ts:121 `!/[\s=]/.test(body[i])`
// - parser.ts:133 `!/\s/.test(body[i])`
// - player.ts:280 `startFile.replace(/^scenario\//, "")`
// - player.ts:283 `startLabel.replace(/^\*/, "")`
// - player.ts:540 `target.replace(/^\*/, "")`
// これらは `tests/vm_corpus.rs` が `parser.ts` を実際に実行して間接的にも
// 検証しているが、ここでは同じパターンをピンポイントで再現する。

#[test]
fn corpus_split_line_endings() {
    assert_eq!(
        as_str_vec(&eval_export(r#""a\r\nb\nc\rd".split(/\r\n|\n|\r/)"#)),
        vec!["a", "b", "c", "d"]
    );
}

#[test]
fn corpus_whitespace_test() {
    assert!(eval_bool(r#"/\s/.test(" ")"#));
    assert!(!eval_bool(r#"/\s/.test("x")"#));
}

#[test]
fn corpus_whitespace_or_equals_class_test() {
    assert!(eval_bool(r#"/[\s=]/.test("=")"#));
    assert!(eval_bool(r#"/[\s=]/.test(" ")"#));
    assert!(!eval_bool(r#"/[\s=]/.test("x")"#));
}

#[test]
fn corpus_scenario_prefix_replace() {
    assert_eq!(
        eval_str(r#""scenario/foo.ks".replace(/^scenario\//, "")"#),
        "foo.ks"
    );
}

#[test]
fn corpus_label_star_prefix_replace() {
    assert_eq!(eval_str(r#""*start".replace(/^\*/, "")"#), "start");
    // 先頭に `*` が無ければ何も変わらない。
    assert_eq!(eval_str(r#""start".replace(/^\*/, "")"#), "start");
}

// ============================== ヘルパー ==============================

fn eval_export(expr: &str) -> tsint::value::Value {
    let src = format!("export const result = ({expr});");
    export(&run_ok(&src), "result")
}
