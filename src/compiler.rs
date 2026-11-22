//! AST → バイトコードのコンパイラ。
//!
//! 設計の要点は `bytecode.rs` の冒頭コメントを参照。ここでは:
//! - ローカル変数はデフォルトで非ボックス（`LocalSlot::Direct`）とし、内側
//!   クロージャから `resolve_upvalue` で実際に捕捉されたスロットだけを
//!   `Chunk::captured_locals` に記録する（`vm.rs` のフレーム生成時にその
//!   スロットのみ `Rc<RefCell<Value>>` を確保する）。
//! - `let`/`const`/`var`/関数宣言/クラス宣言は「ブロック先頭でスロットを
//!   予約してから本体を順にコンパイルする」2 パス方式で処理し、
//!   `let`/`const`/クラスは Uninit のまま予約する（TDZ）。
//! - クロージャの自由変数捕捉は関数コンパイラのスタック
//!   （`Compiler::funcs`）を辿る CLox 風のアルゴリズム。
//! - `break`/`continue`/`return` は `Ctrl` コントロールスタックを辿り、
//!   経路上の `try...finally` があればその `finally` 本体を
//!   「インライン再コンパイル」してから実際のジャンプ/return を発行する。

use std::rc::Rc;

use crate::ast::{self, *};
use crate::bytecode::*;

#[derive(Debug, Clone)]
pub struct CompileError {
    pub message: String,
    pub line: u32,
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "コンパイルエラー {}行目: {}", self.line, self.message)
    }
}

type CResult<T> = Result<T, CompileError>;

fn err(line: u32, msg: impl Into<String>) -> CompileError {
    CompileError {
        message: msg.into(),
        line,
    }
}

// =====================================================================
// スコープ内部表現
// =====================================================================

struct LocalVar {
    name: Rc<str>,
    slot: LocalIdx,
}

/// break/continue/return の経路解決に使うコントロールスタックの要素。
enum Ctrl {
    Loop {
        label: Option<Rc<str>>,
        break_patches: Vec<usize>,
        continue_patches: Vec<usize>,
    },
    Switch {
        break_patches: Vec<usize>,
    },
    Labeled {
        label: Rc<str>,
        break_patches: Vec<usize>,
    },
    TryFinally {
        finally: Rc<[Stmt]>,
    },
    FuncBoundary,
}

struct FuncCtx {
    chunk: Chunk,
    locals: Vec<LocalVar>,
    next_slot: LocalIdx,
    upvalues: Vec<UpvalueDesc>,
    upvalue_names: Vec<Rc<str>>,
    control: Vec<Ctrl>,
    /// 例外の再送出に使う一時ローカルスロット（finally を含む関数のみ確保）。
    exc_slot: Option<LocalIdx>,
}

impl FuncCtx {
    fn new(name: impl Into<Rc<str>>) -> Self {
        FuncCtx {
            chunk: Chunk::new(name),
            locals: Vec::new(),
            next_slot: 0,
            upvalues: Vec::new(),
            upvalue_names: Vec::new(),
            control: vec![Ctrl::FuncBoundary],
            exc_slot: None,
        }
    }

    fn alloc_slot(&mut self) -> LocalIdx {
        let s = self.next_slot;
        self.next_slot += 1;
        if self.next_slot as usize > self.chunk.num_locals {
            self.chunk.num_locals = self.next_slot as usize;
            self.chunk
                .captured_locals
                .resize(self.chunk.num_locals, false);
        }
        s
    }

    fn find_local(&self, name: &str) -> Option<LocalIdx> {
        self.locals
            .iter()
            .rev()
            .find(|l| &*l.name == name)
            .map(|l| l.slot)
    }

    fn get_or_make_exc_slot(&mut self) -> LocalIdx {
        if let Some(s) = self.exc_slot {
            return s;
        }
        let s = self.alloc_slot();
        self.exc_slot = Some(s);
        s
    }
}

enum VarLoc {
    Local(LocalIdx),
    Upvalue(UpvalueIdx),
    Global,
}

pub struct Compiler {
    funcs: Vec<FuncCtx>,
}

// =====================================================================
// エントリポイント
// =====================================================================

/// モジュール全体をトップレベル「関数」としてコンパイルする。
/// import 文が含まれる場合は明確なエラーを返す（T5 でモジュール連携予定）。
pub fn compile_module(module: &Module) -> CResult<Rc<Chunk>> {
    let mut c = Compiler {
        funcs: vec![FuncCtx::new(module.name.clone())],
    };
    c.cur().chunk.is_module = true;
    // モジュールトップレベルの `this` は undefined（strict mode 相当）。
    // スロット 0 を this 用として予約しておく（LoadThis が読む）。
    let this_slot = c.cur().alloc_slot();
    c.cur().locals.push(LocalVar {
        name: "%this".into(),
        slot: this_slot,
    });

    for s in &module.body {
        match &s.kind {
            StmtKind::Import(_) => {
                return Err(err(
                    s.span.start.line,
                    "import 文は T2 では未対応です（T5 で対応予定）",
                ))
            }
            StmtKind::ExportNamed {
                source: Some(_), ..
            }
            | StmtKind::ExportAll { .. } => {
                return Err(err(
                    s.span.start.line,
                    "他モジュールからの re-export は T2 では未対応です（T5 で対応予定）",
                ))
            }
            _ => {}
        }
    }

    c.compile_top_level_stmts(&module.body)?;

    // export 名 -> ローカルスロット の対応表を作る（値の取り出しは実行後に行う）。
    for s in &module.body {
        collect_exports(&mut c, s)?;
    }

    c.emit(OpCode::LoadUndefined, 0);
    c.emit(OpCode::Return, 0);

    let f = c.funcs.pop().unwrap();
    Ok(Rc::new(f.chunk))
}

fn collect_exports(c: &mut Compiler, s: &Stmt) -> CResult<()> {
    match &s.kind {
        StmtKind::ExportDecl(inner) => {
            let names: Vec<String> = match &inner.kind {
                StmtKind::VarDecl { decls, .. } => {
                    decls.iter().flat_map(|d| pattern_names(&d.name)).collect()
                }
                StmtKind::FunctionDecl(f) => f.name.clone().into_iter().collect(),
                StmtKind::ClassDecl(cl) => cl.name.clone().into_iter().collect(),
                _ => Vec::new(),
            };
            for n in names {
                register_export(c, &n, &n, s.span.start.line)?;
            }
        }
        StmtKind::ExportNamed {
            specifiers,
            source: None,
            ..
        } => {
            for spec in specifiers {
                let ExportSpecifier::Named { local, exported } = spec;
                register_export(c, local, exported, s.span.start.line)?;
            }
        }
        StmtKind::ExportDefault(_) => register_export(c, "default", "default", s.span.start.line)?,
        _ => {}
    }
    Ok(())
}

fn register_export(c: &mut Compiler, local: &str, exported: &str, line: u32) -> CResult<()> {
    let slot = c
        .cur()
        .find_local(local)
        .ok_or_else(|| err(line, format!("export 対象 '{local}' が見つかりません")))?;
    c.cur().chunk.exports.push((Rc::from(exported), slot));
    Ok(())
}

/// `new Function(...)` 用: パラメータ名と本体ソースから独立した関数を作る。
/// スコープはグローバル（真の組み込み・ホスト登録グローバル）のみを参照する。
pub fn compile_dynamic_function(param_names: &[String], body_src: &str) -> CResult<Rc<Chunk>> {
    let synthetic = format!(
        "(function anonymous({}) {{\n{}\n}})",
        param_names.join(", "),
        body_src
    );
    let module = crate::parse_module(&synthetic, "<new Function>").map_err(|e| {
        err(
            e.span.start.line,
            format!("new Function の本体解析に失敗: {e}"),
        )
    })?;
    let func_expr = match module.body.first().map(|s| &s.kind) {
        Some(StmtKind::Expr(e)) => match &e.kind {
            ExprKind::Function(f) => f.as_ref(),
            _ => return Err(err(0, "new Function: 内部エラー（関数式ではありません）")),
        },
        _ => return Err(err(0, "new Function: 内部エラー（文の形が想定外です）")),
    };

    let mut c = Compiler { funcs: Vec::new() };
    let template = c.compile_function(func_expr, "anonymous", false)?;
    Ok(template.chunk.clone())
}

// =====================================================================
// Compiler 本体
// =====================================================================

impl Compiler {
    fn cur(&mut self) -> &mut FuncCtx {
        self.funcs.last_mut().unwrap()
    }

    fn cur_addr(&mut self) -> u32 {
        self.cur().chunk.code.len() as u32
    }

    fn emit(&mut self, op: OpCode, line: u32) -> usize {
        let f = self.cur();
        f.chunk.code.push(op);
        f.chunk.lines.push(line);
        f.chunk.code.len() - 1
    }

    fn add_const(&mut self, c: Const) -> ConstIdx {
        let f = self.cur();
        f.chunk.consts.push(c);
        (f.chunk.consts.len() - 1) as ConstIdx
    }

    fn add_const_str(&mut self, s: &str) -> ConstIdx {
        self.add_const(Const::Str(Rc::from(s)))
    }

    fn patch_jump(&mut self, idx: usize, target: u32) {
        let op = &mut self.cur().chunk.code[idx];
        *op = match *op {
            OpCode::Jump(_) => OpCode::Jump(target),
            OpCode::JumpIfFalse(_) => OpCode::JumpIfFalse(target),
            OpCode::JumpIfTrue(_) => OpCode::JumpIfTrue(target),
            OpCode::JumpIfNullish(_) => OpCode::JumpIfNullish(target),
            OpCode::JumpIfNullishCall(_) => OpCode::JumpIfNullishCall(target),
            OpCode::JumpIfFalsyKeep(_) => OpCode::JumpIfFalsyKeep(target),
            OpCode::JumpIfTruthyKeep(_) => OpCode::JumpIfTruthyKeep(target),
            OpCode::JumpIfNotNullishKeep(_) => OpCode::JumpIfNotNullishKeep(target),
            OpCode::JumpIfNotUndefinedKeep(_) => OpCode::JumpIfNotUndefinedKeep(target),
            other => panic!("patch_jump: ジャンプ命令ではありません: {other:?}"),
        };
    }

