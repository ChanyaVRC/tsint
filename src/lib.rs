//! tsint — 完全自作 TypeScript インタプリタ（Noiria エンジン用）。
//!
//! T1: レキサ + パーサ。T2: バイトコードコンパイラ + 明示スタック VM。
//! 依存クレートはゼロ（std のみ）。
//!
//! 対応範囲や設計方針は README を参照。

pub mod ast;
pub mod builtins;
pub mod bytecode;
pub mod compiler;
pub mod lexer;
pub mod parser;
pub mod regex_mini;
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

/// T2 の公開エントリポイント。`Interpreter::new()` → `register_native` →
/// `run_module` の順で使う。
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

    pub fn vm(&mut self) -> &mut vm::Vm {
        &mut self.vm
    }

    /// ソースをパース・コンパイル・実行し、export された値の一覧を返す。
    /// import 文を含むモジュールは明確なエラーになる（T5 で対応予定）。
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
