//! T5a: ES モジュールシステム。
//!
//! 設計方針:
//! - VM/コンパイラ本体（`crate::vm`/`crate::compiler`/`crate::bytecode`）にはほぼ手を
//!   入れない。`import`/`export ... from` を含む AST を、依存モジュールの
//!   評価が終わった後に「名前空間オブジェクトを保持するグローバル変数から
//!   `const` で読み出す」形へ **AST レベルで書き換えてから**
//!   `compiler::compile_module` に渡す（コンパイラは書き換え後の AST しか
//!   見ないので、`Import`/`ExportNamed{source:Some}`/`ExportAll` を実際には
//!   一切コンパイルしない＝既存の「未対応」エラー経路にも触れない）。
//! - モジュールは resolved specifier ごとに 1 回だけ評価してキャッシュする。
//!   循環 import は「参照した時点でまだ埋まっていないプロパティは
//!   `undefined`」という簡易挙動にする: 依存モジュールの名前空間オブジェクト
//!   は評価前（キャッシュ登録と同時）に空オブジェクトとして確保し、
//!   評価完了後にプロパティを書き込む。循環の一方から見れば、評価中の
//!   相手側の名前空間オブジェクトはまだ空のまま参照することになる
//!   （このコーパスでは型のみの循環しか無いため実害はない）。
//! - エントリモジュールだけは「トップレベル await がホスト Promise 待ちの
//!   まま残ってもエラーにしない」経路（`Vm::start_module_task`）を使う。
//!   KAG プレイヤーの `run()` のように実質終わらないゲームループを
//!   トップレベル await するケースを扱うため。ホスト側は
//!   `Interpreter::run_event_loop` を繰り返し呼んで駆動を続ける。
//! - 依存モジュール（エントリ以外）はトップレベル await を使わないという
//!   Noiria のコーパスの前提の下、`Vm::run_module` と同じ「完全に解決する
//!   はず」経路で評価する。解決しなければ `ModuleError::Runtime` になる。

use std::rc::Rc;

use crate::ast::{
    self, Expr, ExprKind, ImportSpecifier, Module, Pos, PropertyKey, Span, Stmt, StmtKind,
    VarDeclarator, VarKind,
};
use crate::compiler::{self, CompileError};
use crate::hash::FastHashMap;
use crate::value::Value;
use crate::vm::ModuleTaskState;
use crate::{parse_module, Interpreter, ParseError};

/// ホストが提供する resolver/loader フック。
///
/// `resolve` は import 元（`referrer`）と specifier 文字列から、モジュール
/// キャッシュのキーとしても使う「解決済み識別子」を作る（多くはファイル
/// パスの正規化形）。`load` はその識別子からソース文字列を読む。
pub trait ModuleHost {
    fn resolve(&self, specifier: &str, referrer: &str) -> Result<String, String>;
    fn load(&self, resolved: &str) -> Result<String, String>;
}

/// `run_module_graph` の失敗要因。
#[derive(Debug)]
pub enum ModuleError {
    Resolve {
        specifier: String,
        referrer: String,
        message: String,
    },
    Load {
        resolved: String,
        message: String,
    },
    Parse {
        resolved: String,
        source: ParseError,
    },
    Compile {
        resolved: String,
        source: CompileError,
    },
    /// 未捕捉の例外（依存モジュールの評価が完全に解決しなかった場合も含む）。
    Runtime {
        resolved: String,
        message: String,
    },
}

impl std::fmt::Display for ModuleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModuleError::Resolve {
                specifier,
                referrer,
                message,
            } => write!(
                f,
                "モジュール解決エラー: '{specifier}' (from {referrer}): {message}"
            ),
            ModuleError::Load { resolved, message } => {
                write!(f, "モジュール読み込みエラー: {resolved}: {message}")
            }
            ModuleError::Parse { resolved, source } => {
                write!(f, "{resolved} の構文エラー: {source}")
            }
            ModuleError::Compile { resolved, source } => {
                write!(f, "{resolved} のコンパイルエラー: {source}")
            }
            ModuleError::Runtime { resolved, message } => {
                write!(f, "{resolved} の実行時エラー: {message}")
            }
        }
    }
}

impl std::error::Error for ModuleError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ModuleError::Parse { source, .. } => Some(source),
            ModuleError::Compile { source, .. } => Some(source),
            ModuleError::Resolve { .. }
            | ModuleError::Load { .. }
            | ModuleError::Runtime { .. } => None,
        }
    }
}

/// `Interpreter::run_module_graph` の戻り値。
#[derive(Debug)]
pub enum ModuleOutcome {
    /// エントリモジュールのトップレベル評価が（トップレベル await も含めて）
    /// 完全に解決した。
    Settled(Vec<(Rc<str>, Value)>),
    /// トップレベル評価がホスト Promise 待ちで中断された。
    /// `Interpreter::run_event_loop` で駆動を続けること。
    Pending,
}

