//! テスト間で共有する小さなヘルパー（`tests/common/mod.rs` は cargo に
//! 独立したテストバイナリとして扱われないため、複数の統合テストから
//! `mod common;` で読み込める）。

use std::path::{Path, PathBuf};

pub fn noiria_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("noiria")
}

pub fn corpus_files() -> Vec<&'static str> {
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

/// コーパスのソース文字列一覧 (相対パス, 内容) を読み込む。
/// Noiria リポジトリが無い環境では、代わりに小さな合成 TS ソース集合を返す
/// （fuzz テストが環境に依存せず常に一定件数を実行できるようにするため）。
pub fn read_corpus_sources() -> Vec<(String, String)> {
    let root = noiria_root();
    let mut out = Vec::new();
    if root.exists() {
        for rel in corpus_files() {
            let path = root.join(rel);
            if let Ok(src) = std::fs::read_to_string(&path) {
                out.push((rel.to_string(), src));
            }
        }
    }
    if out.is_empty() {
        out.push(("synthetic1.ts".to_string(), SYNTHETIC_SOURCE_1.to_string()));
        out.push(("synthetic2.ts".to_string(), SYNTHETIC_SOURCE_2.to_string()));
    }
    out
}

const SYNTHETIC_SOURCE_1: &str = r#"
import { KagPlayer } from "noiria:kag";
import * as api from "noiria:api";

export interface Options {
  x?: number;
  y?: number;
}

export class Widget {
  private count = 0;
  constructor(public name: string) {}
  get value(): number { return this.count; }
  async run(opts: Options = {}): Promise<void> {
    for (const [k, v] of Object.entries(opts)) {
      if (v === undefined) continue;
      this.count += typeof v === "number" ? v : 0;
    }
    await api.sys.wait(10);
  }
}

async function main() {
  const w = new Widget("x");
  try {
    await w.run({ x: 1, y: 2 });
  } catch (e) {
    console.log(`error: ${e}`);
  } finally {
    console.log("done");
  }
}

await main();
"#;

const SYNTHETIC_SOURCE_2: &str = r#"
type Page = "fore" | "back";
const table: Record<string, (n: number) => number> = {
  double: (n) => n * 2,
  square(n) { return n ** 2; },
};

outer: for (let i = 0; i < 10; i++) {
  switch (i % 3) {
    case 0:
      continue outer;
    case 1: {
      const r = /a[bc]+\/d/.test(String(i));
      if (r) break outer;
      break;
    }
    default:
      table.double?.(i) ?? 0;
  }
}
"#;