    fn emit_jump(&mut self, mk: impl Fn(JumpAddr) -> OpCode, line: u32) -> usize {
        self.emit(mk(0), line)
    }

    // ---------------- 変数解決 ----------------

    fn declare_local(&mut self, name: &str) -> LocalIdx {
        let slot = self.cur().alloc_slot();
        self.cur().locals.push(LocalVar {
            name: Rc::from(name),
            slot,
        });
        slot
    }

    fn resolve_upvalue(&mut self, level: usize, name: &str) -> Option<UpvalueIdx> {
        if level == 0 {
            return None;
        }
        let parent = level - 1;
        if let Some(idx) = self.funcs[level]
            .upvalue_names
            .iter()
            .position(|n| &**n == name)
        {
            return Some(idx as UpvalueIdx);
        }
        if let Some(slot) = self.funcs[parent].find_local(name) {
            self.funcs[parent].chunk.captured_locals[slot as usize] = true;
            let f = &mut self.funcs[level];
            f.upvalues.push(UpvalueDesc::ParentLocal(slot));
            f.upvalue_names.push(Rc::from(name));
            return Some((f.upvalues.len() - 1) as UpvalueIdx);
        }
        if let Some(puv) = self.resolve_upvalue(parent, name) {
            let f = &mut self.funcs[level];
            f.upvalues.push(UpvalueDesc::ParentUpvalue(puv));
            f.upvalue_names.push(Rc::from(name));
            return Some((f.upvalues.len() - 1) as UpvalueIdx);
        }
        None
    }

    fn resolve(&mut self, name: &str) -> VarLoc {
        let cur = self.funcs.len() - 1;
        if let Some(slot) = self.funcs[cur].find_local(name) {
            return VarLoc::Local(slot);
        }
        if let Some(uv) = self.resolve_upvalue(cur, name) {
            return VarLoc::Upvalue(uv);
        }
        VarLoc::Global
    }

    fn emit_get_var(&mut self, name: &str, line: u32) {
        match self.resolve(name) {
            VarLoc::Local(s) => {
                self.emit(OpCode::GetLocal(s), line);
            }
            VarLoc::Upvalue(u) => {
                self.emit(OpCode::GetUpvalue(u), line);
            }
            VarLoc::Global => {
                let c = self.add_const_str(name);
                self.emit(OpCode::GetGlobal(c), line);
            }
        }
    }

    fn emit_set_var(&mut self, name: &str, line: u32) {
        match self.resolve(name) {
            VarLoc::Local(s) => {
                self.emit(OpCode::SetLocal(s), line);
            }
            VarLoc::Upvalue(u) => {
                self.emit(OpCode::SetUpvalue(u), line);
            }
            VarLoc::Global => {
                let c = self.add_const_str(name);
                self.emit(OpCode::SetGlobal(c), line);
            }
        }
    }

    fn emit_bind_var(&mut self, name: &str, line: u32) {
        match self.resolve(name) {
            VarLoc::Local(s) => {
                self.emit(OpCode::BindLocal(s), line);
            }
            VarLoc::Upvalue(_) => {
                // 宣言はローカル/グローバルにしか起きない想定（呼び出し側で保証）。
                let c = self.add_const_str(name);
                self.emit(OpCode::BindGlobal(c), line);
            }
            VarLoc::Global => {
                let c = self.add_const_str(name);
                self.emit(OpCode::BindGlobal(c), line);
            }
        }
    }

    // ---------------- ブロックスコープ / 巻き上げ ----------------

    /// ブロック（文リスト）を新しいレキシカルスコープでコンパイルする。
    fn compile_block_new_scope(&mut self, stmts: &[Stmt]) -> CResult<()> {
        let saved_len = self.cur().locals.len();
        self.hoist_and_compile(stmts)?;
        self.cur().locals.truncate(saved_len);
        Ok(())
    }

    /// モジュール/関数のトップレベル文列（新しいスコープを push しない。
    /// 既に関数プロローグでスロットを予約済みの場合に使う）。
    fn compile_top_level_stmts(&mut self, stmts: &[Stmt]) -> CResult<()> {
        self.hoist_and_compile(stmts)
    }

    /// 文リストの「巻き上げ」: 直接の子要素にある var/let/const/function/class
    /// 宣言の名前を先に登録してから、通常の順で文をコンパイルする。
    fn hoist_and_compile(&mut self, stmts: &[Stmt]) -> CResult<()> {
        // 1st pass: 名前の登録（TDZ 用に let/const/class は Uninit で確保）。
        for s in stmts {
            self.hoist_stmt_names(s)?;
        }
        // 2nd pass: 関数宣言の本体を先に評価（相互再帰を許すため）。
        for s in stmts {
            if let StmtKind::FunctionDecl(f) = &s.kind {
                self.compile_function_decl_value(f, s.span.start.line)?;
            }
            if let StmtKind::ExportDecl(inner) = &s.kind {
                if let StmtKind::FunctionDecl(f) = &inner.kind {
                    self.compile_function_decl_value(f, s.span.start.line)?;
                }
            }
        }
        // 3rd pass: 通常のコンパイル。
        for s in stmts {
            self.compile_stmt(s)?;
        }
        Ok(())
    }

    fn hoist_stmt_names(&mut self, s: &Stmt) -> CResult<()> {
        match &s.kind {
            StmtKind::VarDecl { decls, .. } => {
                for d in decls {
                    for name in pattern_names(&d.name) {
                        self.declare_local(&name);
                    }
                }
            }
            StmtKind::FunctionDecl(f) => {
                if let Some(name) = &f.name {
                    self.declare_local(name);
                }
            }
            StmtKind::ClassDecl(c) => {
                if let Some(name) = &c.name {
                    self.declare_local(name);
                }
            }
            StmtKind::ExportDecl(inner) => self.hoist_stmt_names(inner)?,
            StmtKind::ExportDefault(_) => {
                self.declare_local("default");
            }
            _ => {}
        }
        Ok(())
    }

    // ---------------- 文コンパイル ----------------

    fn compile_stmt(&mut self, s: &Stmt) -> CResult<()> {
        let line = s.span.start.line;
        match &s.kind {
            StmtKind::Empty | StmtKind::Debugger => {}
            StmtKind::Expr(e) => {
                self.compile_expr(e)?;
                self.emit(OpCode::Pop, line);
            }
            StmtKind::Block(b) => self.compile_block_new_scope(b)?,
            StmtKind::VarDecl { decls, .. } => {
                for d in decls {
                    if let Some(init) = &d.init {
                        self.compile_expr(init)?;
                    } else {
                        self.emit(OpCode::LoadUndefined, line);
                    }
                    self.compile_bind_pattern(&d.name, line)?;
                }
            }
            StmtKind::FunctionDecl(_) => {
                // 巻き上げ済み（hoist_and_compile の 2nd pass）なのでここでは何もしない。
            }
            StmtKind::ClassDecl(c) => {
                self.compile_class_value(c, line)?;
                if let Some(name) = &c.name {
                    self.emit_bind_var(name, line);
                }
            }
            StmtKind::If { test, cons, alt } => {
                self.compile_expr(test)?;
                let jf = self.emit_jump(OpCode::JumpIfFalse, line);
                self.compile_stmt(cons)?;
                if let Some(alt) = alt {
                    let jend = self.emit_jump(OpCode::Jump, line);
                    let else_addr = self.cur_addr();
                    self.patch_jump(jf, else_addr);
                    self.compile_stmt(alt)?;
                    let end = self.cur_addr();
                    self.patch_jump(jend, end);
                } else {
                    let end = self.cur_addr();
                    self.patch_jump(jf, end);
                }
            }
            StmtKind::While { test, body } => self.compile_while(None, test, body)?,
            StmtKind::DoWhile { body, test } => self.compile_do_while(None, body, test)?,
            StmtKind::For {
                init,
                test,
                update,
                body,
            } => self.compile_for(
                None,
                init.as_deref(),
                test.as_deref(),
                update.as_deref(),
                body,
            )?,
            StmtKind::ForIn {
                left,
                right,
                body,
                is_of,
                ..
            } => self.compile_for_in_of(None, left, right, body, *is_of, line)?,
            StmtKind::Switch { disc, cases } => self.compile_switch(None, disc, cases, line)?,
            StmtKind::Try {
                block,
                handler,
                finalizer,
            } => self.compile_try(block, handler.as_ref(), finalizer.as_deref(), line)?,
            StmtKind::Throw(e) => {
                self.compile_expr(e)?;
                self.emit(OpCode::Throw, line);
            }
            StmtKind::Return(e) => self.compile_return(e.as_deref(), line)?,
            StmtKind::Break(label) => self.compile_break(label.as_deref(), line)?,
            StmtKind::Continue(label) => self.compile_continue(label.as_deref(), line)?,
            StmtKind::Labeled { label, body } => self.compile_labeled(label, body, line)?,
            StmtKind::Import(_) => return Err(err(line, "import 文は未対応です")),
            StmtKind::ExportNamed { .. } | StmtKind::ExportAll { .. } => {
                // Chunk.exports への登録は compile_module 側で扱う（下記参照）。
                // ここでは export 対象自体は既存の束縛を指すだけなので何もしない。
            }
            StmtKind::ExportDefault(d) => {
                match d.as_ref() {
                    DefaultExportable::Expr(e) => self.compile_expr(e)?,
                    DefaultExportable::Function(f) => {
                        let name = f.name.clone().unwrap_or_default();
                        let tpl = self.compile_function(f, &name, false)?;
                        let c = self.add_const(Const::FunctionTemplate(Rc::new(tpl)));
                        self.emit(OpCode::MakeClosure(c), line);
                    }
                    DefaultExportable::Class(c) => self.compile_class_value(c, line)?,
                }
                self.emit_bind_var("default", line);
            }
            StmtKind::ExportDecl(inner) => self.compile_stmt(inner)?,
            StmtKind::TsInterface { .. }
            | StmtKind::TsTypeAlias { .. }
            | StmtKind::TsAmbientDecl => {}
            StmtKind::TsModule { body, .. } => self.compile_block_new_scope(body)?,
        }
        Ok(())
    }

