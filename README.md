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
| T1 | レキサ + パーサ | 実装済み |
| T2 | 明示スタックのバイトコード VM（AST → バイトコードへコンパイルし、Rust の呼び出しスタックに頼らずに実行する） | 実装済み（本リポジトリの現状） |
| T3 | Promise / async・イベントループ（Noiria の `await sys.wait()` 等のゲームループ協調を実現） | 未着手（次段階） |
| T4 | 正規表現エンジン本実装（T2 は `.test()`/`split()` に必要な最小サブセットのみ） | 未着手 |
| T5 | Noiria 統合（`deno_core`/V8 を本インタプリタで置き換え、`runtime/` とサンプルゲームを実機で動作させる。import 文によるモジュール連携もここで対応） | 未着手 |

## 使い方

パースのみ（T1 API、引き続き利用可能）:

```rust
let module = tsint::parse_module(source, "runtime/api.ts")?;
println!("{} 文, {} 関数", module.count_statements(), module.count_functions());
```

コンパイル + 実行（T2 API）:

```rust
let mut interp = tsint::Interpreter::new();
interp.register_native("myHostFn", 1, std::rc::Rc::new(|_vm, _this, args| {
    Ok(tsint::value::Value::Number(42.0))
}));
let exports = interp.run_module(source, "runtime/kag/parser.ts")?;
// exports: Vec<(Rc<str>, Value)> — `export` された値の一覧
```

`parse_module` は `Result<ast::Module, ParseError>` を返す。`ParseError` は
`Display` で `構文エラー <行>:<桁>: <メッセージ>` の形式に整形される。
`Interpreter::run_module` は `Result<Vec<(Rc<str>, Value)>, InterpError>`
（`InterpError` はパース/コンパイル/未捕捉例外の 3 種）。
`import` 文を含むモジュールは T2 では明確なコンパイルエラーになる
（モジュール連携は T5 で対応）。

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
  bytecode.rs     T2: 命令セット（OpCode）・Chunk（定数表・行番号表・例外テーブル）・
                   FunctionTemplate/ClassTemplate（クロージャ生成の設計図）
  compiler.rs     T2: AST → バイトコードコンパイラ（スコープ解決・クロージャ捕捉・
                   分割代入・オプショナルチェーン・try/finally インライン展開・
                   for-of/for-in・class・export 表構築）
  value.rs        T2: ランタイム値（Value/ObjData/PropMap/ClassData/MapData/SetData）
  vm.rs           T2: 明示スタック VM 本体（Frame/Vm、実行ループ、例外巻き戻し、
                   プロパティアクセス、JS 演算子セマンティクス）
  builtins.rs     T2: 組み込みオブジェクト・グローバル関数
                   （console/Object/Array/String/Number/Math/JSON/Map/Set/Date/Error 系）
  regex_mini.rs   T2: 正規表現の最小サブセット（本実装は T4）
  lib.rs          公開 API: `parse_module` に加え `Interpreter`
                   （`new`/`register_native`/`run_module` 等）
tests/
  corpus.rs       T1: Noiria の runtime/*.ts・games/*/main.ts を実際にパースする
                   統合テスト（`../noiria` が無い環境では該当ケースを skip）
  unit_syntax.rs  T1: 構文ごとの AST 形状ユニットテスト（71 ケース）
  fuzz.rs         T1: コーパスを LCG で破壊してパニックしないことを確認する
                   エラー耐性テスト（500 ケース）
  common/mod.rs   T1/T2 共有のコーパス読み込みヘルパー
  vm_basics.rs    T2: 機能別ユニットテスト（89 ケース。クロージャ・分割代入・
                   スプレッド・オプショナルチェーン・??=・try/finally・
                   ラベル付き break・class・Map/Set・JSON・new Function・
                   TDZ・例外伝播・文字列/配列メソッド 等）
  vm_corpus.rs    T2: Noiria コーパス実行テスト（`runtime/kag/parser.ts` /
                   `vars.ts` を実行し、Noiria の V8 統合テストと同一の
                   期待値になることを確認。`../noiria` が無ければ skip）
  vm_programs.rs  T2: ミニ JS プログラム 5 本の出力比較テスト
                   （FizzBuzz・クロージャ+メモ化フィボナッチ・バブルソート・
                   private field を使うクラス・Map による単語頻度集計）
  vm_common/mod.rs  T2 テスト共通ヘルパー
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

## T2: バイトコード VM の設計要点

