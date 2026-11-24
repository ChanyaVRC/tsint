use super::engine::MAX_LOOK_DEPTH;
use super::*;

fn m(pattern: &str, flags: &str, s: &str) -> Option<(usize, usize)> {
    let re = Regex::compile(pattern, flags).unwrap();
    let chars: Vec<char> = s.chars().collect();
    re.search(&chars, 0).unwrap().map(|r| (r.start, r.end))
}

fn groups(pattern: &str, flags: &str, s: &str) -> Vec<Option<String>> {
    let re = Regex::compile(pattern, flags).unwrap();
    let chars: Vec<char> = s.chars().collect();
    let mat = re.search(&chars, 0).unwrap().unwrap();
    mat.groups
        .iter()
        .map(|g| g.map(|(a, b)| chars[a..b].iter().collect()))
        .collect()
}

// ---------------- リテラル・文字クラス ----------------

#[test]
fn literal_char_match() {
    assert_eq!(m("abc", "", "xxabcyy"), Some((2, 5)));
}

#[test]
fn dot_matches_any_but_newline() {
    assert_eq!(m(".", "", "a"), Some((0, 1)));
    assert_eq!(m("^.$", "", "\n"), None);
}

#[test]
fn dot_all_flag_matches_newline() {
    assert_eq!(m("^.$", "s", "\n"), Some((0, 1)));
}

#[test]
fn digit_shorthand() {
    assert_eq!(m(r"\d+", "", "ab123cd"), Some((2, 5)));
}

#[test]
fn not_digit_shorthand() {
    assert_eq!(m(r"\D+", "", "12ab34"), Some((2, 4)));
}

#[test]
fn word_shorthand_is_ascii_only() {
    // JS の \w は ASCII のみ（Unicode の英字は含まない）。
    assert_eq!(m(r"\w+", "", "あabあ"), Some((1, 3)));
}

#[test]
fn whitespace_shorthand() {
    assert!(Regex::compile(r"\s", "").unwrap().test(" ").unwrap());
    assert!(Regex::compile(r"\s", "").unwrap().test("\t").unwrap());
    assert!(!Regex::compile(r"\s", "").unwrap().test("a").unwrap());
}

#[test]
fn char_class_range_and_negate() {
    assert_eq!(m("[a-c]+", "", "xxabcxx"), Some((2, 5)));
    assert_eq!(m("[^a-c]+", "", "abcXYZabc"), Some((3, 6)));
}

#[test]
fn char_class_with_shorthands() {
    assert_eq!(m(r"[\d\s]+", "", "ab12 3cd"), Some((2, 6)));
}

#[test]
fn char_class_literal_bracket_close_must_be_escaped() {
    assert_eq!(m(r"[\]]", "", "]"), Some((0, 1)));
}

// ---------------- 量指定子 ----------------

#[test]
fn star_greedy_takes_maximum() {
    assert_eq!(m("a*", "", "aaab"), Some((0, 3)));
}

#[test]
fn star_lazy_takes_minimum() {
    assert_eq!(m("a*?", "", "aaab"), Some((0, 0)));
}

#[test]
fn plus_requires_at_least_one() {
    assert_eq!(m("a+", "", "bbb"), None);
    assert_eq!(m("a+", "", "aab"), Some((0, 2)));
}

#[test]
fn question_mark_optional() {
    assert_eq!(m("colou?r", "", "color"), Some((0, 5)));
    assert_eq!(m("colou?r", "", "colour"), Some((0, 6)));
}

#[test]
fn brace_exact_count() {
    assert_eq!(m("a{3}", "", "aaaa"), Some((0, 3)));
    assert_eq!(m("a{3}", "", "aa"), None);
}

#[test]
fn brace_min_only() {
    assert_eq!(m("a{2,}", "", "aaaaa"), Some((0, 5)));
}

#[test]
fn brace_min_max() {
    assert_eq!(m("a{2,4}", "", "aaaaaa"), Some((0, 4)));
}

#[test]
fn brace_lazy() {
    assert_eq!(m("a{2,4}?", "", "aaaaaa"), Some((0, 2)));
}

#[test]
fn invalid_brace_range_is_error() {
    assert!(Regex::compile("a{4,2}", "").is_err());
}

#[test]
fn unrecognized_brace_is_literal() {
    // `{` が量指定子として解釈できない場合はリテラル文字として扱う。
    assert_eq!(m(r"a\{foo\}", "", "a{foo}"), Some((0, 6)));
}

#[test]
fn nothing_to_repeat_is_error() {
    assert!(Regex::compile("*abc", "").is_err());
}