    fn compile_function_decl_value(&mut self, f: &Function, line: u32) -> CResult<()> {
        let name = f.name.clone().unwrap_or_default();
        let template = self.compile_function(f, &name, false)?;
        let cidx = self.add_const(Const::FunctionTemplate(Rc::new(template)));
        self.emit(OpCode::MakeClosure(cidx), line);
        self.emit_bind_var(&name, line);
        Ok(())
    }

    // ---------------- ループ ----------------

    fn compile_while(&mut self, label: Option<Rc<str>>, test: &Expr, body: &Stmt) -> CResult<()> {
        let line = test.span.start.line;
        let test_addr = self.cur_addr();
        self.compile_expr(test)?;
        let jf = self.emit_jump(OpCode::JumpIfFalse, line);
        self.cur().control.push(Ctrl::Loop {
            label,
            break_patches: Vec::new(),
            continue_patches: Vec::new(),
        });
        self.compile_stmt(body)?;
        let cont_addr = self.cur_addr();
        self.resolve_loop_continue(cont_addr);
        self.emit(OpCode::Jump(test_addr), line);
        let end = self.cur_addr();
        self.patch_jump(jf, end);
        self.pop_loop_breaks(end);
        Ok(())
    }

    fn compile_do_while(
        &mut self,
        label: Option<Rc<str>>,
        body: &Stmt,
        test: &Expr,
    ) -> CResult<()> {
        let line = test.span.start.line;
        let body_addr = self.cur_addr();
        self.cur().control.push(Ctrl::Loop {
            label,
            break_patches: Vec::new(),
            continue_patches: Vec::new(),
        });
        self.compile_stmt(body)?;
        let cont_addr = self.cur_addr();
        self.resolve_loop_continue(cont_addr);
        self.compile_expr(test)?;
        self.emit(OpCode::JumpIfTrue(body_addr), line);
        let end = self.cur_addr();
        self.pop_loop_breaks(end);
        Ok(())
    }

    fn compile_for(
        &mut self,
        label: Option<Rc<str>>,
        init: Option<&ForInit>,
        test: Option<&Expr>,
        update: Option<&Expr>,
        body: &Stmt,
    ) -> CResult<()> {
        let line = body.span.start.line;
        let saved_len = self.cur().locals.len();
        // let/const で宣言されたループ変数のスロット（`var` は対象外）。
        // JS の for 文は「イテレーションごとに新しい束縛」を要求する
        // （closure がループ変数を捕捉した場合に観測できる違い）ため、
        // 初回テスト直前と各イテレーション末尾（continue の合流点）で
        // セルを丸ごと差し替える（`OpCode::RebindLocal`）。
        let mut per_iter_slots: Vec<LocalIdx> = Vec::new();
        if let Some(init) = init {
            match init {
                ForInit::VarDecl(kind, decls) => {
                    for d in decls {
                        for name in pattern_names(&d.name) {
                            let slot = self.declare_local(&name);
                            if *kind != ast::VarKind::Var {
                                per_iter_slots.push(slot);
                            }
                        }
                        if let Some(e) = &d.init {
                            self.compile_expr(e)?;
                        } else {
                            self.emit(OpCode::LoadUndefined, line);
                        }
                        self.compile_bind_pattern(&d.name, line)?;
                    }
                }
                ForInit::Expr(e) => {
                    self.compile_expr(e)?;
                    self.emit(OpCode::Pop, line);
                }
            }
        }
        for &slot in &per_iter_slots {
            self.emit(OpCode::RebindLocal(slot), line);
        }
        let test_addr = self.cur_addr();
        let jf = if let Some(test) = test {
            self.compile_expr(test)?;
            Some(self.emit_jump(OpCode::JumpIfFalse, line))
        } else {
            None
        };
        self.cur().control.push(Ctrl::Loop {
            label,
            break_patches: Vec::new(),
            continue_patches: Vec::new(),
        });
        self.compile_stmt(body)?;
        let cont_addr = self.cur_addr();
        self.resolve_loop_continue(cont_addr);
        for &slot in &per_iter_slots {
            self.emit(OpCode::RebindLocal(slot), line);
        }
        if let Some(u) = update {
            self.compile_expr(u)?;
            self.emit(OpCode::Pop, line);
        }
        self.emit(OpCode::Jump(test_addr), line);
        let end = self.cur_addr();
        if let Some(jf) = jf {
            self.patch_jump(jf, end);
        }
        self.pop_loop_breaks(end);
        self.cur().locals.truncate(saved_len);
        Ok(())
    }

    /// for-of / for-in を、値をいったん配列に実体化してインデックスで回る
    /// 形にコンパイルする（明示スタック方針: イテレータ状態も全部ローカル
    /// スロット＝ヒープ上のフレームに載る）。for-in はプレーンオブジェクトの
    /// 列挙可能な自前キー一覧を使う（コーパスでの用途を満たす簡易実装）。
    fn compile_for_in_of(
        &mut self,
        label: Option<Rc<str>>,
        left: &ForInit,
        right: &Expr,
        body: &Stmt,
        is_of: bool,
        line: u32,
    ) -> CResult<()> {
        let saved_len = self.cur().locals.len();
        self.compile_expr(right)?;
        if is_of {
            self.emit(OpCode::MakeIterArray, line);
        } else {
            self.emit(OpCode::ObjectKeysArray, line);
        }
        let arr_slot = self.declare_local("%iter_arr");
        self.emit(OpCode::BindLocal(arr_slot), line);
        let c0 = self.const_num(0.0);
        self.emit(OpCode::LoadConst(c0), line);
        let idx_slot = self.declare_local("%iter_idx");
        self.emit(OpCode::BindLocal(idx_slot), line);

        let test_addr = self.cur_addr();
        self.emit(OpCode::GetLocal(idx_slot), line);
        self.emit(OpCode::GetLocal(arr_slot), line);
        let len_c = self.add_const_str("length");
        self.emit(OpCode::GetProp(len_c), line);
        self.emit(OpCode::Lt, line);
        let jf = self.emit_jump(OpCode::JumpIfFalse, line);

        self.cur().control.push(Ctrl::Loop {
            label,
            break_patches: Vec::new(),
            continue_patches: Vec::new(),
        });
        let body_saved_len = self.cur().locals.len();
        self.emit(OpCode::GetLocal(arr_slot), line);
        self.emit(OpCode::GetLocal(idx_slot), line);
        self.emit(OpCode::GetIndex, line);
        match left {
            ForInit::VarDecl(_, decls) => {
                let pat = &decls[0].name;
                for name in pattern_names(pat) {
                    self.declare_local(&name);
                }
                self.compile_bind_pattern(pat, line)?;
            }
            ForInit::Expr(e) => self.compile_assign_target_expr(e, line)?,
        }
        self.compile_stmt(body)?;
        self.cur().locals.truncate(body_saved_len);
        let cont_addr = self.cur_addr();
        self.resolve_loop_continue(cont_addr);
        self.emit(OpCode::GetLocal(idx_slot), line);
        let c1 = self.const_num(1.0);
        self.emit(OpCode::LoadConst(c1), line);
        self.emit(OpCode::Add, line);
        self.emit(OpCode::BindLocal(idx_slot), line);
        self.emit(OpCode::Jump(test_addr), line);

        let end = self.cur_addr();
        self.patch_jump(jf, end);
        self.pop_loop_breaks(end);
        self.cur().locals.truncate(saved_len);
        Ok(())
    }

    fn const_num(&mut self, n: f64) -> ConstIdx {
        self.add_const(Const::Number(n))
    }

    fn resolve_loop_continue(&mut self, addr: u32) {
        if let Some(Ctrl::Loop {
            continue_patches, ..
        }) = self.cur().control.last_mut()
        {
            let patches = std::mem::take(continue_patches);
            for idx in patches {
                self.patch_jump(idx, addr);
            }
        }
    }

    fn pop_loop_breaks(&mut self, end_addr: u32) {
        if let Some(Ctrl::Loop { break_patches, .. }) = self.cur().control.pop() {
            for idx in break_patches {
                self.patch_jump(idx, end_addr);
            }
        }
    }

    fn compile_labeled(&mut self, label: &str, body: &Stmt, line: u32) -> CResult<()> {
        let label_rc: Rc<str> = Rc::from(label);
        match &body.kind {
            StmtKind::While { test, body: b } => self.compile_while(Some(label_rc), test, b),
            StmtKind::DoWhile { body: b, test } => self.compile_do_while(Some(label_rc), b, test),
            StmtKind::For {
                init,
                test,
                update,
                body: b,
            } => self.compile_for(
                Some(label_rc),
                init.as_deref(),
                test.as_deref(),
                update.as_deref(),
                b,
            ),
            StmtKind::ForIn {
                left,
                right,
                body: b,
                is_of,
                ..
            } => self.compile_for_in_of(Some(label_rc), left, right, b, *is_of, line),
            _ => {
                self.cur().control.push(Ctrl::Labeled {
                    label: label_rc,
                    break_patches: Vec::new(),
                });
                self.compile_stmt(body)?;
                let end = self.cur_addr();
                if let Some(Ctrl::Labeled { break_patches, .. }) = self.cur().control.pop() {
                    for idx in break_patches {
                        self.patch_jump(idx, end);
                    }
                }
                Ok(())
            }
        }
    }

