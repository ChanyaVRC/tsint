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
| T2 | 明示スタックのバイトコード VM（AST → バイトコードへコンパイルし、Rust の呼び出しスタックに頼らずに実行する） | 実装済み |
| T3 | Promise / async・イベントループ（Noiria の `await sys.wait()` 等のゲームループ協調を実現） | 実装済み（本リポジトリの現状） |
| T4 | 正規表現エンジン本実装（T2/T3 は `.test()`/`split()` に必要な最小サブセットのみ） | 未着手（次段階） |
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
限界。ホスト駆動の非同期実行には `run_event_loop` を直接使うこと）。

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
  value.rs        T2/T3: ランタイム値（Value/ObjData/PropMap/ClassData/MapData/SetData/
                   PromiseData・PromiseReaction・AwaitContinuation）
  vm.rs           T2/T3: 明示スタック VM 本体（Frame/Vm、実行ループ、例外巻き戻し、
                   プロパティアクセス、JS 演算子セマンティクス、Promise の解決手続き、
                   マイクロタスクキュー、async 関数のタスク駆動、HostPromise）
  builtins.rs     T2/T3: 組み込みオブジェクト・グローバル関数
                   （console/Object/Array/String/Number/Math/JSON/Map/Set/Date/Error/
                   Promise 系）
  regex_mini.rs   T2: 正規表現の最小サブセット（本実装は T4）
  lib.rs          公開 API: `parse_module` に加え `Interpreter`
                   （`new`/`register_native`/`register_native_async`/`run_module`/
                   `run_event_loop` 等）
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
  vm_async.rs     T3: Promise・マイクロタスクキュー・async/await・イベントループの
                   テスト（77 ケース。順序テスト・Noiria パターンの再現・
                   並走タスクのフレーム分離・T2 の妥協解消の回帰テスト 等）
  vm_common/mod.rs  T2/T3 テスト共通ヘルパー
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
- `class` の `extends`/`super` は未実装（コーパスに無いため明確なエラーに
  する方針。T5 までに必要になれば追加）。
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
- ~~`for (let i ...)` のループ変数がイテレーションごとに新しい束縛を
  作らない~~ → **T3 で解消**（`OpCode::RebindLocal` 参照）。
- ~~`computed member` への複合代入・論理代入が未対応~~ → **T3 で解消**。

## T3: Promise / async・await / イベントループの設計要点

### 実行単位（タスク）の表現

T2 の設計メモにあった通り、`Vm.frames: Vec<Frame>` は既に
「ヒープ上の明示スタック」だった。T3 はこれをそのまま
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
  T5 で Noiria のゲームループへ統合する際は、ホストのフレームループから
  「入力/タイマー等のイベントで該当する `HostPromise` を resolve →
  `run_event_loop` を呼ぶ」を繰り返す形になる想定。
- テストでは `tests/vm_async.rs` に「仮想タイマー」（`wait(ms)` を
  `register_native_async` で登録し、テストコードが仮想時刻を進めて
  期限が来た `HostPromise` を resolve する）を実装してデモしている
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
  T5 で Noiria の `sys.wait` 等をこの形で載せる）。
- Promise オブジェクトの `Symbol.toStringTag` 等、内部スロットの
  細部は省略。`console.log`/文字列化は `Promise { <pending> }` 等の
  簡易表示のみ。

## T4（正規表現エンジン本実装）への引き継ぎメモ

- `String.prototype.replace`/`split`/`test`/`match` 等で実用に足る
  正規表現（量指定子・グループ・キャプチャ・アンカー・後方参照の要否は
  コーパスを見て判断）を実装すること。現状の `regex_mini.rs` は
  文字クラス・代替・エスケープショートハンドのみの最小サブセット。
- Promise とは直接の依存関係は無いが、`String.prototype.replace` に
  関数コールバックを渡す形（`str.replace(re, (match) => ...)`）は
  `Vm::call_value` を使うだけで T3 の変更の影響を受けずに実装できるはず。

## T5（Noiria 統合）への引き継ぎメモ

- `run_module` は「トップレベル await がホスト Promise 待ちのまま残ると
  エラーになる」同期 API のままなので、Noiria 本体の統合では使わず、
  `Interpreter::run_event_loop` を直接使うか、`run_module` 相当の処理を
  ホスト側のゲームループに合わせて再実装すること（モジュールの
  トップレベルタスクを「完了するまで」ではなく「1 フレーム分だけ進める」
  形に分解する必要がある）。
- `Interpreter::register_native_async` + `HostPromise` がそのまま
  `op_wait`/`op_input_next_event` 等の実装の土台になる想定
  （`tests/vm_async.rs` の仮想タイマー・`waitClick`/`waitForFlag`
  パターンを参照）。
- `import` 文の実装（モジュール間の値の受け渡し）と、async 関数の
  タスク分離の仕組み（`Vm.frames` の退避・復元）は独立した関心事なので、
  モジュールシステムの導入で `run_module`/`drive_current_task` 周りを
  大きく変える必要は無いはず（モジュールごとに `Interpreter`/`Vm` を
  共有しつつ複数の `run_module` 相当の呼び出しを行う形になる見込み）。
- `class` の `extends`/`super` が必要になった場合、コンストラクタ内で
  `super(...)` 呼び出し前は `this` にアクセスできない、という TDZ 的な
  制約は今のところ考慮していない（extends 自体が T2 時点で明確な
  コンパイルエラーのため）。

## テスト

```sh
cargo test                              # 全テスト（T1: 71 + T2 単体: 89 + T3: 77 +
                                         # コーパス: 4 + プログラム: 5 + 内蔵: 数件）
cargo clippy --all-targets -- -D warnings
cargo fmt --all
```

コーパステスト・fuzz テストは `../noiria`（Noiria リポジトリ）が
存在する前提。無い環境では該当ケースを skip するか、fuzz テストは
内蔵の合成ソースにフォールバックする。
