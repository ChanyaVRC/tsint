# tsint

[![crates.io](https://img.shields.io/crates/v/tsint.svg)](https://crates.io/crates/tsint)
[![API Docs](https://img.shields.io/badge/docs-GitHub%20Pages-blue)](https://chanyavrc.github.io/tsint/)
[![License: MIT](https://img.shields.io/badge/license-MIT-green.svg)](LICENSE)

完全自作・ゼロ依存（`std` のみ）の TypeScript インタプリタ。ノベルゲームエンジン
**[Noiria](https://github.com/ChanyaVRC/noiria)** のデフォルト配布（`dist`）
スクリプトエンジンとして採用されており、Noiria 本体から crates.io 上の
`tsint = "0.1.0"` として利用されている。

- **リポジトリ**: https://github.com/ChanyaVRC/tsint （`main` ブランチ、MIT ライセンス）
- **crates.io**: https://crates.io/crates/tsint
- **API ドキュメント（rustdoc）**: https://chanyavrc.github.io/tsint/
- **Noiria エコシステム**: [Noiria エンジン](https://github.com/ChanyaVRC/noiria) /
  [Noiria Wiki & Website](https://chanyavrc.github.io/noiria-site/)

汎用の JS/TS エンジンを目指すものではなく、**Noiria の `runtime/` と
ゲームスクリプト（`games/*/main.ts`）を実用的かつ高速・安全に動かせること**を
基準に、対応する構文・機能と最適化をコーパス駆動で整備している。

依存クレートはゼロ（`Cargo.toml` に `[dependencies]` の中身なし、
`dev-dependencies` もなし）。Rust 標準ライブラリのみで完結する。

## インストール

```sh
cargo add tsint
```

または `Cargo.toml` に以下を追加する。

```toml
[dependencies]
tsint = "0.1.0"
```

## 段階表

| 段階 | 内容 | 状態 |
| --- | --- | --- |
| T1 | レキサ + パーサ | 実装済み |
| T2 | 明示スタックのバイトコード VM（AST → バイトコードへコンパイルし、Rust の呼び出しスタックに頼らずに実行する。非キャプチャローカル変数の非ボックス化・O(1) `PropMap` 索引化・循環参照 GC を含む） | 実装済み |
| T3 | Promise / async・await・イベントループ（Noiria の `await sys.wait()` 等のゲームループ協調を実現） | 実装済み |
| T4 | 正規表現エンジン本実装（明示バックトラックスタック + アンドゥログによる反復実行器。量指定子・グループ・キャプチャ・アンカー・後方参照・先読みに対応） | 実装済み |
| T5a | ES モジュールシステム（`import`/`export` の実行時解決・トップレベル `await` 対応のモジュール評価）+ Noiria の KAG ランタイム全体を tsint 上で完走させる統合テスト | 実装済み |
| T5b | Noiria 本体への組み込み（デフォルト `dist` スクリプトエンジンとして採用、`deno_core` ops 互換配線・V8 並走比較・crates.io `tsint = "0.1.0"` 依存） | 実装済み |

## 使い方

パースのみ（T1 API）:

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
`import` 文を含むモジュールを `run_module`（単一ファイル API）に直接
渡すと明確なコンパイルエラーになる。複数モジュールにまたがる
`import`/`export` は次節のモジュールシステム（T5a）を使うこと。

Promise / async・await / イベントループ（T3 API）:

```rust
let mut interp = tsint::Interpreter::new();
// ホストが後から resolve/reject するタイマー的な非同期 API を登録する。
interp.register_native_async("wait", 1, |vm, _this, args, handle| {
    // handle（HostPromise）をどこかに保持しておき、後で
    // handle.resolve(vm, value) / handle.reject(vm, err) を呼ぶ。
});
interp.run_module("await wait(100);", "main.ts")?; // トップレベル await も可
// ホスト側のゲームループから 1 フレームごとに呼び、マイクロタスク
// （Promise チェーン・await の再開）を汲み尽くす。
match interp.run_event_loop() {
    tsint::vm::PumpState::Idle => { /* これ以上進む処理は無い */ }
    tsint::vm::PumpState::Waiting => { /* HostPromise の解決待ち */ }
}
```

`run_module` はモジュール本体の実行後、必ず 1 回マイクロタスクキューを
汲み尽くしてから戻る（`Promise.resolve().then(...)` のようなコードが
`run_module` 呼び出しの中で完了することを保証する）。トップレベル
`await` がマイクロタスクだけで解決するなら（Promise チェーンのみに
依存するなら）`run_module` はそれも待ってから戻る。ホスト側の
`HostPromise` の解決を待ったまま残った場合はエラーになる（同期 API の
限界。ホスト駆動の非同期実行には `run_module_graph` + `run_event_loop` を使うこと）。

ES モジュールシステム（T5a API、`src/module.rs`）:

```rust
struct MyHost { /* ファイルシステム等へのアクセス */ }
impl tsint::module::ModuleHost for MyHost {
    fn resolve(&self, specifier: &str, referrer: &str) -> Result<String, String> { /* ... */ }
    fn load(&self, resolved: &str) -> Result<String, String> { /* ... */ }
}

let mut interp = tsint::Interpreter::new();
let host = MyHost { /* ... */ };
match interp.run_module_graph("entry-specifier", &host)? {
    tsint::module::ModuleOutcome::Settled(exports) => { /* エントリが完全に解決した */ }
    tsint::module::ModuleOutcome::Pending => {
        // トップレベル await がホスト Promise 待ちのまま残っている
        // （KagPlayer.run() のような終わらないゲームループを抱える
        // main.ts が典型例）。host promise を解決しつつ
        // interp.run_event_loop() を繰り返し呼んで駆動を続ける。
    }
}
```

循環参照 GC の明示呼び出し（通常はセーフポイントで自動実行される）:

```rust
let collected = interp.collect_garbage(); // 回収した循環オブジェクト数
interp.vm().set_gc_threshold(4096);       // 自動 GC 閾値のチューニングも可能
```

モジュールシステムの設計の要点:

- `import`/`export ... from`/`export * (as ns)` を含む AST を、依存
  モジュールの評価が終わった後に「名前空間オブジェクトを保持する
  グローバル変数から `const` で読み出す」形へ **AST レベルで書き換えて
  から** `compiler::compile_module` に渡す。コンパイラ・VM 本体
  （`compiler.rs`/`bytecode.rs`/`vm.rs`）への侵襲を最小限に抑えている
  （`vm.rs` 側は `Vm::start_module_task`/`ModuleTaskState` による
  未解決トップレベル `await` 許容経路のみ）。
- モジュールは resolved specifier ごとに 1 回だけ評価してキャッシュする
  （ダイヤモンド依存でも評価は 1 回）。
- 循環 import は「参照した時点でまだ埋まっていないプロパティは
  `undefined`」という簡易挙動にする（名前空間オブジェクトを評価前に
  空で確保してからキャッシュ登録し、評価完了後にプロパティを書き込む
  ため）。また named import はモジュール評価完了時の値スナップショットであり、
  `export let` を後から再代入した場合のライブバインディングには対応していない。
  Noiria コーパスの循環は型のみ（`import type`）なので実害はない。
- `import type` / `export type { ... } from` はまるごと無視する
  （依存モジュールを解決すらしない。存在しないファイルを指す
  `.d.ts` 由来の型 import があっても壊れない）。値と型が同じ
  `import { real, TypeOnlyThing } from "mod"` に混在していても
  （TS の既定動作どおり）安全（型名は単に `undefined` を指す未使用の
  `const` になるだけ）。
- 依存モジュール（エントリ以外）はトップレベル await を使わないという
  Noiria コーパスの前提の下、`Vm::run_module` と同じ「完全に解決する
  はず」経路で評価する。エントリモジュールだけ `Vm::start_module_task`
  を使い、`ModuleOutcome::Pending` を許す。

## リポジトリ構成

```
.github/workflows/
  ci.yml          CI（rustfmt + clippy -D warnings、Ubuntu/Windows debug & release、
                  ネイティブ aarch64 テスト）
  docs.yml        GitHub Pages への rustdoc 自動ビルド・公開
  release.yml     vX.Y.Z タグ駆動のパッケージ検証・crates.io 公開・GitHub Release 作成
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
  bytecode.rs     T2: 命令セット（OpCode）・Chunk（定数表・行番号表・例外テーブル・
                  captured_locals 解析結果）・FunctionTemplate/ClassTemplate
  compiler.rs     T2: AST → バイトコードコンパイラ（スコープ解決・captured_locals
                  解析・クロージャ捕捉・分割代入・オプショナルチェーン・
                  try/finally インライン展開・for-of/for-in・class・export 表構築）
  value.rs        T2/T3: ランタイム値（Value/ObjData/PropMap［小オブジェクト線形高速パス +
                  遅延 HashMap O(1) 索引］/ClassData/MapData/SetData/
                  PromiseData・PromiseReaction・AwaitContinuation）
  vm.rs           T2/T3: 明示スタック VM 本体（Frame/LocalSlot::{Direct, Boxed}/Vm、
                  実行ループ、例外巻き戻し、プロパティアクセス、JS 演算子セマンティクス、
                  Promise 解決手続き、マイクロタスクキュー、async タスク駆動、
                  HostPromise、弱参照レジストリ + trial-deletion 循環参照 GC）
  builtins.rs     T2/T3: 組み込みオブジェクト・グローバル関数
                  （console/Object/Array/String/Number/Math/JSON/Map/Set/Date/Error/
                  Promise/RegExp 系）
  regex.rs        T4: 正規表現エンジン本実装（パーサ + AST + Save/Jmp/Split
                  命令列へのコンパイル + 明示ヒープスタック＆アンドゥログ反復実行器）
  module.rs       T5a: ES モジュールシステム（`ModuleHost` トレイト・
                  import/export の AST 書き換え・モジュールキャッシュ・
                  循環 import の簡易挙動・`Interpreter::run_module_graph`）
  lib.rs          公開 API: `parse_module` に加え `Interpreter`
                  （`new`/`register_native`/`register_native_async`/`run_module`/
                  `run_module_graph`/`run_event_loop`/`collect_garbage` 等）
tests/
  corpus.rs       T1: Noiria の runtime/*.ts・games/*/main.ts を実際にパースする
                  統合テスト（2 ケース。`../noiria` が無い環境では skip）
  unit_syntax.rs  T1: 構文ごとの AST 形状ユニットテスト（71 ケース）
  fuzz.rs         T1: コーパスを LCG で破壊してパニックしないことを確認する
                  エラー耐性テスト（1 テスト内で 500 ケース）
  common/mod.rs   T1/T2 共有のコーパス読み込みヘルパー
  vm_basics.rs    T2: 機能別ユニットテスト（90 ケース。クロージャ・分割代入・
                  スプレッド・オプショナルチェーン・??=・try/finally・
                  ラベル付き break・class・Map/Set・JSON・new Function・
                  TDZ・例外伝播・文字列/配列メソッド 等）
  vm_corpus.rs    T2: Noiria コーパス実行テスト（4 ケース。`runtime/kag/parser.ts` /
                  `vars.ts` を実行し、Noiria の V8 統合テストと同一の
                  期待値になることを確認。`../noiria` が無ければ skip）
  vm_programs.rs  T2: ミニ JS プログラム 5 本の出力比較テスト（5 ケース。
                  FizzBuzz・クロージャ+メモ化フィボナッチ・バブルソート・
                  private field を使うクラス・Map による単語頻度集計）
  vm_async.rs     T3: Promise・マイクロタスクキュー・async/await・イベントループの
                  テスト（77 ケース。順序テスト・Noiria パターンの再現・
                  並走タスクのフレーム分離・T2 の妥協解消の回帰テスト 等）
  vm_regex.rs     T4: RegExp/String.prototype.{match,replace,replaceAll,split,
                  search} の JS 挙動一致テスト（54 ケース。Node.js での実行
                  結果とコメントで突き合わせている値を含む。Noiria コーパス
                  由来の正規表現の再現・壊滅的バックトラックのステップ上限
                  テストも含む）
  vm_common/mod.rs  T2/T3/T4 テスト共通ヘルパー
  module_basics.rs  T5a: モジュールシステムのユニットテスト（13 ケース。
                  named/default/namespace import・`export ... from`・
                  `export * as ns from`・`export * from`・
                  `import type`/`export type ... from` の無視・
                  ダイヤモンド依存の 1 回評価・循環 import の簡易挙動・
                  エントリモジュールのトップレベル await が
                  `run_event_loop` で駆動できること 等。インメモリの
                  `ModuleHost` のみを使い、Noiria リポジトリは不要）
  kag_full.rs     T5a 統合テスト: Noiria の実ファイル
                  （`../noiria/runtime/api.ts`・`runtime/kag/*.ts`）を
                  `tsint::module` 経由でそのままロードし、
                  `(globalThis).Deno.core.ops` をモック ops テーブルとして
                  注入して KAG プレイヤーを実際に走らせる（2 ケース。
                  仮想クロック・入力イベント注入・`op_text_rects` の
                  矩形に合わせたクリック座標でリンク選択・ファイル間
                  `[jump]`・`[if]`/`[else]` の分岐・`op_quit` 到達までを
                  検証。`../noiria` が無い環境では skip）
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
- 正規表現リテラル（パースおよび T4 の実行エンジンに対応）
- オブジェクトリテラル（ショートハンド・メソッド省略記法・
  getter/setter・computed key・spread）
- TS の `as`/`satisfies` キャスト・非 null アサーション `!`
  （パース後は型情報を捨て、`TsAs`/`TsNonNull` として痕跡だけ残す）

### 非対応（コーパスに現れないため見送り）

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
  オペランドスタック・ローカル変数配列（`Vec<LocalSlot>`）・アップバリュー
  配列を持つ。`Vm::run_until` は `loop { self.step() }` で回るため、
  `await` 等でループを抜けて `Vm` 構造体や `Frame` 列を保持し後で再開する
  実装が自然に乗る。ネイティブ関数がコールバックを呼ぶ場合のみ Rust の
  再帰（`call_value` の入れ子）を使うが、ユーザー JS の呼び出し深さとは
  無関係。
- **例外処理**: `try/catch/finally` は例外テーブル方式。`finally` は
  各脱出経路（正常フォールスルー・catch 完了後・break/continue/return・
  例外での巻き戻し）ごとにバイトコードをインライン再コンパイルする
  （コンパイラが `Ctrl::TryFinally` スコープを一時的に取り除いてから
  再帰コンパイルすることで無限展開を防ぐ）。

## パフォーマンス最適化とメモリ管理（VM / 値表現 / GC）

### `PropMap` の小オブジェクト高速パス + 遅延 O(1) 索引化（`src/value.rs`）

JS オブジェクトの大半はプロパティ数が少ない小オブジェクトである一方、
状態テーブルや辞書用途では多数のプロパティを持つオブジェクトも現れる。
`PropMap` は両方を効率よく扱うハイブリッド設計を採用している。

- **小オブジェクト高速パス（`<= 16` エントリ）**: プロパティ数が
  `PROP_MAP_LINEAR_THRESHOLD`（16）以下の間は `HashMap` を一切確保せず
  （`index: Option<Box<HashMap<Rc<str>, usize>>>` が `None` のため追加
  サイズも 1 ポインタ幅のみ）、`entries: Vec<Option<(Rc<str>, PropSlot)>>`
  の線形探索だけでキャッシュフレンドリーかつ追加ヒープ確保ゼロで動作する。
- **遅延 O(1) ハッシュ索引**: 17 エントリを超えたタイミングで遅延的に
  `HashMap<Rc<str>, usize>` を構築し、以降の検索・更新・削除を O(1) 化する。
- **トゥームストーンと償却 O(1) 圧縮**: 索引ありの状態での `delete` は
  対象スロットを `None`（トゥームストーン）に置き換えることで O(1) とし、
  トゥームストーンが半分以上に達したタイミング（または生存数が閾値の半分以下まで
  減って線形モードへ戻すタイミング）で圧縮・再索引する。これにより、
  JS のプロパティ列挙順（挿入順）と削除→再挿入（末尾へ移動）セマンティクスを
  厳密に保ちつつ償却 O(1) を達成している。

### 非キャプチャローカル変数の非ボックス化（`src/bytecode.rs`, `src/compiler.rs`, `src/vm.rs`）

- コンパイラは各 `Chunk` ごとに、内側クロージャの `resolve_upvalue` によって
  実際に自由変数として捕捉されたローカルスロットだけを `Chunk::captured_locals`
  に記録する。
- VM の `Frame` は各ローカル変数を `LocalSlot::Direct(Value)` または
  `LocalSlot::Boxed(Rc<RefCell<Value>>)` として保持し、捕捉されるスロットのみ
  `Boxed` セルを確保する。
- 捕捉されない通常のローカル変数および非キャプチャの `for (let ...)` ループ
  変数は `Direct` のまま値操作されるため、`Rc<RefCell<Value>>` のヒープ確保・
  参照カウント増減・`RefCell` 借用チェックのオーバーヘッドがゼロになる。

### 循環参照コレクタ / GC（`Vm::collect_garbage` / `Interpreter::collect_garbage`）

非循環オブジェクトは Rust の `Rc` 参照カウントで即座に解放されるが、
JS のオブジェクト相互参照・クロージャの自己捕捉・Promise チェーン等で生じる
到達不能な循環参照を解放するため、**弱参照レジストリ + 厳密外部参照カウント検査
（trial-deletion）方式の循環参照 GC** を備えている。

- **回収対象**: プレーンオブジェクト、配列、クロージャ（関数・アップバリュー・
  `raw_args`）、クラス、`Map`/`Set`、`Promise`（pending 反応および中断中
  タスクの `Frame` 列を含む）のすべてを対象とする。
- **5 フェーズアルゴリズム**:
  1. **ルートマーク**: VM ルート（`frames`, `globals`, `pending_module_exports`,
     `microtasks`, `maybe_unhandled`, `current_owner_promise`）から到達可能な
     オブジェクトをすべてマークする。
  2. **候補収集**: 弱参照レジストリ（`gc_objects`）から解放済みエントリを除去し、
     未マークかつ生存中の候補を収集する。
  3. **内部エッジの trial-deletion**: 各候補の外部参照数 `Rc::strong_count - 1`
     から、候補集合内部のエッジ（共有される `FunctionObj` / アップバリューセル /
     `raw_args` は重複排除）を厳密に差し引く。
  4. **外部参照オブジェクトの保護**: 差し引き後に外部参照数が 1 以上残る候補
     （ホスト Rust コードが直接保持している `Value` や `HostPromise`、不透明な
     ネイティブクロージャが保持する参照など）およびそこから到達可能な候補をすべて
     生存保護する。
  5. **循環の切断と即時解放**: 外部参照 0 かつ外部生存集合からも非到達な
     100% 到達不能の循環ゴミのみ発信参照（`props`/`proto`/`kind`）を切断し、
     `Rc` により即時解放する。
- **自動起動と安全性**: VM の実行ループおよびイベントループのセーフポイント
  （`native_call_depth == 0`）で、前回回収以降の新規確保オブジェクト数が閾値
  （最小 `2048`、回収後は `max(生存オブジェクト数 * 2, 2048)` に動的調整）へ
  達すると自動実行される。ホスト側が保持する Rust の `Value` / `HostPromise`
  ハンドルを壊すことはない。明示的に `Interpreter::collect_garbage()` /
  `Vm::collect_garbage()` を呼ぶこともできる。

## T2 で妥協した JS セマンティクス

- `==` の抽象等価演算は主要規則（`null == undefined`・数値⇄文字列⇄真偽値の
  相互変換・オブジェクトの ToPrimitive）のみ実装。Symbol は非対応。
- `class` の `extends`/`super` は未実装（コーパスに無いため明確なエラーに
  する方針。必要になれば追加する）。
- ~~正規表現は `.test()`/`String.split()` に必要な最小サブセットのみ~~
  → **T4 で本実装**（詳細は後述の「T4: 正規表現エンジンの設計要点」を参照）。
- 配列の穴（elision, `[1, , 3]`）は `undefined` として扱う（本来の
  「存在しない要素」との区別はしない）。
- `console.log` の整形は簡易版（ネストしたオブジェクト/配列の
  `JSON.stringify` 相当の詳細な表示はしない）。
- `Date.prototype.toLocaleString` は UTC 前提の簡易フォーマット
  （`YYYY-MM-DD HH:MM:SS`）。現在時刻は `Interpreter::set_now_fn` で
  ホストが注入しない限り常に 0（VM 自体は決定的）。
- ~~`for (let i ...)` のループ変数がイテレーションごとに新しい束縛を作らない~~
  → **T3 で解消**（`OpCode::RebindLocal` 参照。非キャプチャ変数はさらにボックス化も回避）。
- ~~`computed member` への複合代入・論理代入が未対応~~ → **T3 で解消**。

## T3: Promise / async・await / イベントループの設計要点

### 実行単位（タスク）の表現

`Vm.frames: Vec<Frame>` はヒープ上の明示スタックであり、T3 はこれをそのまま
**1 回の async 関数呼び出し（またはモジュールのトップレベル）＝
1 つの独立したフレーム列（タスク）** という単位に拡張した:

- async 関数を呼び出すと（`dispatch_call` が `FunctionObj::is_async` を見て
  分岐する）、呼び出し元の `Vm.frames` を一旦退避し、その呼び出し専用の
  **新しい** `Vec<Frame>` を `Vm.frames` に差し替えて、そのタスクを
  「最初の `await` または完了」まで同期的に駆動する
  （`spawn_async_task_from_function` → `drive_current_task`）。
  ここまでが JS の意味論どおり「async 関数は呼び出し時点で本体の先頭から
  最初の await まで同期実行し、Promise を返した時点で呼び出し元に
  制御を戻す」を実現する部分。駆動が終わったら呼び出し元の `Vm.frames`
  を復元し、（完了していようが中断していようが）その場で作った
  Promise を呼び出し元へ返す。
- モジュールのトップレベルも「タスク」の一種として同じ経路
  （`drive_current_task`）で駆動する。`bytecode::Chunk::is_module`
  フラグで「このタスクの一番外側のフレームはモジュール本体である」を
  識別し（深さだけでは async 関数呼び出しと区別できないため）、`Return`
  実行時に export 値を確定させる。

### 中断・再開の仕組み

`OpCode::Await` の実行は:

1. オペランドを pop し、`Vm::coerce_to_promise` で Promise 化する
   （非 Promise 値は「即座に fulfill する新しい Promise」を作る —
   これにより「1 マイクロタスク後に再開」が自然に実現される。詳細後述）。
2. **`self.frames` を丸ごと `std::mem::take` で抜き取り**、
   `AwaitContinuation { frames, owner_promise }` として、対象 Promise の
   pending 反応（`PromiseReaction::Await`）に登録する
   （`owner_promise` は現在駆動中のタスクが完了した時に確定させる
   Promise — `Vm.current_owner_promise` に積んである）。
3. `step()` は `StepSignal::Suspended` を返し、`run_until` はこれを
   `RunOutcome::Suspended` としてそのまま呼び出し元（`drive_current_task`）
   へ伝える。この時点で Rust のコールスタックには何も残っていない
   （`run_until` のループを抜けただけ）。
4. 対象 Promise が解決すると、`settle_promise` が pending 反応を
   マイクロタスクキューへ `MicrotaskJob::ResumeAwait` として積む。
   `Vm::run_event_loop` がこれを処理する際、保存しておいたフレーム列を
   `self.frames` へ差し戻し、解決値を push（または例外テーブル経由で
   投げ直し）してから、また `drive_current_task` で「次の await または
   完了」まで駆動する。

**既知の制約（Rust の呼び出しスタックに起因）**: async 関数の呼び出し
自体は上記の通りいつでも独立したタスクとして中断できるが、
「ネイティブ関数が Rust の再帰で同期的に呼び出したコールバックの内側」
（`Array.prototype.forEach` のコールバック、getter/setter、`new` の
フィールド初期化子 等）で **直接** `await` すると中断できない
（Rust の呼び出しスタックを巻き戻す手段が無いため）。`Vm.native_call_depth`
というカウンタでこの状況を検出し、`await` 実行時にこれが 0 より大きければ
明確な `TypeError` にする（`call_value`/`call_function_obj` の中で
インクリメントし、async 関数のタスク駆動は毎回 0 にリセットする —
「async 関数呼び出しはどんな文脈から呼ばれても独立して中断できる」を
保証するため）。この境界を跨がない限り（＝素直に `await` を使う分には）
問題にならない。

### マイクロタスクキューと Promise の解決手続き

- `Vm.microtasks: VecDeque<MicrotaskJob>` が唯一のキュー。
  `.then`/`.catch`/`.finally` の反応・`PromiseResolveThenableJob` 相当
  （thenable 吸収）・`await` の再開、すべてこの 1 つのキューに積まれ、
  **登録順（FIFO）** で処理される。これにより「2 本の Promise チェーンが
  横並びに交互実行される」ような順序が自然に再現できる。
- `Vm::resolve_promise`（Resolve 手続き）: 値が本物の Promise なら
  `PromiseReaction::Then { on_fulfilled: None, on_rejected: None, .. }`
  という「ハンドラ無しの `.then()`」として連結する（＝そのまま
  transparent forwarding）。汎用 thenable（`then` が呼び出し可能な
  プレーンオブジェクト）は `MicrotaskJob::ResolveThenable` を 1 回
  キューに積んで `then(resolve, reject)` を叩く。それ以外はプレーンな
  値として即 fulfill する。
- `await <非 Promise値>` は `coerce_to_promise` の中で
  `resolve_promise` が同期的に即 fulfill するが、`Await` 自体は
  常に `add_reaction` 経由でマイクロタスクとして登録するため、
  **必ず 1 tick 遅延する**（0 tick で再開することは無い。歴史的には
  3 tick だったが、現行仕様に合わせて 1 tick を採用）。
- 未捕捉 reject の検出: `settle_promise` が reject した瞬間に反応が
  0 件なら `Vm.maybe_unhandled` に控えておき、`run_event_loop` が
  キューを完全に汲み尽くした時点でまだ `handled` になっていなければ
  `Interpreter::set_unhandled_rejection_hook` で登録したフックへ通知する
  （Node.js の `unhandledRejection` に近いタイミング。1 マイクロタスク
  "tick" の間に `.catch` が付けば報告されない）。

### ホスト非同期 API とイベントループ

- `Interpreter::register_native_async(name, arity, f)`: 呼び出す度に
  `Vm::new_host_promise` で pending Promise を作り、`f` に
  `HostPromise` ハンドルを渡して即座にその Promise を返す。
  `HostPromise::resolve`/`reject` はホスト側が好きなタイミングで
  `&mut Vm` を渡して呼べる（`vm.settle_host_promise` 経由。
  「解決前だったら `pending_host_count` を減らす」処理を挟むだけで、
  中身は `resolve_promise`/`settle_promise` と同じ）。
- `Vm::run_event_loop`/`Interpreter::run_event_loop`: マイクロタスク
  キューを空になるまで処理し、`PumpState::Idle`（もう進む処理が無い）か
  `PumpState::Waiting`（`HostPromise` の解決待ちが残っている）を返す。
  Noiria 等のゲームループでは、フレームごとに「入力/タイマー等のイベントで
  該当する `HostPromise` を resolve → `run_event_loop()` を呼ぶ」を
  繰り返して駆動する。
- テストでは `tests/vm_async.rs` に「仮想タイマー」（`wait(ms)` を
  `register_native_async` で登録し、テストコードが仮想時刻を進めて
  期限が来た `HostPromise` を resolve する）を実装して検証している
  （`VirtualClock`/`advance_time`）。実時間に一切依存しないため決定的。

## T3 で妥協した JS セマンティクス（ES 仕様との既知の差異）

- 上述の通り、ネイティブ関数の Rust 再帰呼び出しの内側（コールバック）で
  直接 `await` すると中断できず `TypeError` になる（async 関数を
  ラップして呼べば問題ない）。
- `await` の tick 数は 1 で固定（値が既に解決済みの Promise でも thenable
  でもプレーン値でも同じ）。ES2022 以降の V8 の実装と概ね一致するが、
  仕様書の逐語的な PromiseResolveThenableJob の段数までは追っていない。
- `Promise.prototype.finally` のコールバックが Promise/thenable を
  返した場合、それを待ってから確定させる、という仕様上の挙動は省略した
  （コールバックが同期的に完了する前提。投げた場合のみ結果を上書きする
  対応はしている）。
- `Promise.any`/`Promise.allSettled`/`AggregateError` は未実装
  （コーパスで使う想定が無いため。`race`/`all`/`resolve`/`reject` のみ）。
- ジェネレータ・非同期イテレータ（`for await...of` の本格対応、
  `Symbol.asyncIterator`）は非対応。`for await...of` は構文としては
  T1 でパースできるが、コンパイラは通常の `for...of` と同様「値を配列に
  実体化してから回す」簡易実装のまま（非同期イテレータのプロトコルには
  対応しない）。
- `queueMicrotask`/`setTimeout` のようなグローバル関数は用意していない
  （ホストが `register_native_async` で必要な名前を注入する方針。
  Noiria では `op_wait`/`op_input_next_event` 等をこの形で配線している。
  モック実装例は `tests/kag_full.rs` を参照）。
- Promise オブジェクトの `Symbol.toStringTag` 等、内部スロットの
  細部は省略。`console.log`/文字列化は `Promise { <pending> }` 等の
  簡易表示のみ。

## T4: 正規表現エンジンの設計要点

`src/regex.rs` に、JS の RegExp の実用サブセット（Noiria コーパスで
実際に使われる構文 + ゲーム開発者が普通に書く範囲）を対象とした
バックトラッキング方式の本実装がある。

### 対応構文表

| 構文 | 対応 | 備考 |
| --- | --- | --- |
| リテラル文字・`.` | ○ | `.` は `s` フラグで改行にも一致 |
| 文字クラス `[...]` `[^...]` `a-z` | ○ | クラス内エスケープ・ショートハンド込み |
| `\d \D \w \W \s \S` | ○ | `\w` は ASCII のみ（JS 仕様通り）。`\s` は Unicode 空白 + BOM |
| `\n \r \t \v \f \0 \xHH \uHHHH \u{H...}` | ○ | 制御・16進・Unicode エスケープ |
| 量指定子 `* + ? {n} {n,} {n,m}` | ○ | 末尾 `?` で非貪欲化 |
| グループ `(...)` / `(?:...)` | ○ | キャプチャ / 非キャプチャ |
| 選択 `\|` | ○ | 左の枝から順に試す（優先順位あり、最長一致ではない） |
| アンカー `^ $` | ○ | `m` フラグで行頭・行末にも一致 |
| 単語境界 `\b \B` | ○ | |
| 後方参照 `\1`〜 | ○ | 未マッチのグループへの参照は空文字列扱い（JS 仕様） |
| 先読み `(?=...)` `(?!...)` | ○ | |
| フラグ `g i m s` | ○ | ステートフルな `lastIndex`（`g`）込み |
| フラグ `u` | △ | 受理はするが元々コードポイント単位動作が既定なので実質 no-op |
| 後読み `(?<=...)` `(?<!...)` | × | 明確なコンパイルエラー |
| 名前付きキャプチャ `(?<name>...)` / `\k<name>` | × | 明確なコンパイルエラー |
| フラグ `y`（sticky）`d`（indices） | × | 明確なコンパイルエラー |
| Unicode プロパティエスケープ `\p{...}` | × | 明確なコンパイルエラー |

### 実行方式

パーサが構築した AST を、`Save`/`Jmp`/`Split`/`Match` 等からなる
フラットな命令列（`Inst`）へコンパイルする（ジャンプ先はコンパイル時に
確定するアドレス方式。バックパッチはモジュール内で完結する）。実行は
**ヒープ上の明示的なバックトラックスタック（`Vec<BacktrackFrame>`）と
アンドゥログ（`trail`）を用いる反復ループ `Exec::run_from`** で行う。

- **インライン展開による完全なバックトラック**: 選択 `|` と量指定子
  `* + ? {n,m}` は、いずれも `Split`/`Jmp` を使って**地続きの命令列として
  インライン展開**する。量指定子の外側にある残りのパターンとの継続関係を
  通常の `pc` の流れで表現することで、量指定子の中にネストした別の量指定子や
  選択があっても、外側が後から失敗してバックトラックした時に本体側の
  「何文字消費するか」の選択を正しく再試行できる（回帰テストは
  `tests/vm_regex.rs` の `nested_quantifier_backtracking_splits_correctly`/
  `classic_backtracking_alternation_and_repeat_combo` を参照）。
  `{n,m}` の上限分は有限個の `Split` を反復展開し、`*`/`+`/`{n,}` の
  無制限部分は `Split` + 進捗チェック付きの `LoopBack` 命令によるループとして
  展開する（空文字列にしかマッチしない本体を持つ量指定子 `(a?)*` 等でも
  `LoopBack` が進捗なしを検知して打ち切るため無限ループしない）。
- **先読み `(?=...)` `(?!...)`**: 常にゼロ幅のため、`Inst::Look` は対応する
  部分命令列を独立したサブルーチンとして命令列内に埋め込み、成否だけを判定
  してから後続アドレスへ進む。
- **アンドゥログ（`trail`）によるゼロクローン巻き戻し**: バックトラッキングの
  選択点（`Split`・`Look`）でキャプチャ配列 `saved` 全体を毎回クローンする
  代わりに、`Inst::Save` の書き換え前に旧値を `trail: Vec<(usize, Option<usize>)>`
  へ記録し、分岐フレームには `trail_mark` だけを積む。枝が失敗した際は
  `trail_mark` まで逆順に巻き戻すだけで済むため、バックトラックごとの
  ヒープ再確保が発生しない。
- **スタックオーバーフロー・ReDoS 防止上限**: 後方参照・先読みは正規言語では
  ないため Thompson 構成の NFA ではなくバックトラッキング方式を採っている。
  そのトレードオフである壊滅的バックトラック（ReDoS）や過大なメモリ消費への
  対策として、以下の安全上限を設け、超過時は明確な `RegexError`（VM 上では
  `RangeError`）を返す:
  - 実行ステップ数上限: 既定 200 万（`DEFAULT_MAX_STEPS`）
  - ヒープ上のバックトラックスタック深さ上限: 既定 5 万（`DEFAULT_MAX_DEPTH`）
  - 先読みネスト深さ上限: 既定 256（`MAX_LOOK_DEPTH`、OS コールスタック保護）

### VM/builtins への接続

- `value.rs`: `ObjKind::Regex(Rc<regex::Regex>)`。`vm.rs::new_regex_value`
  が正規表現リテラル（`OpCode::LoadRegex`）・`new RegExp(...)` の両方から
  使われる共通のオブジェクト生成ヘルパー（`lastIndex` を own プロパティ
  として初期化する。`source`/`flags`/`global`/`ignoreCase`/`multiline`/
  `dotAll`/`unicode` は読み取り専用のプロパティフォールバックとして
  `Vm::get_prop_object` 側で供給する）。
- `builtins.rs`: `RegExp` コンストラクタ（文字列 + フラグ、または既存の
  `RegExp` オブジェクトのコピー）、`RegExp.prototype.{test,exec,toString}`
  （`g` フラグ時は `lastIndex` を読み書きしてステートフルに動作する）、
  `String.prototype.{match,search,replace,replaceAll,split}`
  （`replace`/`replaceAll` は `$&` `` $` `` `$'` `$1`〜`$9`（2桁対応）`$$`
  の展開とコールバック関数の両方に対応。`split` はキャプチャグループを
  結果配列に含める）。

### JS との既知の差異

- **Unicode/UTF-16**: 本エンジンは Rust の `char`（Unicode スカラ値）単位で
  動作する。JS の文字列は内部的に UTF-16 コード単位の列であり、サロゲート
  ペア（絵文字等の追加面文字）は `u` フラグ無しでは 2 つの別々の「文字」
  として扱われる。本エンジンは `u` フラグの有無に関わらず常にコードポイント
  単位で動作するため、サロゲートペアの挙動は JS の非 `u` モードとは異なる
  （`u` モードの挙動に近い）。
- `\s` は Rust の `char::is_whitespace()`（Unicode `White_Space` プロパティ）
  + BOM(U+FEFF) による近似。JS 仕様の厳密な `WhiteSpace`/`LineTerminator`
  文字集合と完全一致するとは限らない。
- `i` フラグの大文字小文字比較は `to_lowercase()` の先頭 1 文字同士を
  比較する簡易実装。独 `ß` のように 1 文字が複数文字へ展開される
  ケースなど、厳密な Unicode ケースフォールディングとは異なりうる。
- 壊滅的バックトラックしうるパターンや極端に大きい入力は、無限に固まったり
  スタックオーバーフローする代わりに、ステップ数（200 万）・バックトラック
  スタック深さ（5 万）・先読みネスト深さ（256）の上限に達した時点で
  明確なエラー（`RangeError`）になる（安全側に倒した設計）。通常の量指定子
  繰り返し自体はヒープ上の明示スタックで実行されるため、OS コールスタックを
  圧迫しない。
- 後読み `(?<=...)` `(?<!...)`、名前付きキャプチャ `(?<name>...)` /
  `\k<name>`、`y`（sticky）・`d`（indices）フラグ、Unicode プロパティ
  エスケープ `\p{...}` は未対応（明確なコンパイルエラーになる）。
- `String.prototype.replace`/`replaceAll`/`split`/`search`、および
  `g` フラグ無しの `match` は、渡された `RegExp` オブジェクトの
  `lastIndex` を読み書きしない（内部で独立した検索を行う）。実際の V8 は
  `replace`/`g` 付き `match` の実行中は対象の `lastIndex` を触るが、
  スキャン完了後はいずれ `0` に戻るため通常は違いが観測されない
  （`exec`/`test`/`g` フラグ付きループ等、ユーザーが明示的に `lastIndex`
  を使う典型パターンはステートフルに正しく動作する）。

## Noiria 本体への組み込み（T5b・完了済み）

[Noiria](https://github.com/ChanyaVRC/noiria)（[Wiki & Website](https://chanyavrc.github.io/noiria-site/)）
では、すでに `tsint` がデフォルトの `dist` スクリプトエンジンとして組み込まれており、
crates.io 上の `tsint = "0.1.0"` に依存している。

- **ホスト API 配線**: `Interpreter::register_native` / `register_native_async`
  + `HostPromise` を通じて、Noiria の同期・非同期 ops（`op_wait`,
  `op_input_next_event`, `op_trans_wait`, `op_audio_wait_end` 等）を
  `globalThis.Deno.core.ops` 互換の形で提供し、ゲームループからフレームごとに
  `run_event_loop()` を駆動する構成となっている（本リポジトリの
  `tests/kag_full.rs` にも同構造のモック ops 完走テストがある）。
- **モジュール解決**: `tsint::module::ModuleHost` の実装により、`noiria:api` /
  `noiria:kag` 等の仮想 specifier およびゲームスクリプトをロードする。
- **V8 との互換検証**: `tests/vm_corpus.rs` や `tests/kag_full.rs` に加え、
  Noiria 本体側でも V8 バックエンドと同一シナリオでの挙動検証が行われている。

## CI/CD・リリース

`.github/workflows/` に以下の GitHub Actions ワークフローを備えている。

- **`ci.yml`**: `main` への push および PR ごとに実行。
  - `rustfmt --all --check` および `clippy --all-targets -- -D warnings`
  - `ubuntu-latest` / `windows-latest` での `cargo build --all-targets`、
    `cargo test`（debug）、`cargo test --release`
  - `ubuntu-24.04-arm`（ネイティブ `aarch64`）でのビルド＆テスト
- **`docs.yml`**: `main` への push ごとに `cargo doc --no-deps` をビルドし、
  GitHub Pages（https://chanyavrc.github.io/tsint/）へ自動公開。
- **`release.yml`**: `vX.Y.Z` タグ push 時に `Cargo.toml` のバージョン一致検証、
  `cargo test --locked`、`cargo package --locked` 展開後のテストを実行し、
  crates.io への公開および GitHub Release 作成を冪等に実行。

## テスト・検証

```sh
cargo test                                   # 全 385 テスト
                                             # - 内蔵ユニットテスト: 66 (regex: 52, value: 5, vm: 9)
                                             # - T1: unit_syntax 71 + corpus 2 + fuzz 1 (500 破壊入力)
                                             # - T2: vm_basics 90 + vm_corpus 4 + vm_programs 5
                                             # - T3: vm_async 77
                                             # - T4: vm_regex 54
                                             # - T5a: module_basics 13 + kag_full 2
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo doc --no-deps
```

コーパステスト（`corpus.rs`, `vm_corpus.rs`）・`fuzz.rs`・`kag_full.rs` は
隣接ディレクトリに `../noiria`（Noiria リポジトリ）が存在すると実コーパスで
実行される。`../noiria` が無い環境（CI や単体チェックアウト時）では該当ケースを
自動で skip する（`fuzz.rs` は内蔵の合成ソースへフォールバック）。
`tests/module_basics.rs` や全ユニットテストはインメモリで完結するため
外部リポジトリ無しで常に実行される。

## ライセンス

[MIT License](LICENSE)