    fn compile_switch(
        &mut self,
        label: Option<Rc<str>>,
        disc: &Expr,
        cases: &[SwitchCase],
        line: u32,
    ) -> CResult<()> {
        self.compile_expr(disc)?;
        let saved_len = self.cur().locals.len();
        // 1st: switch 本体全体を 1 つのブロックスコープとして巻き上げる。
        let mut all_stmts: Vec<&Stmt> = Vec::new();
        for c in cases {
            for s in &c.body {
                all_stmts.push(s);
            }
        }
        for s in &all_stmts {
            self.hoist_stmt_names(s)?;
        }
        for s in &all_stmts {
            if let StmtKind::FunctionDecl(f) = &s.kind {
                self.compile_function_decl_value(f, s.span.start.line)?;
            }
        }

        let mut body_jumps: Vec<usize> = Vec::new();
        let mut default_jump: Option<usize> = None;
        for (i, c) in cases.iter().enumerate() {
            if let Some(test) = &c.test {
                self.emit(OpCode::Dup, line);
                self.compile_expr(test)?;
                self.emit(OpCode::StrictEq, line);
                let j = self.emit_jump(OpCode::JumpIfTrue, line);
                body_jumps.push(j);
            } else {
                default_jump = Some(i);
            }
        }
        let has_default = default_jump.is_some();
        // どの case にもマッチしなかった時のジャンプ。default があればそこへ、
        // 無ければ switch の終端へ（終端で discriminant を捨てる）。
        let end_jump_if_no_match = self.emit_jump(OpCode::Jump, line);

        // 判別値 (discriminant) は「ジャンプで case 本体へ入る瞬間」に
        // ちょうど 1 回だけ捨てたい。しかし case 本体はフォールスルーで
        // 連続しており、後続 case の先頭にも別ルート（直接マッチ）から
        // ジャンプしてくる可能性があるため、本体先頭で Pop すると
        // フォールスルー経由の 2 回目の Pop でスタックが枯渇する。
        // そこで case ごとに「Pop してから本体先頭へジャンプする」小さな
        // トランポリンを用意し、フォールスルーはトランポリンを経由せず
        // 本体同士を直接連結する。
        let mut trampolines: Vec<usize> = Vec::new(); // トランポリンの Jump 命令アドレス
        for _ in cases {
            self.emit(OpCode::Pop, line);
            trampolines.push(self.emit_jump(OpCode::Jump, line));
        }
        let end_trampoline_pop = self.cur_addr();
        self.emit(OpCode::Pop, line);
        let end_trampoline_jump = self.emit_jump(OpCode::Jump, line);

        let mut ti = 0usize;
        for (i, c) in cases.iter().enumerate() {
            let tramp_addr = trampolines[i].saturating_sub(1) as u32; // 対応する Pop 命令の位置
            if c.test.is_some() {
                self.patch_jump(body_jumps[ti], tramp_addr);
                ti += 1;
            } else if default_jump == Some(i) {
                self.patch_jump(end_jump_if_no_match, tramp_addr);
            }
        }
        if !has_default {
            self.patch_jump(end_jump_if_no_match, end_trampoline_pop);
        }

        self.cur().control.push(Ctrl::Switch {
            break_patches: Vec::new(),
        });
        for (i, c) in cases.iter().enumerate() {
            let addr = self.cur_addr();
            self.patch_jump(trampolines[i], addr);
            for s in &c.body {
                self.compile_stmt(s)?;
            }
        }
        let end = self.cur_addr();
        self.patch_jump(end_trampoline_jump, end);
        if let Some(Ctrl::Switch { break_patches }) = self.cur().control.pop() {
            for idx in break_patches {
                self.patch_jump(idx, end);
            }
        }
        let _ = label;
        self.cur().locals.truncate(saved_len);
        Ok(())
    }

    fn compile_break(&mut self, label: Option<&str>, line: u32) -> CResult<()> {
        let len = self.cur().control.len();
        let mut i = len;
        let mut finallies: Vec<(usize, Rc<[Stmt]>)> = Vec::new();
        let target_idx = loop {
            if i == 0 {
                return Err(err(line, "break の対象が見つかりません"));
            }
            i -= 1;
            match &self.cur().control[i] {
                Ctrl::TryFinally { finally } => finallies.push((i, finally.clone())),
                Ctrl::Loop { label: l, .. } if label.is_none() || l.as_deref() == label => break i,
                Ctrl::Switch { .. } if label.is_none() => break i,
                Ctrl::Labeled { label: l, .. } if label.is_some() && Some(&**l) == label => {
                    break i
                }
                Ctrl::FuncBoundary => return Err(err(line, "break の対象が見つかりません")),
                _ => {}
            }
        };
        for (idx, fb) in &finallies {
            self.compile_finally_copy(*idx, fb)?;
        }
        let jidx = self.emit_jump(OpCode::Jump, line);
        match &mut self.cur().control[target_idx] {
            Ctrl::Loop { break_patches, .. }
            | Ctrl::Switch { break_patches }
            | Ctrl::Labeled { break_patches, .. } => break_patches.push(jidx),
            _ => unreachable!(),
        }
        Ok(())
    }

    fn compile_continue(&mut self, label: Option<&str>, line: u32) -> CResult<()> {
        let len = self.cur().control.len();
        let mut i = len;
        let mut finallies: Vec<(usize, Rc<[Stmt]>)> = Vec::new();
        let target_idx = loop {
            if i == 0 {
                return Err(err(line, "continue の対象が見つかりません"));
            }
            i -= 1;
            match &self.cur().control[i] {
                Ctrl::TryFinally { finally } => finallies.push((i, finally.clone())),
                Ctrl::Loop { label: l, .. } if label.is_none() || l.as_deref() == label => break i,
                Ctrl::FuncBoundary => return Err(err(line, "continue の対象が見つかりません")),
                _ => {}
            }
        };
        for (idx, fb) in &finallies {
            self.compile_finally_copy(*idx, fb)?;
        }
        let jidx = self.emit_jump(OpCode::Jump, line);
        if let Ctrl::Loop {
            continue_patches, ..
        } = &mut self.cur().control[target_idx]
        {
            continue_patches.push(jidx);
        }
        Ok(())
    }

    fn compile_return(&mut self, arg: Option<&Expr>, line: u32) -> CResult<()> {
        if let Some(e) = arg {
            self.compile_expr(e)?;
        } else {
            self.emit(OpCode::LoadUndefined, line);
        }
        let len = self.cur().control.len();
        let mut finallies: Vec<(usize, Rc<[Stmt]>)> = Vec::new();
        for i in (0..len).rev() {
            match &self.cur().control[i] {
                Ctrl::TryFinally { finally } => finallies.push((i, finally.clone())),
                Ctrl::FuncBoundary => break,
                _ => {}
            }
        }
        for (idx, fb) in &finallies {
            self.compile_finally_copy(*idx, fb)?;
        }
        self.emit(OpCode::Return, line);
        Ok(())
    }

    // ---------------- try/catch/finally ----------------

    fn compile_try(
        &mut self,
        block: &[Stmt],
        handler: Option<&CatchClause>,
        finalizer: Option<&[Stmt]>,
        line: u32,
    ) -> CResult<()> {
        let finally_rc: Option<Rc<[Stmt]>> =
            finalizer.map(|f| Rc::from(f.to_vec().into_boxed_slice()));
        let control_idx = if let Some(fin) = &finally_rc {
            self.cur().control.push(Ctrl::TryFinally {
                finally: fin.clone(),
            });
            Some(self.cur().control.len() - 1)
        } else {
            None
        };

        let try_start = self.cur_addr();
        self.compile_block_new_scope(block)?;
        let try_end = self.cur_addr();
        if let (Some(fin), Some(idx)) = (&finally_rc, control_idx) {
            self.compile_finally_copy(idx, fin)?;
        }
        let jump_after_try = self.emit_jump(OpCode::Jump, line);

        let catch_addr = self.cur_addr();
        let mut catch_end = catch_addr;
        let mut jump_after_catch = None;
        if let Some(h) = handler {
            let saved_len = self.cur().locals.len();
            if let Some(pat) = &h.param {
                for name in pattern_names(pat) {
                    self.declare_local(&name);
                }
                self.compile_bind_pattern(pat, line)?;
            } else {
                self.emit(OpCode::Pop, line);
            }
            self.hoist_and_compile(&h.body)?;
            self.cur().locals.truncate(saved_len);
            catch_end = self.cur_addr();
            if let (Some(fin), Some(idx)) = (&finally_rc, control_idx) {
                self.compile_finally_copy(idx, fin)?;
            }
            jump_after_catch = Some(self.emit_jump(OpCode::Jump, line));
        }

        if let Some(idx) = control_idx {
            self.cur().control.remove(idx);
        }

        // 例外巻き戻し用の finally コピー（catch が無い場合、または catch 自体が
        // 投げた場合の両方をカバーする）。
        let unwind_addr = if let Some(fin) = &finally_rc {
            let addr = self.cur_addr();
            let exc_slot = self.cur().get_or_make_exc_slot();
            self.emit(OpCode::BindLocal(exc_slot), line);
            self.compile_block_new_scope(fin)?;
            self.emit(OpCode::GetLocal(exc_slot), line);
            self.emit(OpCode::Throw, line);
            Some(addr)
        } else {
            None
        };

        let end = self.cur_addr();
        self.patch_jump(jump_after_try, end);
        if let Some(j) = jump_after_catch {
            self.patch_jump(j, end);
        }

        if handler.is_some() {
            self.cur().chunk.exception_table.push(ExceptionEntry {
                try_start,
                try_end,
                handler_addr: catch_addr,
            });
            if let Some(unwind) = unwind_addr {
                self.cur().chunk.exception_table.push(ExceptionEntry {
                    try_start: catch_addr,
                    try_end: catch_end,
                    handler_addr: unwind,
                });
            }
        } else if let Some(unwind) = unwind_addr {
            self.cur().chunk.exception_table.push(ExceptionEntry {
                try_start,
                try_end,
                handler_addr: unwind,
            });
        }
        Ok(())
    }

    fn compile_finally_copy(&mut self, control_idx: usize, finally: &[Stmt]) -> CResult<()> {
        let saved = self.cur().control.remove(control_idx);
        let r = self.compile_block_new_scope(finally);
        self.cur().control.insert(control_idx, saved);
        r
    }

    // ---------------- 分割代入（宣言側: Pattern） ----------------

