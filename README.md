# tsint

完全自作の TypeScript インタプリタ。ノベルゲームエンジン **Noiria**
（`../noiria`）が現在 `deno_core`/V8 に依存している部分を、最終的に
このインタプリタで置き換えることを目標とする。

汎用の JS/TS エンジンを目指すものではない。**Noiria の `runtime/` と
ゲームスクリプト（`games/*/main.ts`）を実際に動かせること**を基準に、
対応する構文・機能をコーパス駆動で決めていく。

依存クレートはゼロ（`Cargo.toml` に `[dependencies]` の中身なし、
`dev-dependencies` もなし）。標準ライブラリのみで完結する。

## 段階表

| 段階 | 内容 | 状態 |
| --- | --- | --- |
| T1 | レキサ + パーサ（本リポジトリの現状） | 実装済み |
| T2 | 明示スタックのバイトコード VM（AST → バイトコードへコンパイルし、Rust の呼び出しスタックに頼らずに実行する） | 未着手 |
| T3 | Promise / async・イベントループ（Noiria の `await sys.wait()` 等のゲームループ協調を実現） | 未着手 |
| T4 | 正規表現エンジン（T1 では正規表現は「トークンとして正しく読める」だけで、マッチングは未実装） | 未着手 |
| T5 | Noiria 統合（`deno_core`/V8 を本インタプリタで置き換え、`runtime/` とサンプルゲームを実機で動作させる） | 未着手 |

## 使い方

```rust
let module = tsint::parse_module(source, "runtime/api.ts")?;
println!("{} 文, {} 関数", module.count_statements(), module.count_functions());
```

`parse_module` は `Result<ast::Module, ParseError>` を返す。`ParseError` は
`Display` で `構文エラー <行>:<桁>: <メッセージ>` の形式に整形される。

## リポジトリ構成

```
src/
  lexer.rs        トークナイザ（正規表現/除算の曖昧性解決、テンプレート
                   リテラルのネスト、ASI 用の改行フラグを持つトークン列を生成）
  ast.rs          AST 定義（NodeId・Span 付き。文/関数/式の再帰統計ヘルパー）
  parser.rs       Parser 本体（トークン操作・エントリポイント）と共通ヘルパー
  parser/
    expr.rs       式（Pratt 優先順位パース、アロー関数の投機的判定、
                   new/メンバ/呼び出し/オプショナルチェーン等）
    stmt.rs       文（制御構造・class・import/export・TS declare）
    pattern.rs    分割代入パターン（var 宣言・引数・catch・for-of/in で共有）
    types.rs      TS の型注釈・interface・generics を「読み飛ばす」ための
                   ヘルパー群（構造化した型 AST は作らない）
  lib.rs          公開 API: `parse_module(source, name) -> Result<Module, ParseError>`
tests/
  corpus.rs       Noiria の runtime/*.ts・games/*/main.ts を実際にパースする
                   統合テスト（`../noiria` が無い環境では該当ケースを skip）
  unit_syntax.rs  構文ごとの AST 形状ユニットテスト（71 ケース）
  fuzz.rs         コーパスを LCG で破壊してパニックしないことを確認する
                   エラー耐性テスト（500 ケース）
  common/mod.rs   上記テスト間で共有するコーパス読み込みヘルパー
```

## 対応構文一覧

コーパス（Noiria の `runtime/api.ts`, `runtime/kag/*.ts`,
`runtime/noiria.d.ts`, `games/*/main.ts`）を読んで洗い出した構文。

### 文・宣言

- `const` / `let` / `var`（`var` はコーパスには出現しないが対応）
- 関数宣言・関数式・アロー関数（`async` 含む、裸パラメータ 1 個の
  アロー、括弧付き複数パラメータ、型注釈・デフォルト値・rest 引数）
- `class`（メソッド・getter/setter・static・private `#field`・
  コンストラクタ・フィールド初期化子・`extends`。TS の
  `public`/`private`/`protected`/`readonly`/`abstract`/`override` 修飾子は
  読み飛ばす）
- `if`/`else`、`while`、`do...while`、`for`（古典形・`for...of`・
  `for...in`・`for await...of`）、`switch`/`case`/`default`
- `try`/`catch`（バインディング省略可）/`finally`（`catch` 単独・
  `finally` 単独・両方の組み合わせすべて対応）
- `throw`、`return`、`break`/`continue`（ラベル付き含む）、ラベル文
- `import`（default / named（rename 含む）/ namespace(`* as`) /
  side-effect only / `import type`）
- `export`（named（rename・re-export `from` 含む）/ `export * from` /
  `export * as ns from` / `export default`（式・関数・クラス）/
  `export const/let/function/class/interface/type` / `export type { .. }`）
- `interface` 宣言・`type` エイリアス（パースして名前と span だけ
  AST に残し、詳細は捨てる）
- `declare module "..."` / `declare namespace ...`（アンビエント宣言。
  本体なしの関数シグネチャ `f(): void;` にも対応）

### 式

- テンプレートリテラル（`${}` の任意ネスト対応）
- 分割代入（オブジェクト/配列、ネスト、デフォルト値、rest）とスプレッド
- オプショナルチェーン `?.`（プロパティ・添字・呼び出し）、`??`、`??=`
- 三項演算子、論理演算子（`&&` `||`）、ビット演算子一式、比較演算子、
  べき乗 `**`（右結合）、`typeof`/`instanceof`/`in`/`delete`/`void`
- `new`（`new.target` の読み飛ばし込み）、ジェネリック呼び出し
  `foo<T>(...)` / `new Foo<T>()` の投機的パース
- `async`/`await`（トップレベル await も含む。Noiria のエントリポイントが
  モジュールトップで `await player.run(...)` する形に対応）
- 正規表現リテラル（トークンとして正しく読み取る。マッチング実装は T4）
- オブジェクトリテラル（ショートハンド・メソッド省略記法・
  getter/setter・computed key・spread）
- TS の `as`/`satisfies` キャスト・非 null アサーション `!`
  （パース後は型情報を捨て、`TsAs`/`TsNonNull` として痕跡だけ残す）

### 非対応（コーパスに現れないため T1 では見送り）

- ジェネレータ（`function*`、`yield`）
- `enum`
- デコレータ（`@decorator`）
- 複雑な条件型・mapped 型などの高度な型演算（`skip_type` は寛容な
  フォールバックで読み飛ばすため構文エラーにはならないが、型情報として
  構造化はしない）
- タグ付きテンプレートリテラル（`` tag`...` ``）

## テスト

```sh
cargo test                              # 全テスト
cargo clippy --all-targets -- -D warnings
cargo fmt --all
```

コーパステスト・fuzz テストは `../noiria`（Noiria リポジトリ）が
存在する前提。無い環境では該当ケースを skip するか、fuzz テストは
内蔵の合成ソースにフォールバックする。
