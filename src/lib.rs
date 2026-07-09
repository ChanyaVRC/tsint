//! tsint — 完全自作 TypeScript インタプリタ（Noiria エンジン用）。
//!
//! T1: レキサ + パーサ。依存クレートはゼロ（std のみ）。
//!
//! 対応範囲や設計方針は README を参照。ここではモジュールの再エクスポートと
//! トップレベル API `parse_module` のみを提供する。

pub mod ast;
pub mod lexer;
pub mod parser;

pub use ast::Module;
pub use parser::ParseError;

/// ソース文字列を 1 モジュールとしてパースする。
///
/// `name` は診断メッセージや `Module::name` に使われる識別子（多くはファイルパス）。
pub fn parse_module(source: &str, name: &str) -> Result<Module, ParseError> {
    let parser = parser::Parser::new(source)?;
    parser.parse_module(name)
}