    fn compile_bind_pattern(&mut self, pat: &Pattern, line: u32) -> CResult<()> {
        match pat {
            Pattern::Ident(name) => self.emit_bind_var(name, line),
            Pattern::Assign { left, right } => {
                let j = self.emit_jump(OpCode::JumpIfNotUndefinedKeep, line);
                self.compile_expr(right)?;
                let after = self.cur_addr();
                self.patch_jump(j, after);
                self.compile_bind_pattern(left, line)?;
            }
            Pattern::Object { props, rest } => {
                let mut used_keys: Vec<String> = Vec::new();
                for p in props {
                    self.emit(OpCode::Dup, line);
                    match &p.key {
                        PropertyKey::Computed(ke) => {
                            self.compile_expr(ke)?;
                            self.emit(OpCode::GetIndex, line);
                        }
                        _ => {
                            let name = property_key_static_name(&p.key);
                            used_keys.push(name.clone());
                            let c = self.add_const_str(&name);
                            self.emit(OpCode::GetProp(c), line);
                        }
                    }
                    self.compile_bind_pattern(&p.value, line)?;
                }
                if let Some(rest) = rest {
                    self.emit(OpCode::Dup, line);
                    for k in &used_keys {
                        let c = self.add_const_str(k);
                        self.emit(OpCode::LoadConst(c), line);
                    }
                    self.emit(OpCode::ObjectRestExclude(used_keys.len() as u16), line);
                    self.compile_bind_pattern(rest, line)?;
                }
                self.emit(OpCode::Pop, line);
            }
            Pattern::Array { elements, rest } => {
                self.emit(OpCode::MakeIterArray, line);
                for (i, el) in elements.iter().enumerate() {
                    if let Some(subpat) = el {
                        self.emit(OpCode::Dup, line);
                        let c = self.const_num(i as f64);
                        self.emit(OpCode::LoadConst(c), line);
                        self.emit(OpCode::GetIndex, line);
                        self.compile_bind_pattern(subpat, line)?;
                    }
                }
                if let Some(rest) = rest {
                    self.emit(OpCode::Dup, line);
                    let c = self.const_num(elements.len() as f64);
                    self.emit(OpCode::LoadConst(c), line);
                    self.emit(OpCode::ArraySliceFrom, line);
                    self.compile_bind_pattern(rest, line)?;
                }
                self.emit(OpCode::Pop, line);
            }
        }
        Ok(())
    }

    // ---------------- 分割代入（代入式側: Expr を疑似パターンとして扱う） ----------------

    fn compile_assign_target_expr(&mut self, target: &Expr, line: u32) -> CResult<()> {
        match &target.kind {
            ExprKind::ArrayLit(elements) => {
                self.emit(OpCode::MakeIterArray, line);
                let mut idx = 0usize;
                for el in elements {
                    match el {
                        None => {
                            idx += 1;
                        }
                        Some(e) => {
                            if let ExprKind::Spread(inner) = &e.kind {
                                self.emit(OpCode::Dup, line);
                                let c = self.const_num(idx as f64);
                                self.emit(OpCode::LoadConst(c), line);
                                self.emit(OpCode::ArraySliceFrom, line);
                                self.compile_assign_target_expr(inner, line)?;
                                idx += 1;
                                continue;
                            }
                            self.emit(OpCode::Dup, line);
                            let c = self.const_num(idx as f64);
                            self.emit(OpCode::LoadConst(c), line);
                            self.emit(OpCode::GetIndex, line);
                            self.compile_assign_target_leaf(e, line)?;
                            idx += 1;
                        }
                    }
                }
                self.emit(OpCode::Pop, line);
            }
            ExprKind::ObjectLit(props) => {
                let mut used_keys = Vec::new();
                for p in props {
                    match p {
                        ObjectProp::KeyValue { key, value, .. } => {
                            self.emit(OpCode::Dup, line);
                            match key {
                                PropertyKey::Computed(ke) => {
                                    self.compile_expr(ke)?;
                                    self.emit(OpCode::GetIndex, line);
                                }
                                _ => {
                                    let name = property_key_static_name(key);
                                    used_keys.push(name.clone());
                                    let c = self.add_const_str(&name);
                                    self.emit(OpCode::GetProp(c), line);
                                }
                            }
                            self.compile_assign_target_leaf(value, line)?;
                        }
                        ObjectProp::Spread(inner) => {
                            self.emit(OpCode::Dup, line);
                            for k in &used_keys {
                                let c = self.add_const_str(k);
                                self.emit(OpCode::LoadConst(c), line);
                            }
                            self.emit(OpCode::ObjectRestExclude(used_keys.len() as u16), line);
                            self.compile_assign_target_expr(inner, line)?;
                        }
                        ObjectProp::Method { .. } => {
                            return Err(err(line, "分割代入の左辺にメソッド記法は使えません"))
                        }
                    }
                }
                self.emit(OpCode::Pop, line);
            }
            _ => self.compile_assign_target_leaf(target, line)?,
        }
        Ok(())
    }

    /// 分割代入の「葉」1 個分（デフォルト値付きの可能性あり）を、スタック上の
    /// 値を消費してバインドする。
    fn compile_assign_target_leaf(&mut self, target: &Expr, line: u32) -> CResult<()> {
        if let ExprKind::Assign {
            op: AssignOp::Assign,
            target: inner_target,
            value: default,
        } = &target.kind
        {
            let j = self.emit_jump(OpCode::JumpIfNotUndefinedKeep, line);
            self.compile_expr(default)?;
            let after = self.cur_addr();
            self.patch_jump(j, after);
            return self.compile_assign_target_leaf(inner_target, line);
        }
        match &target.kind {
            ExprKind::ArrayLit(_) | ExprKind::ObjectLit(_) => {
                self.compile_assign_target_expr(target, line)
            }
            ExprKind::Ident(name) => {
                self.emit_set_var(name, line);
                self.emit(OpCode::Pop, line);
                Ok(())
            }
            ExprKind::Member { obj, prop, .. } => {
                self.compile_expr(obj)?;
                self.emit(OpCode::Swap, line);
                match prop {
                    PropertyKey::Computed(ke) => {
                        self.compile_expr(ke)?;
                        self.emit(OpCode::Swap, line);
                        self.emit(OpCode::SetIndex, line);
                    }
                    _ => {
                        let name = property_key_static_name(prop);
                        let c = self.add_const_str(&name);
                        self.emit(OpCode::SetProp(c), line);
                    }
                }
                self.emit(OpCode::Pop, line);
                Ok(())
            }
            _ => Err(err(line, "分割代入の左辺として使えない式です")),
        }
    }

    // ---------------- 関数 ----------------

    fn compile_function(
        &mut self,
        f: &Function,
        name: &str,
        is_method: bool,
    ) -> CResult<FunctionTemplate> {
        self.funcs.push(FuncCtx::new(name));
        if !f.is_arrow {
            let this_slot = self.cur().alloc_slot();
            self.cur().locals.push(LocalVar {
                name: "%this".into(),
                slot: this_slot,
            });
        }
        let _ = is_method;

        let mut has_rest = false;
        for (i, p) in f.params.iter().enumerate() {
            let line = f.span.start.line;
            if p.rest {
                has_rest = true;
                self.emit(OpCode::GetRestArgs(i as u16), line);
                for name in pattern_names(&p.pattern) {
                    self.declare_local(&name);
                }
                self.compile_bind_pattern(&p.pattern, line)?;
                continue;
            }
            self.emit(OpCode::GetArg(i as u16), line);
            if let Some(def) = &p.default {
                let j = self.emit_jump(OpCode::JumpIfNotUndefinedKeep, line);
                self.compile_expr(def)?;
                let after = self.cur_addr();
                self.patch_jump(j, after);
            }
            for name in pattern_names(&p.pattern) {
                self.declare_local(&name);
            }
            self.compile_bind_pattern(&p.pattern, line)?;
        }

        if let Some(body) = &f.body {
            self.hoist_and_compile(body)?;
            let line = f.span.end.line;
            self.emit(OpCode::LoadUndefined, line);
            self.emit(OpCode::Return, line);
        } else if let Some(expr_body) = &f.expr_body {
            self.compile_expr(expr_body)?;
            self.emit(OpCode::Return, expr_body.span.start.line);
        } else {
            self.emit(OpCode::LoadUndefined, f.span.start.line);
            self.emit(OpCode::Return, f.span.start.line);
        }

        let fctx = self.funcs.pop().unwrap();
        Ok(FunctionTemplate {
            name: Rc::from(name),
            chunk: Rc::new(fctx.chunk),
            upvalues: fctx.upvalues,
            is_arrow: f.is_arrow,
            is_async: f.is_async,
            param_count: f.params.iter().filter(|p| !p.rest).count(),
            has_rest,
        })
    }

    // ---------------- クラス ----------------