#[test]
fn nullable_star_does_not_infinite_loop() {
    // `(a?)*` は空文字列にもマッチでき、無限ループしないことを確認する。
    let re = Regex::compile("(a?)*", "").unwrap();
    let chars: Vec<char> = "".chars().collect();
    let r = re.search(&chars, 0).unwrap().unwrap();
    assert_eq!((r.start, r.end), (0, 0));
}

// ---------------- グループ・選択 ----------------

#[test]
fn alternation_priority_left_to_right() {
    // 選択は左から順に試し、最初に成功した枝を採用する（最長一致ではない）。
    assert_eq!(m("a|ab", "", "ab"), Some((0, 1)));
}

#[test]
fn non_capturing_group_does_not_capture() {
    let re = Regex::compile("(?:abc)(def)", "").unwrap();
    let chars: Vec<char> = "abcdef".chars().collect();
    let r = re.search(&chars, 0).unwrap().unwrap();
    assert_eq!(r.groups.len(), 1);
}

#[test]
fn nested_group_capture_numbering_is_by_open_paren_order() {
    let g = groups(r"((a)(b))(c)", "", "abc");
    assert_eq!(
        g,
        vec![
            Some("ab".into()),
            Some("a".into()),
            Some("b".into()),
            Some("c".into())
        ]
    );
}

#[test]
fn optional_group_capture_is_undefined_when_not_participating() {
    let g = groups(r"(a)(b)?", "", "a");
    assert_eq!(g, vec![Some("a".into()), None]);
}

// ---------------- アンカー・単語境界 ----------------

#[test]
fn caret_dollar_without_multiline_match_whole_string_only() {
    assert_eq!(m("^abc$", "", "abc"), Some((0, 3)));
    assert_eq!(m("^abc$", "", "xabc"), None);
    assert_eq!(m("^foo$", "", "foo\n"), None);
}

#[test]
fn caret_dollar_with_multiline_match_line_boundaries() {
    assert_eq!(m("^b", "m", "a\nb"), Some((2, 3)));
    assert_eq!(m("a$", "m", "a\nb"), Some((0, 1)));
}

#[test]
fn word_boundary_basic() {
    assert_eq!(m(r"\bcat\b", "", "a cat sat"), Some((2, 5)));
    assert_eq!(m(r"\bcat\b", "", "concatenate"), None);
}

#[test]
fn non_word_boundary_basic() {
    assert_eq!(m(r"\Bcat", "", "concatenate"), Some((3, 6)));
}

// ---------------- 後方参照 ----------------

#[test]
fn backreference_matches_same_text() {
    assert_eq!(m(r"(\w+) \1", "", "hello hello"), Some((0, 11)));
    assert_eq!(m(r"(\w+) \1", "", "hello world"), None);
}

#[test]
fn backreference_to_unmatched_group_is_empty() {
    // JS 仕様: 未マッチのグループへの後方参照は空文字列にマッチする。
    assert_eq!(m(r"(a)?\1b", "", "b"), Some((0, 1)));
}

#[test]
fn invalid_backreference_number_is_compile_error() {
    assert!(Regex::compile(r"\1", "").is_err());
}

// ---------------- 先読み ----------------

#[test]
fn positive_lookahead() {
    assert_eq!(m(r"foo(?=bar)", "", "foobar"), Some((0, 3)));
    assert_eq!(m(r"foo(?=bar)", "", "foobaz"), None);
}

#[test]
fn negative_lookahead() {
    assert_eq!(m(r"foo(?!bar)", "", "foobaz"), Some((0, 3)));
    assert_eq!(m(r"foo(?!bar)", "", "foobar"), None);
}

#[test]
fn lookbehind_is_unsupported_error() {
    assert!(Regex::compile(r"(?<=foo)bar", "").is_err());
    assert!(Regex::compile(r"(?<!foo)bar", "").is_err());
}

#[test]
fn named_group_is_unsupported_error() {
    assert!(Regex::compile(r"(?<name>foo)", "").is_err());
}

// ---------------- フラグ ----------------

#[test]
fn ignore_case_flag() {
    assert_eq!(m("abc", "i", "ABC"), Some((0, 3)));
}

#[test]
fn unsupported_flag_is_error() {
    assert!(Regex::compile("a", "y").is_err());
    assert!(Regex::compile("a", "d").is_err());
}

#[test]
fn duplicate_flag_is_error() {
    assert!(Regex::compile("a", "gg").is_err());
}

// ---------------- 元コーパスの正規表現 ----------------

