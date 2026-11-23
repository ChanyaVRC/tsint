//! tsint — 完全自作・ゼロ依存（`std` のみ）の TypeScript インタプリタ。
//!
//! ノベルゲームエンジン [Noiria](https://github.com/ChanyaVRC/noiria) の
//! デフォルト配布スクリプトエンジンとして設計・実装されている。
//!
//! - T1: レキサ + パーサ（[`parse_module`]）
//! - T2: バイトコードコンパイラ + 明示スタック VM（非キャプチャローカル変数の
//!   非ボックス化・O(1) `PropMap` 索引化・循環参照 GC）
//! - T3: Promise / async・await / イベントループ（[`Interpreter::register_native_async`]・
//!   [`Interpreter::run_event_loop`]）
//! - T4: 正規表現エンジン本実装（明示バックトラックスタック + アンドゥログ）
//! - T5: ES モジュールシステム（[`Interpreter::run_module_graph`]）および Noiria 本体統合
//!
//! 対応範囲や設計方針は README を参照。

pub mod ast;
pub mod builtins;
pub mod bytecode;
pub mod compiler;
pub mod lexer;
pub mod module;
pub mod parser;
pub mod regex;
pub mod value;
pub mod vm;

pub use ast::Module;
pub use parser::ParseError;

use std::rc::Rc;

/// ソース文字列を 1 モジュールとしてパースする。
///
/// `name` は診断メッセージや `Module::name` に使われる識別子（多くはファイルパス）。
pub fn parse_module(source: &str, name: &str) -> Result<Module, ParseError> {
    let parser = parser::Parser::new(source)?;
    parser.parse_module(name)
}

/// `run_module` の失敗要因。
#[derive(Debug)]
pub enum InterpError {
    Parse(ParseError),
    Compile(compiler::CompileError),
    /// 未捕捉の例外（表示用に文字列化済み）。
    Runtime(String),
}

impl std::fmt::Display for InterpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InterpError::Parse(e) => write!(f, "{e}"),
            InterpError::Compile(e) => write!(f, "{e}"),
            InterpError::Runtime(s) => write!(f, "未捕捉の例外: {s}"),
        }
    }
}

/// `tsint` の公開エントリポイント。`Interpreter::new()` → `register_native` /
/// `register_native_async` → `run_module` / `run_module_graph` の順で使う。
pub struct Interpreter {
    vm: vm::Vm,
}

impl Interpreter {
    pub fn new() -> Self {
        Interpreter { vm: vm::Vm::new() }
    }

    /// ホスト関数をグローバルとして登録する（`Noiria` の `sys.*` 等に相当）。
    pub fn register_native(&mut self, name: &str, arity: usize, f: value::NativeFn) {
        self.vm.register_native(name, arity, f);
    }

    /// 任意のグローバル値を直接登録する。
    pub fn define_global(&mut self, name: &str, value: value::Value) {
        self.vm.define_global(name, value);
    }

    /// `console.log` 相当の出力フックを差し替える（既定は標準出力）。
    pub fn set_console_hook(&mut self, f: impl FnMut(&str) + 'static) {
        self.vm.on_log = Box::new(f);
    }

    /// `Date` 等が使う「現在時刻」をホストから注入する。VM 自体は
    /// 決定的に保つ方針（未設定なら常に 0 を返す）。
    pub fn set_now_fn(&mut self, f: impl Fn() -> f64 + 'static) {
        self.vm.now_fn = Some(Rc::new(f));
    }

    /// 未捕捉の Promise reject をホストへ報告するフックを差し替える
    /// （既定は何もしない）。`Vm::run_event_loop` がマイクロタスクを
    /// 汲み尽くした時点でまだ誰も観測していない reject に対して呼ばれる。
    pub fn set_unhandled_rejection_hook(&mut self, f: impl FnMut(&value::Value) + 'static) {
        self.vm.on_unhandled_rejection = Box::new(f);
    }

    /// ホスト側が非同期に解決する `Promise` を返すネイティブ関数を登録する
    /// （Noiria の `op_wait` / `op_input_next_event` のようなホスト非同期 API 向け）。
    ///
    /// `f` は呼び出しの度に `HostPromise` ハンドルを受け取る。呼び出し自体は
    /// 即座にその Promise を返す（同期）。ハンドルはホスト側で保持しておき、
    /// 後で（タイマー発火・入力イベント等のタイミングで）
    /// `HostPromise::resolve`/`reject` を呼んで解決する。
    pub fn register_native_async(
        &mut self,
        name: &str,
        arity: usize,
        f: impl Fn(&mut vm::Vm, value::Value, &[value::Value], vm::HostPromise) + 'static,
    ) {
        let f = Rc::new(f);
        self.vm.register_native(
            name,
            arity,
            Rc::new(move |vm, this, args| {
                let (promise_val, handle) = vm.new_host_promise();
                f(vm, this, args, handle);
                Ok(promise_val)
            }),
        );
    }

    /// マイクロタスクキューを汲み尽くす（`.then` 反応・`await` の再開等）。
    /// 保留中のホスト Promise（`register_native_async` 由来）がまだ残って
    /// いれば `PumpState::Waiting`、無ければ `PumpState::Idle` を返す。
    /// ゲームループの 1 フレームごとにこれを呼び、ホスト Promise を解決して
    /// からまた呼ぶ形で駆動する。
    pub fn run_event_loop(&mut self) -> vm::PumpState {
        self.vm.run_event_loop()
    }

    pub fn vm(&mut self) -> &mut vm::Vm {
        &mut self.vm
    }

    /// 到達不能な循環参照を明示的に回収し、解放（参照切断）したオブジェクト数を返す。
    pub fn collect_garbage(&mut self) -> usize {
        self.vm.collect_garbage()
    }

    /// ソースをパース・コンパイル・実行し、export された値の一覧を返す。
    /// `import` 文を含むモジュールは明確なエラーになる（複数モジュールには
    /// [`Interpreter::run_module_graph`] を使用する）。
    pub fn run_module(
        &mut self,
        source: &str,
        name: &str,
    ) -> Result<Vec<(Rc<str>, value::Value)>, InterpError> {
        let module = parse_module(source, name).map_err(InterpError::Parse)?;
        let chunk = compiler::compile_module(&module).map_err(InterpError::Compile)?;
        self.vm
            .run_module(chunk)
            .map_err(|thrown| InterpError::Runtime(self.vm.to_display_string(&thrown)))
    }
}

impl Default for Interpreter {
    fn default() -> Self {
        Self::new()
    }
}