    fn compile_class_value(&mut self, c: &Class, line: u32) -> CResult<()> {
        if c.super_class.is_some() {
            return Err(err(
                line,
                "class の extends は T2 では未対応です（コーパスに無いため省略）",
            ));
        }
        let name = c
            .name
            .clone()
            .unwrap_or_else(|| "<anonymous class>".to_string());
        let mut ctor_tpl: Option<Rc<FunctionTemplate>> = None;
        let mut methods = Vec::new();
        let mut accessors = Vec::new();
        let mut static_fields = Vec::new();
        let mut instance_fields = Vec::new();

        for m in &c.members {
            let mname = property_key_static_name(&m.name);
            if mname == "<static-block>" {
                continue;
            }
            match m.kind {
                ClassMemberKind::Constructor => {
                    let f = m.function.as_ref().unwrap();
                    let tpl = self.compile_function(f, "constructor", true)?;
                    ctor_tpl = Some(Rc::new(tpl));
                }
                ClassMemberKind::Method => {
                    let f = m.function.as_ref().unwrap();
                    let tpl = self.compile_function(f, &mname, true)?;
                    methods.push(ClassMethodTemplate {
                        name: Rc::from(mname.as_str()),
                        is_static: m.is_static,
                        func: Rc::new(tpl),
                    });
                }
                ClassMemberKind::Getter | ClassMemberKind::Setter => {
                    let f = m.function.as_ref().unwrap();
                    let tpl = self.compile_function(f, &mname, true)?;
                    accessors.push(ClassAccessorTemplate {
                        name: Rc::from(mname.as_str()),
                        is_static: m.is_static,
                        is_get: m.kind == ClassMemberKind::Getter,
                        func: Rc::new(tpl),
                    });
                }
                ClassMemberKind::Field => {
                    let init_tpl = if let Some(v) = &m.value {
                        Some(self.compile_field_initializer(v)?)
                    } else {
                        None
                    };
                    if m.is_static {
                        static_fields.push((Rc::from(mname.as_str()), init_tpl));
                    } else {
                        instance_fields.push((Rc::from(mname.as_str()), init_tpl));
                    }
                }
            }
        }

        let template = ClassTemplate {
            name: Rc::from(name.as_str()),
            ctor: ctor_tpl,
            methods,
            accessors,
            static_fields,
            instance_fields,
        };
        let cidx = self.add_const(Const::ClassTemplate(Rc::new(template)));
        self.emit(OpCode::MakeClass(cidx), line);
        Ok(())
    }

    /// フィールド初期化式を「引数なしのミニ関数」としてコンパイルする。
    /// メソッドとまったく同じクロージャ捕捉の仕組み（アップバリュー記述）を
    /// 使い回せる。`this` はスロット 0（`new` の際にインスタンスを渡して呼ぶ）。
    fn compile_field_initializer(&mut self, e: &Expr) -> CResult<Rc<FunctionTemplate>> {
        self.funcs.push(FuncCtx::new("<field-init>"));
        let this_slot = self.cur().alloc_slot();
        self.cur().locals.push(LocalVar {
            name: "%this".into(),
            slot: this_slot,
        });
        self.compile_expr(e)?;
        self.emit(OpCode::Return, e.span.start.line);
        let fctx = self.funcs.pop().unwrap();
        Ok(Rc::new(FunctionTemplate {
            name: Rc::from("<field-init>"),
            chunk: Rc::new(fctx.chunk),
            upvalues: fctx.upvalues,
            is_arrow: false,
            is_async: false,
            param_count: 0,
            has_rest: false,
        }))
    }

    // ---------------- 式コンパイル ----------------

    fn compile_expr(&mut self, e: &Expr) -> CResult<()> {
        let line = e.span.start.line;
        match &e.kind {
            ExprKind::NumberLit(n) => {
                let c = self.const_num(*n);
                self.emit(OpCode::LoadConst(c), line);
            }
            ExprKind::StringLit(s) => {
                let c = self.add_const_str(s);
                self.emit(OpCode::LoadConst(c), line);
            }
            ExprKind::BoolLit(b) => {
                self.emit(
                    if *b {
                        OpCode::LoadTrue
                    } else {
                        OpCode::LoadFalse
                    },
                    line,
                );
            }
            ExprKind::NullLit => {
                self.emit(OpCode::LoadNull, line);
            }
            ExprKind::UndefinedLit => {
                self.emit(OpCode::LoadUndefined, line);
            }
            ExprKind::Ident(name) => self.emit_get_var(name, line),
            ExprKind::This => self.emit_get_var("%this", line),
            ExprKind::Super => {
                return Err(err(
                    line,
                    "super は T2 では未対応です（extends 未対応のため）",
                ))
            }
            ExprKind::TemplateLit { quasis, exprs } => {
                self.compile_template(quasis, exprs, line)?
            }
            ExprKind::RegexLit { pattern, flags } => {
                let re = crate::regex::Regex::compile(pattern, flags).map_err(|e2| {
                    err(
                        line,
                        format!("正規表現 /{pattern}/ を解釈できません: {}", e2.0),
                    )
                })?;
                let c = self.add_const(Const::Regex(Rc::new(re)));
                self.emit(OpCode::LoadRegex(c), line);
            }
            ExprKind::ArrayLit(elements) => {
                self.emit(OpCode::NewArray, line);
                for el in elements {
                    match el {
                        None => {
                            self.emit(OpCode::LoadUndefined, line);
                            self.emit(OpCode::ArrayPush, line);
                        }
                        Some(e) => {
                            if let ExprKind::Spread(inner) = &e.kind {
                                self.compile_expr(inner)?;
                                self.emit(OpCode::SpreadAppendArray, line);
                            } else {
                                self.compile_expr(e)?;
                                self.emit(OpCode::ArrayPush, line);
                            }
                        }
                    }
                }
            }
            ExprKind::ObjectLit(props) => self.compile_object_lit(props, line)?,
            ExprKind::Function(f) => {
                let name = f.name.clone().unwrap_or_default();
                let tpl = self.compile_function(f, &name, false)?;
                let c = self.add_const(Const::FunctionTemplate(Rc::new(tpl)));
                self.emit(OpCode::MakeClosure(c), line);
            }
            ExprKind::Arrow(f) => {
                let tpl = self.compile_function(f, "", false)?;
                let c = self.add_const(Const::FunctionTemplate(Rc::new(tpl)));
                self.emit(OpCode::MakeClosure(c), line);
            }
            ExprKind::Class(c) => self.compile_class_value(c, line)?,
            ExprKind::Unary { op, arg } => self.compile_unary(*op, arg, line)?,
            ExprKind::Update { op, arg, prefix } => self.compile_update(*op, arg, *prefix, line)?,
            ExprKind::Binary { op, left, right } => {
                self.compile_expr(left)?;
                self.compile_expr(right)?;
                self.emit(binary_opcode(*op), line);
            }
            ExprKind::Logical { op, left, right } => {
                self.compile_logical(*op, left, right, line)?
            }
            ExprKind::Assign { op, target, value } => {
                self.compile_assign(*op, target, value, line)?
            }
            ExprKind::Conditional { test, cons, alt } => {
                self.compile_expr(test)?;
                let jf = self.emit_jump(OpCode::JumpIfFalse, line);
                self.compile_expr(cons)?;
                let jend = self.emit_jump(OpCode::Jump, line);
                let alt_addr = self.cur_addr();
                self.patch_jump(jf, alt_addr);
                self.compile_expr(alt)?;
                let end = self.cur_addr();
                self.patch_jump(jend, end);
            }
            ExprKind::Call { .. } | ExprKind::Member { .. } => {
                let mut patches = Vec::new();
                self.compile_chain(e, &mut patches)?;
                let end = self.cur_addr();
                for p in patches {
                    self.patch_jump(p, end);
                }
            }
            ExprKind::New { callee, args } => self.compile_new(callee, args, line)?,
            ExprKind::Sequence(list) => {
                for (i, e2) in list.iter().enumerate() {
                    self.compile_expr(e2)?;
                    if i + 1 != list.len() {
                        self.emit(OpCode::Pop, line);
                    }
                }
            }
            ExprKind::Spread(_) => return Err(err(line, "スプレッドはここでは使えません")),
            ExprKind::Await(inner) => {
                self.compile_expr(inner)?;
                self.emit(OpCode::Await, line);
            }
            ExprKind::TsAs(inner) | ExprKind::TsNonNull(inner) => self.compile_expr(inner)?,
        }
        Ok(())
    }

    fn compile_template(&mut self, quasis: &[String], exprs: &[Expr], line: u32) -> CResult<()> {
        let c0 = self.add_const_str(&quasis[0]);
        self.emit(OpCode::LoadConst(c0), line);
        for (i, e) in exprs.iter().enumerate() {
            self.compile_expr(e)?;
            self.emit(OpCode::ToStringOp, line);
            self.emit(OpCode::Add, line);
            let cq = self.add_const_str(&quasis[i + 1]);
            self.emit(OpCode::LoadConst(cq), line);
            self.emit(OpCode::Add, line);
        }
        Ok(())
    }

    fn compile_object_lit(&mut self, props: &[ObjectProp], line: u32) -> CResult<()> {
        self.emit(OpCode::NewObject, line);
        for p in props {
            match p {
                ObjectProp::KeyValue { key, value, .. } => match key {
                    PropertyKey::Computed(ke) => {
                        self.compile_expr(ke)?;
                        self.compile_expr(value)?;
                        self.emit(OpCode::ObjectSetComputed, line);
                    }
                    _ => {
                        let name = property_key_static_name(key);
                        self.compile_expr(value)?;
                        let c = self.add_const_str(&name);
                        self.emit(OpCode::ObjectSetProp(c), line);
                    }
                },
                ObjectProp::Method {
                    key,
                    function,
                    kind,
                } => {
                    let name = property_key_static_name(key);
                    let tpl = self.compile_function(function, &name, true)?;
                    let c = self.add_const(Const::FunctionTemplate(Rc::new(tpl)));
                    self.emit(OpCode::MakeClosure(c), line);
                    let nc = self.add_const_str(&name);
                    match kind {
                        MethodKind::Normal => self.emit(OpCode::ObjectSetMethod(nc), line),
                        MethodKind::Get => self.emit(OpCode::ObjectDefineGetter(nc), line),
                        MethodKind::Set => self.emit(OpCode::ObjectDefineSetter(nc), line),
                    };
                }
                ObjectProp::Spread(e) => {
                    self.compile_expr(e)?;
                    self.emit(OpCode::ObjectSpread, line);
                }
            }
        }
        Ok(())
    }