fn line_span(line: u32) -> Span {
    let pos = Pos {
        line,
        col: 0,
        byte: 0,
    };
    Span::new(pos, pos)
}

fn ident_expr(name: &str, line: u32) -> Expr {
    Expr {
        id: 0,
        span: line_span(line),
        kind: ExprKind::Ident(name.to_string()),
    }
}

fn member_expr(obj: Expr, prop: &str, line: u32) -> Expr {
    Expr {
        id: 0,
        span: line_span(line),
        kind: ExprKind::Member {
            obj: Box::new(obj),
            prop: PropertyKey::Ident(prop.to_string()),
            optional: false,
        },
    }
}

/// `const <name> = <init>;` を作る。
fn const_decl(name: &str, init: Expr, line: u32) -> Stmt {
    Stmt {
        id: 0,
        span: line_span(line),
        kind: StmtKind::VarDecl {
            kind: VarKind::Const,
            decls: Box::new([VarDeclarator {
                name: ast::Pattern::Ident(name.to_string()),
                init: Some(Box::new(init)),
            }]),
        },
    }
}

/// `export { <name> };` を作る（同一モジュール内の既存ローカルを export する形。
/// `compiler::compile_module` が元から対応している経路）。
fn export_named(name: &str, line: u32) -> Stmt {
    Stmt {
        id: 0,
        span: line_span(line),
        kind: StmtKind::ExportNamed {
            specifiers: Box::new([ast::ExportSpecifier::Named {
                local: name.to_string(),
                exported: name.to_string(),
            }]),
            source: None,
            type_only: false,
        },
    }
}

struct Loader<'i, 'h> {
    interp: &'i mut Interpreter,
    host: &'h dyn ModuleHost,
    /// resolved specifier -> 名前空間オブジェクトを保持するグローバル変数名。
    cache: FastHashMap<String, Rc<str>>,
    /// resolved specifier -> export 名一覧（`export * from` 用）。
    export_names: FastHashMap<String, Vec<Rc<str>>>,
    next_id: u32,
}

impl<'i, 'h> Loader<'i, 'h> {
    fn fresh_name(&mut self, prefix: &str) -> String {
        self.next_id += 1;
        format!("__tsint_mod_{prefix}_{}", self.next_id)
    }

    fn resolve(&self, specifier: &str, referrer: &str) -> Result<String, ModuleError> {
        self.host
            .resolve(specifier, referrer)
            .map_err(|message| ModuleError::Resolve {
                specifier: specifier.to_string(),
                referrer: referrer.to_string(),
                message,
            })
    }

    /// 解決済みモジュールを読み込み・書き換え・コンパイルしてチャンクを返す。
    fn compile_resolved(
        &mut self,
        resolved: &str,
    ) -> Result<Rc<crate::bytecode::Chunk>, ModuleError> {
        let source = self
            .host
            .load(resolved)
            .map_err(|message| ModuleError::Load {
                resolved: resolved.to_string(),
                message,
            })?;
        let parsed = parse_module(&source, resolved).map_err(|source| ModuleError::Parse {
            resolved: resolved.to_string(),
            source,
        })?;
        let rewritten = self.rewrite_module(parsed, resolved)?;
        compiler::compile_module(&rewritten).map_err(|source| ModuleError::Compile {
            resolved: resolved.to_string(),
            source,
        })
    }

    fn push_reexport(
        &mut self,
        out: &mut Vec<Stmt>,
        ns_name: &str,
        source_name: &str,
        exported: String,
        line: u32,
    ) {
        let tmp = self.fresh_name("reexp");
        out.push(const_decl(
            &tmp,
            member_expr(ident_expr(ns_name, line), source_name, line),
            line,
        ));
        out.push(Stmt {
            id: 0,
            span: line_span(line),
            kind: StmtKind::ExportNamed {
                specifiers: Box::new([ast::ExportSpecifier::Named {
                    local: tmp,
                    exported,
                }]),
                source: None,
                type_only: false,
            },
        });
    }

    /// 依存モジュールを読み込み・評価し（キャッシュ済みなら再利用）、
    /// その名前空間オブジェクトを保持するグローバル変数名を返す。
    fn load_dependency(&mut self, resolved: &str) -> Result<Rc<str>, ModuleError> {
        if let Some(name) = self.cache.get(resolved) {
            return Ok(name.clone());
        }

        // 循環 import 対策: 先に空の名前空間オブジェクトをキャッシュ＆
        // グローバル登録してから本体を評価する。
        let ns_name: Rc<str> = Rc::from(self.fresh_name("ns"));
        let ns_obj = Value::new_plain_object();
        self.cache.insert(resolved.to_string(), ns_name.clone());
        self.interp.define_global(&ns_name, ns_obj.clone());

        let chunk = self.compile_resolved(resolved)?;
        let exports =
            self.interp
                .vm()
                .run_module(chunk)
                .map_err(|thrown| ModuleError::Runtime {
                    resolved: resolved.to_string(),
                    message: self.interp.vm().to_display_string(&thrown),
                })?;

        let mut names = Vec::with_capacity(exports.len());
        if let Value::Object(o) = &ns_obj {
            let mut b = o.borrow_mut();
            for (k, v) in exports {
                names.push(k.clone());
                b.props.set_value(k, v);
            }
        }
        self.export_names.insert(resolved.to_string(), names);

        Ok(ns_name)
    }