#[test]
fn corpus_line_ending_alternation() {
    let re = Regex::compile(r"\r\n|\n|\r", "").unwrap();
    assert!(re.test("a\r\nb").unwrap());
}

#[test]
fn corpus_leading_whitespace_test() {
    let re = Regex::compile(r"\s", "").unwrap();
    assert!(re.test(" ").unwrap());
    assert!(!re.test("x").unwrap());
}

#[test]
fn corpus_scenario_prefix_replace_pattern() {
    assert_eq!(m(r"^scenario/", "", "scenario/foo.ks"), Some((0, 9)));
}

#[test]
fn corpus_label_star_prefix_pattern() {
    assert_eq!(m(r"^\*", "", "*start"), Some((0, 1)));
    assert_eq!(m(r"^\*", "", "start"), None);
}

// ---------------- ステップ上限・アンドゥログ・スタック保護 ----------------

#[test]
fn catastrophic_backtracking_hits_step_limit() {
    let re = Regex::compile(r"^(a+)+$", "").unwrap();
    let s = "a".repeat(35) + "!";
    let chars: Vec<char> = s.chars().collect();
    let err = re.search(&chars, 0).unwrap_err();
    assert!(err.0.contains("ステップ数") || err.0.contains("再帰の深さ"));
}

#[test]
fn redos_unanchored_nested_plus_completes_fast_without_hang_or_overflow() {
    let re = Regex::compile(r"(a+)+b", "").unwrap();
    let s = "a".repeat(30) + "c";
    let chars: Vec<char> = s.chars().collect();
    let start = std::time::Instant::now();
    let err = re.search(&chars, 0).unwrap_err();
    assert!(err.0.contains("ステップ数") || err.0.contains("再帰の深さ"));
    assert!(
        start.elapsed() < std::time::Duration::from_secs(5),
        "ReDoS 検出に時間がかかりすぎています: {:?}",
        start.elapsed()
    );
}

#[test]
fn deep_repetition_does_not_stack_overflow() {
    let s = "a".repeat(5000);
    assert_eq!(m(r"^a{5000}$", "", &s), Some((0, 5000)));
    assert_eq!(m(r"^a{0,5000}$", "", &s), Some((0, 5000)));
    let s10k = "a".repeat(10_000);
    assert_eq!(m(r"^a+$", "", &s10k), Some((0, 10_000)));
}

#[test]
fn backtracking_rolls_back_captures_from_failed_branch() {
    assert_eq!(
        groups(r"^(?:(a)(b)c|(a)bd)$", "", "abd"),
        vec![None, None, Some("a".into())]
    );
    assert_eq!(
        groups(r"^(a+)(a+)b$", "", "aaaab"),
        vec![Some("aaa".into()), Some("a".into())]
    );
    assert_eq!(
        groups(r"^(a|ab)+(c)$", "", "ababc"),
        vec![Some("ab".into()), Some("c".into())]
    );
}

#[test]
fn backreference_with_backtracking_and_trail_rewind() {
    assert_eq!(groups(r"^(\w+)\1$", "", "abab"), vec![Some("ab".into())]);
    assert_eq!(m(r"^(\w+)\1$", "", "abac"), None);
}

#[test]
fn positive_lookahead_commits_captures_and_rolls_back_on_outer_backtrack() {
    assert_eq!(groups(r"(?=(a+))\1b", "", "aaab"), vec![Some("aaa".into())]);
    assert_eq!(
        groups(r"^(?:(?=(a))x|(?=(ab))ab)$", "", "ab"),
        vec![None, Some("ab".into())]
    );
    assert_eq!(
        groups(r"^(?=(?=(ab))a)ab$", "", "ab"),
        vec![Some("ab".into())]
    );
}

#[test]
fn negative_lookahead_rolls_back_captures_on_both_success_and_failure() {
    assert_eq!(
        groups(r"^(?!(a)x)(a)y$", "", "ay"),
        vec![None, Some("a".into())]
    );
    assert_eq!(groups(r"^(?:(?!(a))x|a)$", "", "a"), vec![None]);
}

#[test]
fn deeply_nested_lookahead_hits_depth_limit_cleanly() {
    let depth = MAX_LOOK_DEPTH + 10;
    let pat = format!("{}{}{}", "(?=".repeat(depth), "a", ")".repeat(depth));
    let re = Regex::compile(&pat, "").unwrap();
    let chars: Vec<char> = "a".chars().collect();
    let err = re.search(&chars, 0).unwrap_err();
    assert!(err.0.contains("再帰の深さ"));
}