    fn compile_unary(&mut self, op: UnaryOp, arg: &Expr, line: u32) -> CResult<()> {
        if op == UnaryOp::Delete {
            return self.compile_delete(arg, line);
        }
        if op == UnaryOp::Typeof {
            if let ExprKind::Ident(name) = &arg.kind {
                if matches!(self.resolve(name), VarLoc::Global) {
                    // 未宣言識別子への typeof は ReferenceError にせず "undefined" を返す（JS 仕様通り）。
                    let c = self.add_const_str(name);
                    self.emit(OpCode::TypeOfGlobal(c), line);
                    return Ok(());
                }
            }
        }
        self.compile_expr(arg)?;
        let op_code = match op {
            UnaryOp::Minus => OpCode::Neg,
            UnaryOp::Plus => OpCode::Pos,
            UnaryOp::Not => OpCode::Not,
            UnaryOp::BitNot => OpCode::BitNot,
            UnaryOp::Typeof => OpCode::TypeOf,
            UnaryOp::Void => {
                self.emit(OpCode::Pop, line);
                self.emit(OpCode::LoadUndefined, line);
                return Ok(());
            }
            UnaryOp::Delete => unreachable!(),
        };
        self.emit(op_code, line);
        Ok(())
    }

    fn compile_delete(&mut self, arg: &Expr, line: u32) -> CResult<()> {
        match &arg.kind {
            ExprKind::Member { obj, prop, .. } => {
                self.compile_expr(obj)?;
                match prop {
                    PropertyKey::Computed(ke) => {
                        self.compile_expr(ke)?;
                        self.emit(OpCode::DeleteIndex, line);
                    }
                    _ => {
                        let name = property_key_static_name(prop);
                        let c = self.add_const_str(&name);
                        self.emit(OpCode::DeleteProp(c), line);
                    }
                }
            }
            _ => {
                self.emit(OpCode::LoadTrue, line);
            }
        }
        Ok(())
    }

    fn temp_slot(&mut self) -> LocalIdx {
        self.declare_local("%tmp")
    }

    fn compile_update(&mut self, op: UpdateOp, arg: &Expr, prefix: bool, line: u32) -> CResult<()> {
        // x++ / x-- / ++x / --x を「読み出し→加減算→書き戻し」に展開する。
        // member の場合は obj/key を一時ローカルに保存してから GetXxx/SetXxx を
        // それぞれ 1 回ずつ行う（Dup/Swap だけでは 3 要素の並べ替えができないため）。
        let delta = match op {
            UpdateOp::Inc => 1.0,
            UpdateOp::Dec => -1.0,
        };
        match &arg.kind {
            ExprKind::Ident(name) => {
                self.emit_get_var(name, line);
                self.emit(OpCode::Pos, line); // ToNumber
                if !prefix {
                    self.emit(OpCode::Dup, line);
                }
                let c = self.const_num(delta);
                self.emit(OpCode::LoadConst(c), line);
                self.emit(OpCode::Add, line);
                self.emit_set_var(name, line);
                if !prefix {
                    self.emit(OpCode::Pop, line);
                }
            }
            ExprKind::Member { obj, prop, .. } => {
                let tmp_obj = self.temp_slot();
                self.compile_expr(obj)?;
                self.emit(OpCode::BindLocal(tmp_obj), line);
                let tmp_key = match prop {
                    PropertyKey::Computed(ke) => {
                        let t = self.temp_slot();
                        self.compile_expr(ke)?;
                        self.emit(OpCode::BindLocal(t), line);
                        Some(t)
                    }
                    _ => None,
                };
                let name = match prop {
                    PropertyKey::Computed(_) => None,
                    _ => Some(property_key_static_name(prop)),
                };
                // 現在値の読み出し。
                self.emit(OpCode::GetLocal(tmp_obj), line);
                if let Some(name) = &name {
                    let c = self.add_const_str(name);
                    self.emit(OpCode::GetProp(c), line);
                } else {
                    self.emit(OpCode::GetLocal(tmp_key.unwrap()), line);
                    self.emit(OpCode::GetIndex, line);
                }
                self.emit(OpCode::Pos, line);
                let tmp_old = if !prefix {
                    let t = self.temp_slot();
                    self.emit(OpCode::Dup, line);
                    self.emit(OpCode::BindLocal(t), line);
                    Some(t)
                } else {
                    None
                };
                let cnum = self.const_num(delta);
                self.emit(OpCode::LoadConst(cnum), line);
                self.emit(OpCode::Add, line);
                let tmp_new = self.temp_slot();
                self.emit(OpCode::BindLocal(tmp_new), line);
                self.emit(OpCode::GetLocal(tmp_obj), line);
                if let Some(name) = &name {
                    self.emit(OpCode::GetLocal(tmp_new), line);
                    let c = self.add_const_str(name);
                    self.emit(OpCode::SetProp(c), line);
                } else {
                    self.emit(OpCode::GetLocal(tmp_key.unwrap()), line);
                    self.emit(OpCode::GetLocal(tmp_new), line);
                    self.emit(OpCode::SetIndex, line);
                }
                if let Some(t) = tmp_old {
                    self.emit(OpCode::Pop, line);
                    self.emit(OpCode::GetLocal(t), line);
                }
            }
            _ => return Err(err(line, "++/-- の対象として使えない式です")),
        }
        Ok(())
    }

    fn compile_logical(
        &mut self,
        op: LogicalOp,
        left: &Expr,
        right: &Expr,
        line: u32,
    ) -> CResult<()> {
        self.compile_expr(left)?;
        let j = match op {
            LogicalOp::And => self.emit_jump(OpCode::JumpIfFalsyKeep, line),
            LogicalOp::Or => self.emit_jump(OpCode::JumpIfTruthyKeep, line),
            LogicalOp::Nullish => self.emit_jump(OpCode::JumpIfNotNullishKeep, line),
        };
        self.emit(OpCode::Pop, line);
        self.compile_expr(right)?;
        let end = self.cur_addr();
        self.patch_jump(j, end);
        Ok(())
    }

    fn compile_assign(
        &mut self,
        op: AssignOp,
        target: &Expr,
        value: &Expr,
        line: u32,
    ) -> CResult<()> {
        if op == AssignOp::Assign {
            if matches!(target.kind, ExprKind::ArrayLit(_) | ExprKind::ObjectLit(_)) {
                self.compile_expr(value)?;
                self.emit(OpCode::Dup, line);
                self.compile_assign_target_expr(target, line)?;
                return Ok(());
            }
            match &target.kind {
                ExprKind::Ident(name) => {
                    self.compile_expr(value)?;
                    self.emit_set_var(name, line);
                }
                ExprKind::Member { obj, prop, .. } => {
                    self.compile_expr(obj)?;
                    match prop {
                        PropertyKey::Computed(ke) => {
                            self.compile_expr(ke)?;
                            self.compile_expr(value)?;
                            self.emit(OpCode::SetIndex, line);
                        }
                        _ => {
                            self.compile_expr(value)?;
                            let name = property_key_static_name(prop);
                            let c = self.add_const_str(&name);
                            self.emit(OpCode::SetProp(c), line);
                        }
                    }
                }
                _ => return Err(err(line, "代入の左辺として使えない式です")),
            }
            return Ok(());
        }

        // 複合代入（算術・ビット演算・論理）。
        let (is_logical, plain_op) = assign_op_kind(op);
        match &target.kind {
            ExprKind::Ident(name) => {
                if is_logical {
                    self.emit_get_var(name, line);
                    let j = match op {
                        AssignOp::AndAssign => self.emit_jump(OpCode::JumpIfFalsyKeep, line),
                        AssignOp::OrAssign => self.emit_jump(OpCode::JumpIfTruthyKeep, line),
                        AssignOp::NullishAssign => {
                            self.emit_jump(OpCode::JumpIfNotNullishKeep, line)
                        }
                        _ => unreachable!(),
                    };
                    self.emit(OpCode::Pop, line);
                    self.compile_expr(value)?;
                    self.emit_set_var(name, line);
                    let end = self.cur_addr();
                    self.patch_jump(j, end);
                } else {
                    self.emit_get_var(name, line);
                    self.compile_expr(value)?;
                    self.emit(plain_op.unwrap(), line);
                    self.emit_set_var(name, line);
                }
            }
            ExprKind::Member { obj, prop, .. } => {
                if let PropertyKey::Computed(ke) = prop {
                    // `obj[expr] += v` 等: obj/key を一時ローカルへ保存して
                    // 二重評価を避ける（`compile_update` の Member 分岐と同じ
                    // 「新しい値も一時ローカルへ束縛してから正しい順序で
                    // 積み直す」パターンを使う）。
                    let tmp_obj = self.temp_slot();
                    self.compile_expr(obj)?;
                    self.emit(OpCode::BindLocal(tmp_obj), line);
                    let tmp_key = self.temp_slot();
                    self.compile_expr(ke)?;
                    self.emit(OpCode::BindLocal(tmp_key), line);
                    if is_logical {
                        self.emit(OpCode::GetLocal(tmp_obj), line);
                        self.emit(OpCode::GetLocal(tmp_key), line);
                        self.emit(OpCode::GetIndex, line);
                        let j = match op {
                            AssignOp::AndAssign => self.emit_jump(OpCode::JumpIfFalsyKeep, line),
                            AssignOp::OrAssign => self.emit_jump(OpCode::JumpIfTruthyKeep, line),
                            AssignOp::NullishAssign => {
                                self.emit_jump(OpCode::JumpIfNotNullishKeep, line)
                            }
                            _ => unreachable!(),
                        };
                        self.emit(OpCode::Pop, line);
                        self.compile_expr(value)?;
                        let tmp_new = self.temp_slot();
                        self.emit(OpCode::BindLocal(tmp_new), line);
                        self.emit(OpCode::GetLocal(tmp_obj), line);
                        self.emit(OpCode::GetLocal(tmp_key), line);
                        self.emit(OpCode::GetLocal(tmp_new), line);
                        self.emit(OpCode::SetIndex, line);
                        let end = self.cur_addr();
                        self.patch_jump(j, end);
                    } else {
                        self.emit(OpCode::GetLocal(tmp_obj), line);
                        self.emit(OpCode::GetLocal(tmp_key), line);
                        self.emit(OpCode::GetIndex, line);
                        self.compile_expr(value)?;
                        self.emit(plain_op.unwrap(), line);
                        let tmp_new = self.temp_slot();
                        self.emit(OpCode::BindLocal(tmp_new), line);
                        self.emit(OpCode::GetLocal(tmp_obj), line);
                        self.emit(OpCode::GetLocal(tmp_key), line);
                        self.emit(OpCode::GetLocal(tmp_new), line);
                        self.emit(OpCode::SetIndex, line);
                    }
                    return Ok(());
                }
                let name = property_key_static_name(prop);
                let tmp_obj = self.temp_slot();
                self.compile_expr(obj)?;
                self.emit(OpCode::BindLocal(tmp_obj), line);
                if is_logical {
                    self.emit(OpCode::GetLocal(tmp_obj), line);
                    let c = self.add_const_str(&name);
                    self.emit(OpCode::GetProp(c), line);
                    let j = match op {
                        AssignOp::AndAssign => self.emit_jump(OpCode::JumpIfFalsyKeep, line),
                        AssignOp::OrAssign => self.emit_jump(OpCode::JumpIfTruthyKeep, line),
                        AssignOp::NullishAssign => {
                            self.emit_jump(OpCode::JumpIfNotNullishKeep, line)
                        }
                        _ => unreachable!(),
                    };
                    self.emit(OpCode::Pop, line);
                    self.emit(OpCode::GetLocal(tmp_obj), line);
                    self.compile_expr(value)?;
                    let c2 = self.add_const_str(&name);
                    self.emit(OpCode::SetProp(c2), line);
                    let end = self.cur_addr();
                    self.patch_jump(j, end);
                } else {
                    self.emit(OpCode::GetLocal(tmp_obj), line);
                    let c = self.add_const_str(&name);
                    self.emit(OpCode::GetProp(c), line);
                    self.compile_expr(value)?;
                    self.emit(plain_op.unwrap(), line);
                    self.emit(OpCode::GetLocal(tmp_obj), line);
                    self.emit(OpCode::Swap, line);
                    let c2 = self.add_const_str(&name);
                    self.emit(OpCode::SetProp(c2), line);
                }
            }
            _ => return Err(err(line, "代入の左辺として使えない式です")),
        }
        Ok(())
    }