    fn rewrite_module(&mut self, module: Module, referrer: &str) -> Result<Module, ModuleError> {
        let mut out = Vec::with_capacity(module.body.len());
        for stmt in module.body {
            self.rewrite_stmt(stmt, referrer, &mut out)?;
        }
        Ok(Module {
            name: module.name,
            body: out,
            node_count: module.node_count,
        })
    }

    fn rewrite_stmt(
        &mut self,
        stmt: Stmt,
        referrer: &str,
        out: &mut Vec<Stmt>,
    ) -> Result<(), ModuleError> {
        let id = stmt.id;
        let span = stmt.span;
        let line = span.start.line;
        match stmt.kind {
            StmtKind::Import(decl) => {
                if decl.type_only {
                    return Ok(());
                }
                let resolved = self.resolve(&decl.source, referrer)?;
                let ns_name = self.load_dependency(&resolved)?;
                for spec in &decl.specifiers {
                    match spec {
                        ImportSpecifier::Default(local) => out.push(const_decl(
                            local,
                            member_expr(ident_expr(&ns_name, line), "default", line),
                            line,
                        )),
                        ImportSpecifier::Namespace(local) => {
                            out.push(const_decl(local, ident_expr(&ns_name, line), line))
                        }
                        ImportSpecifier::Named { imported, local } => out.push(const_decl(
                            local,
                            member_expr(ident_expr(&ns_name, line), imported, line),
                            line,
                        )),
                    }
                }
            }
            StmtKind::ExportNamed {
                specifiers,
                source: Some(src),
                type_only,
            } => {
                if type_only {
                    return Ok(());
                }
                let resolved = self.resolve(&src, referrer)?;
                let ns_name = self.load_dependency(&resolved)?;
                for spec in specifiers.into_vec() {
                    let ast::ExportSpecifier::Named { local, exported } = spec;
                    self.push_reexport(out, &ns_name, &local, exported, line);
                }
            }
            StmtKind::ExportAll {
                as_name: Some(ns_local),
                source,
            } => {
                let resolved = self.resolve(&source, referrer)?;
                let ns_name = self.load_dependency(&resolved)?;
                out.push(const_decl(&ns_local, ident_expr(&ns_name, line), line));
                out.push(export_named(&ns_local, line));
            }
            StmtKind::ExportAll {
                as_name: None,
                source,
            } => {
                let resolved = self.resolve(&source, referrer)?;
                let ns_name = self.load_dependency(&resolved)?;
                let names = self
                    .export_names
                    .get(&resolved)
                    .cloned()
                    .unwrap_or_default();
                for name in names {
                    if &*name == "default" {
                        continue;
                    }
                    self.push_reexport(out, &ns_name, &name, name.to_string(), line);
                }
            }
            other => out.push(Stmt {
                id,
                span,
                kind: other,
            }),
        }
        Ok(())
    }
}

impl Interpreter {
    /// モジュールグラフを `entry_specifier` から評価する。
    ///
    /// 依存モジュール（エントリ以外）はすべて完全に解決するはず、という
    /// 前提で評価する（トップレベル await がホスト Promise 待ちのまま残ると
    /// `ModuleError::Runtime` になる）。エントリモジュールだけは
    /// `ModuleOutcome::Pending` を許す（ゲームループを抱えた
    /// `main.ts` 相当の想定）。
    pub fn run_module_graph(
        &mut self,
        entry_specifier: &str,
        host: &dyn ModuleHost,
    ) -> Result<ModuleOutcome, ModuleError> {
        let mut loader = Loader {
            interp: self,
            host,
            cache: FastHashMap::default(),
            export_names: FastHashMap::default(),
            next_id: 0,
        };
        let resolved = loader.resolve(entry_specifier, "<entry>")?;
        let chunk = loader.compile_resolved(&resolved)?;
        let outcome = loader
            .interp
            .vm()
            .start_module_task(chunk)
            .map_err(|thrown| ModuleError::Runtime {
                resolved: resolved.clone(),
                message: loader.interp.vm().to_display_string(&thrown),
            })?;
        Ok(match outcome {
            ModuleTaskState::Settled(exports) => ModuleOutcome::Settled(exports),
            ModuleTaskState::Pending => ModuleOutcome::Pending,
        })
    }
}