- **命令セット**: `OpCode` は約 75 種類。スタック操作・定数/変数
  アクセス（ローカル/アップバリュー/グローバルを個別命令に分離）・
  プロパティ/添字アクセス・算術/比較/論理演算・オプショナルチェーン
  専用のジャンプ命令・制御フロー・関数/クラス/オブジェクト生成の
  各カテゴリからなる。
- **中断・再開できる VM 構造**: `Vm.frames: Vec<Frame>` がヒープ上の
  明示スタックであり、関数呼び出しは Rust の再帰を使わずこの `Vec` へ
  `Frame` を push/pop するだけで実現する。各 `Frame` は自分専用の
  オペランドスタック・ローカル変数（`Rc<RefCell<Value>>` でボックス化
  済み）・アップバリュー配列を持つ。`Vm::run_until` はただの
  `loop { self.step() }` なので、T3 で「特定の命令（`await` 等）で
  ループを抜けて `Vm` 構造体ごと保持し、後で再開する」実装が自然に
  乗せられる。ネイティブ関数がコールバックを呼ぶ場合のみ Rust の
  再帰（`call_value` の入れ子）を使うが、これはユーザー JS の呼び出し
  深さとは無関係。
- **例外処理**: `try/catch/finally` は例外テーブル方式。`finally` は
  各脱出経路（正常フォールスルー・catch 完了後・break/continue/return・
  例外での巻き戻し）ごとにバイトコードをインライン再コンパイルする
  （コンパイラが `Ctrl::TryFinally` スコープを一時的に取り除いてから
  再帰コンパイルすることで無限展開を防ぐ）。

## T2 で妥協した JS セマンティクス

- `==` の抽象等価演算は主要規則（`null == undefined`・数値⇄文字列⇄真偽値の
  相互変換・オブジェクトの ToPrimitive）のみ実装。Symbol は非対応。
- `for (let i ...)` のループ変数はクロージャ捕捉時に「イテレーションごとに
  新しい束縛」を作らない（`var` 相当の共有セルになる）。
- `class` の `extends`/`super` は未実装（コーパスに無いため明確なエラーに
  する方針。T5 までに必要になれば追加）。
- `computed member`（`obj[expr]`）への複合代入・論理代入（`obj[k] += v` 等）
  は未対応（明確なエラー）。単純な `obj[expr] = v` は対応済み。
- 正規表現は `.test()`/`String.split()` に必要な最小サブセットのみ
  （文字クラス・代替・エスケープショートハンド。量指定子・グループ・
  アンカーは非対応で明確なエラーになる。本実装は T4）。
- 配列の穴（elision, `[1, , 3]`）は `undefined` として扱う（本来の
  「存在しない要素」との区別はしない）。
- `console.log` の整形は簡易版（ネストしたオブジェクト/配列の
  `JSON.stringify` 相当の詳細な表示はしない）。
- `Date.prototype.toLocaleString` は UTC 前提の簡易フォーマット
  （`YYYY-MM-DD HH:MM:SS`）。現在時刻は `Interpreter::set_now_fn` で
  ホストが注入しない限り常に 0（VM 自体は決定的）。
- `async function`/`await` は構文としては受理するが、`await` は
  「Promise らしき値（`then` を持つオブジェクト）ならエラー、それ以外は
  そのまま素通り」という同期的パススルーのみ（T3 で本実装）。

## T3 への引き継ぎメモ

- `Vm.frames`/`Frame` は既に「ヒープ上の明示スタック」なので、await の
  中断点では `OpCode::Await` の実行を「現在の `Frame` 一式を Promise の
  継続として保存し、`run_until` のループを抜ける」ように変更するだけで
  済むはず。マイクロタスクキューと `Vm::run_until` の呼び出しループの
  結合方法を設計すること。
- `is_async` フラグは `ast::Function` に既にあるが `compiler.rs`
  では現状無視している。async 関数の呼び出しが Promise を返すように
  する変更が必要（現状は同期的に本体を実行して戻り値をそのまま返す）。
- ループ変数の per-iteration 束縛（上記の妥協点）は、`await` を含む
  ループで `setTimeout`/`Promise` 的なコールバックがループ変数を
  キャプチャする場合に問題化しうるので、T3 で本気の非同期処理を
  実装する際に合わせて見直すことを推奨。

## テスト

```sh
cargo test                              # 全テスト（T1: 71 + T2 単体: 89 + コーパス: 4 + プログラム: 5 + 内蔵: 数件）
cargo clippy --all-targets -- -D warnings
cargo fmt --all
```

コーパステスト・fuzz テストは `../noiria`（Noiria リポジトリ）が
存在する前提。無い環境では該当ケースを skip するか、fuzz テストは
内蔵の合成ソースにフォールバックする。