    /// Member/Call の連鎖（オプショナルチェーン込み）をコンパイルする。
    /// `patches` に集めたジャンプは、呼び出し元がチェーン全体の終端で
    /// まとめてパッチする。
    fn compile_chain(&mut self, e: &Expr, patches: &mut Vec<usize>) -> CResult<()> {
        let line = e.span.start.line;
        match &e.kind {
            ExprKind::Member {
                obj,
                prop,
                optional,
            } => {
                self.compile_chain_or_base(obj, patches)?;
                if *optional {
                    let j = self.emit_jump(OpCode::JumpIfNullish, line);
                    patches.push(j);
                }
                match prop {
                    PropertyKey::Computed(ke) => {
                        self.compile_expr(ke)?;
                        self.emit(OpCode::GetIndex, line);
                    }
                    _ => {
                        let name = property_key_static_name(prop);
                        let c = self.add_const_str(&name);
                        self.emit(OpCode::GetProp(c), line);
                    }
                }
            }
            ExprKind::Call {
                callee,
                args,
                optional,
            } => {
                let is_chain_callee =
                    matches!(callee.kind, ExprKind::Member { .. } | ExprKind::Call { .. });
                if let ExprKind::Member {
                    obj,
                    prop,
                    optional: mopt,
                } = &callee.kind
                {
                    self.compile_chain_or_base(obj, patches)?;
                    if *mopt {
                        let j = self.emit_jump(OpCode::JumpIfNullish, line);
                        patches.push(j);
                    }
                    self.emit(OpCode::Dup, line);
                    match prop {
                        PropertyKey::Computed(ke) => {
                            self.compile_expr(ke)?;
                            self.emit(OpCode::GetIndex, line);
                        }
                        _ => {
                            let name = property_key_static_name(prop);
                            let c = self.add_const_str(&name);
                            self.emit(OpCode::GetProp(c), line);
                        }
                    }
                } else if is_chain_callee {
                    self.emit(OpCode::LoadUndefined, line);
                    self.compile_chain_or_base(callee, patches)?;
                } else {
                    self.emit(OpCode::LoadUndefined, line);
                    self.compile_expr(callee)?;
                }
                if *optional {
                    let j = self.emit_jump(OpCode::JumpIfNullishCall, line);
                    patches.push(j);
                }
                self.compile_call_args(args, line)?;
            }
            _ => self.compile_expr(e)?,
        }
        Ok(())
    }

    fn compile_chain_or_base(&mut self, e: &Expr, patches: &mut Vec<usize>) -> CResult<()> {
        if matches!(e.kind, ExprKind::Member { .. } | ExprKind::Call { .. }) {
            self.compile_chain(e, patches)
        } else {
            self.compile_expr(e)
        }
    }

    fn compile_call_args(&mut self, args: &[Expr], line: u32) -> CResult<()> {
        if args.iter().any(|a| matches!(a.kind, ExprKind::Spread(_))) {
            self.emit(OpCode::NewArray, line);
            for a in args {
                if let ExprKind::Spread(inner) = &a.kind {
                    self.compile_expr(inner)?;
                    self.emit(OpCode::SpreadAppendArray, line);
                } else {
                    self.compile_expr(a)?;
                    self.emit(OpCode::ArrayPush, line);
                }
            }
            self.emit(OpCode::CallSpread, line);
        } else {
            for a in args {
                self.compile_expr(a)?;
            }
            self.emit(OpCode::Call(args.len() as u16), line);
        }
        Ok(())
    }

    fn compile_new(&mut self, callee: &Expr, args: &[Expr], line: u32) -> CResult<()> {
        self.compile_expr(callee)?;
        if args.iter().any(|a| matches!(a.kind, ExprKind::Spread(_))) {
            self.emit(OpCode::NewArray, line);
            for a in args {
                if let ExprKind::Spread(inner) = &a.kind {
                    self.compile_expr(inner)?;
                    self.emit(OpCode::SpreadAppendArray, line);
                } else {
                    self.compile_expr(a)?;
                    self.emit(OpCode::ArrayPush, line);
                }
            }
            self.emit(OpCode::NewSpread, line);
        } else {
            for a in args {
                self.compile_expr(a)?;
            }
            self.emit(OpCode::New(args.len() as u16), line);
        }
        Ok(())
    }
}

fn assign_op_kind(op: AssignOp) -> (bool, Option<OpCode>) {
    match op {
        AssignOp::AddAssign => (false, Some(OpCode::Add)),
        AssignOp::SubAssign => (false, Some(OpCode::Sub)),
        AssignOp::MulAssign => (false, Some(OpCode::Mul)),
        AssignOp::DivAssign => (false, Some(OpCode::Div)),
        AssignOp::ModAssign => (false, Some(OpCode::Mod)),
        AssignOp::PowAssign => (false, Some(OpCode::Pow)),
        AssignOp::ShlAssign => (false, Some(OpCode::Shl)),
        AssignOp::ShrAssign => (false, Some(OpCode::Shr)),
        AssignOp::UShrAssign => (false, Some(OpCode::UShr)),
        AssignOp::BitAndAssign => (false, Some(OpCode::BitAnd)),
        AssignOp::BitOrAssign => (false, Some(OpCode::BitOr)),
        AssignOp::BitXorAssign => (false, Some(OpCode::BitXor)),
        AssignOp::AndAssign | AssignOp::OrAssign | AssignOp::NullishAssign => (true, None),
        AssignOp::Assign => unreachable!(),
    }
}

fn binary_opcode(op: BinaryOp) -> OpCode {
    match op {
        BinaryOp::Add => OpCode::Add,
        BinaryOp::Sub => OpCode::Sub,
        BinaryOp::Mul => OpCode::Mul,
        BinaryOp::Div => OpCode::Div,
        BinaryOp::Mod => OpCode::Mod,
        BinaryOp::Pow => OpCode::Pow,
        BinaryOp::Eq => OpCode::Eq,
        BinaryOp::NotEq => OpCode::NotEq,
        BinaryOp::StrictEq => OpCode::StrictEq,
        BinaryOp::StrictNotEq => OpCode::StrictNotEq,
        BinaryOp::Lt => OpCode::Lt,
        BinaryOp::Gt => OpCode::Gt,
        BinaryOp::LtEq => OpCode::LtEq,
        BinaryOp::GtEq => OpCode::GtEq,
        BinaryOp::Shl => OpCode::Shl,
        BinaryOp::Shr => OpCode::Shr,
        BinaryOp::UShr => OpCode::UShr,
        BinaryOp::BitAnd => OpCode::BitAnd,
        BinaryOp::BitOr => OpCode::BitOr,
        BinaryOp::BitXor => OpCode::BitXor,
        BinaryOp::Instanceof => OpCode::Instanceof,
        BinaryOp::In => OpCode::In,
    }
}

fn property_key_static_name(key: &PropertyKey) -> String {
    match key {
        PropertyKey::Ident(s) => s.clone(),
        PropertyKey::Str(s) => s.clone(),
        PropertyKey::Num(n) => crate::vm::number_to_js_string(*n),
        PropertyKey::Computed(_) => panic!("computed key を静的名として扱おうとしました"),
    }
}

fn pattern_names(pat: &Pattern) -> Vec<String> {
    let mut out = Vec::new();
    collect_pattern_names(pat, &mut out);
    out
}

fn collect_pattern_names(pat: &Pattern, out: &mut Vec<String>) {
    match pat {
        Pattern::Ident(n) => out.push(n.clone()),
        Pattern::Assign { left, .. } => collect_pattern_names(left, out),
        Pattern::Object { props, rest } => {
            for p in props {
                collect_pattern_names(&p.value, out);
            }
            if let Some(r) = rest {
                collect_pattern_names(r, out);
            }
        }
        Pattern::Array { elements, rest } => {
            for el in elements.iter().flatten() {
                collect_pattern_names(el, out);
            }
            if let Some(r) = rest {
                collect_pattern_names(r, out);
            }
        }
    }
}

// ast モジュールの再エクスポート（compile_dynamic_function 内の match で使用）。
#[allow(unused_imports)]
use ast::StmtKind as _AstStmtKindReexport;
