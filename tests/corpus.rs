//! コーパステスト: Noiria リポジトリの runtime/*.ts と games/*/main.ts を
//! すべてパースし、成功することを確認する。
//!
//! Noiria リポジトリは `../noiria`（tsint と同じ親ディレクトリ）にある前提。
//! 存在しない環境（CI で Noiria を持たない等）では該当ケースをスキップする。

use std::path::{Path, PathBuf};

fn noiria_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("noiria")
}

/// コーパス対象ファイル一覧（相対パス）。
fn corpus_files() -> Vec<&'static str> {
    vec![
        "runtime/api.ts",
        "runtime/noiria.d.ts",
        "runtime/kag/mod.ts",
        "runtime/kag/parser.ts",
        "runtime/kag/player.ts",
        "runtime/kag/tags.ts",
        "runtime/kag/vars.ts",
        "games/sample/main.ts",
        "games/testbed/main.ts",
    ]
}

#[test]
fn parses_all_noiria_corpus_files() {
    let root = noiria_root();
    if !root.exists() {
        eprintln!(
            "skip: Noiria リポジトリが見つかりません ({}). コーパステストをスキップします。",
            root.display()
        );
        return;
    }

    let mut total_stmts = 0usize;
    let mut total_funcs = 0usize;
    let mut checked = 0usize;

    for rel in corpus_files() {
        let path = root.join(rel);
        if !path.exists() {
            eprintln!("skip: {} が存在しません", path.display());
            continue;
        }
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{} の読み込みに失敗: {e}", path.display()));

        let module = match tsint::parse_module(&source, rel) {
            Ok(m) => m,
            Err(e) => {
                panic!(
                    "コーパスのパースに失敗しました: {}\n{}\n(該当行付近を確認してください)",
                    path.display(),
                    e
                );
            }
        };

        let stmts = module.count_statements();
        let funcs = module.count_functions();
        println!(
            "{rel}: 文={stmts} 関数={funcs} node_count={}",
            module.node_count
        );

        assert!(
            stmts > 0,
            "{rel}: 文が 0 件でした（パースが空になっている可能性）"
        );
        assert!(module.node_count > 0, "{rel}: node_count が 0 でした");

        total_stmts += stmts;
        total_funcs += funcs;
        checked += 1;
    }

    assert!(checked > 0, "コーパスファイルが 1 件も見つかりませんでした");
    assert!(total_stmts > 0, "全コーパス合計の文数が 0 でした");
    println!(
        "=== コーパス統計: ファイル数={checked} 総文数={total_stmts} 総関数数={total_funcs} ==="
    );
}

/// runtime/kag/player.ts は最も構文的に複雑なファイル（ラベル付き break、
/// getter、static readonly、非同期メソッド等）なので個別に統計を検証する。
#[test]
fn parses_player_ts_with_expected_richness() {
    let root = noiria_root();
    let path = root.join("runtime/kag/player.ts");
    if !path.exists() {
        eprintln!("skip: {} が見つかりません", path.display());
        return;
    }
    let source = std::fs::read_to_string(&path).unwrap();
    let module = tsint::parse_module(&source, "runtime/kag/player.ts")
        .unwrap_or_else(|e| panic!("パース失敗: {e}"));

    assert!(
        module.count_functions() >= 20,
        "player.ts の関数数が想定より少ない"
    );
    assert!(
        module.count_statements() >= 100,
        "player.ts の文数が想定より少ない"
    );
}
