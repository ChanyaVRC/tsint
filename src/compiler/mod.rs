//! AST → バイトコードのコンパイラ。
//!
//! ## サブモジュール構成
//! - `stmt`: ブロックスコープ・巻き上げ・文・ループ・switch・try/catch/finally
//! - `pattern`: 分割代入（宣言/代入先）・関数・クラスのコンパイル
//! - `expr`: 式・テンプレート・演算子・代入・チェーン呼び出し・純粋性判定

mod expr;
mod pattern;
mod stmt;

use std::rc::Rc;

use crate::ast::{ExportSpecifier, Module, Stmt, StmtKind};
use crate::bytecode::*;
use crate::hash::FastHashMap;

use pattern::pattern_names;

/// 定数重複排除用のハッシュマップを遅延確保する閾値。
const CONST_DEDUP_THRESHOLD: usize = 16;

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

pub(super) type CResult<T> = Result<T, CompileError>;

pub(super) fn err(line: u32, msg: impl Into<String>) -> CompileError {
    CompileError {
        message: msg.into(),
        line,
    }
}

// =====================================================================
// スコープ内部表現
// =====================================================================

pub(super) struct LocalVar {
    pub(super) name: Rc<str>,
    pub(super) slot: LocalIdx,
}

/// break/continue/return の経路解決に使うコントロールスタックの要素。
pub(super) enum Ctrl {
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

pub(super) struct FuncCtx {
    pub(super) chunk: Chunk,
    pub(super) locals: Vec<LocalVar>,
    pub(super) next_slot: LocalIdx,
    pub(super) upvalues: Vec<UpvalueDesc>,
    pub(super) upvalue_names: Vec<Rc<str>>,
    pub(super) control: Vec<Ctrl>,
    /// 例外の再送出に使う一時ローカルスロット（finally を含む関数のみ確保）。
    exc_slot: Option<LocalIdx>,
    /// 定数表の重複排除キャッシュ（定数が多い関数でのみ遅延確保）。
    #[allow(clippy::box_collection)]
    str_consts: Option<Box<FastHashMap<Rc<str>, ConstIdx>>>,
    #[allow(clippy::box_collection)]
    num_consts: Option<Box<FastHashMap<u64, ConstIdx>>>,
}

impl FuncCtx {
    pub(super) fn new(name: impl Into<Rc<str>>) -> Self {
        FuncCtx {
            chunk: Chunk::new(name),
            locals: Vec::new(),
            next_slot: 0,
            upvalues: Vec::new(),
            upvalue_names: Vec::new(),
            control: vec![Ctrl::FuncBoundary],
            exc_slot: None,
            str_consts: None,
            num_consts: None,
        }
    }

    pub(super) fn alloc_slot(&mut self) -> LocalIdx {
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

    pub(super) fn find_local(&self, name: &str) -> Option<LocalIdx> {
        self.locals
            .iter()
            .rev()
            .find(|l| &*l.name == name)
            .map(|l| l.slot)
    }

    pub(super) fn get_or_make_exc_slot(&mut self) -> LocalIdx {
        if let Some(s) = self.exc_slot {
            return s;
        }
        let s = self.alloc_slot();
        self.exc_slot = Some(s);
        s
    }
}

pub(super) enum VarLoc {
    Local(LocalIdx),
    Upvalue(UpvalueIdx),
    Global,
}

pub struct Compiler {
    pub(super) funcs: Vec<FuncCtx>,
}

// =====================================================================
// エントリポイント
// =====================================================================

/// モジュール全体をトップレベル「関数」としてコンパイルする。
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

    for s in &module.body {
        collect_exports(&mut c, s)?;
    }

    c.emit(OpCode::LoadUndefined, 0);
    c.emit(OpCode::Return, 0);

    let mut f = c.funcs.pop().unwrap();
    f.chunk.finalize();
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
            crate::ast::ExprKind::Function(f) => f.as_ref(),
            _ => return Err(err(0, "new Function: 内部エラー（関数式ではありません）")),
        },
        _ => return Err(err(0, "new Function: 内部エラー（文の形が想定外です）")),
    };

    let mut c = Compiler { funcs: Vec::new() };
    let template = c.compile_function(func_expr, "anonymous", false)?;
    Ok(template.chunk.clone())
}

// =====================================================================
// コア出力・定数・ジャンプ・変数解決ヘルパー
// =====================================================================

impl Compiler {
    pub(super) fn cur(&mut self) -> &mut FuncCtx {
        self.funcs.last_mut().unwrap()
    }

    pub(super) fn cur_addr(&mut self) -> u32 {
        self.cur().chunk.code.len() as u32
    }

    pub(super) fn emit(&mut self, op: OpCode, line: u32) -> usize {
        let f = self.cur();
        f.chunk.code.push(op);
        f.chunk.lines.push(line);
        f.chunk.code.len() - 1
    }

    pub(super) fn emit_jump(&mut self, mk: impl Fn(JumpAddr) -> OpCode, line: u32) -> usize {
        self.emit(mk(0), line)
    }

    pub(super) fn add_const(&mut self, c: Const) -> ConstIdx {
        let f = self.cur();
        f.chunk.consts.push(c);
        (f.chunk.consts.len() - 1) as ConstIdx
    }

    pub(super) fn add_const_str(&mut self, s: &str) -> ConstIdx {
        let f = self.cur();
        if let Some(ref mut map) = f.str_consts {
            if let Some(&idx) = map.get(s) {
                return idx;
            }
            let rc: Rc<str> = Rc::from(s);
            let idx = f.chunk.consts.len() as ConstIdx;
            f.chunk.consts.push(Const::Str(rc.clone()));
            map.insert(rc, idx);
            return idx;
        }
        for (i, c) in f.chunk.consts.iter().enumerate() {
            if let Const::Str(existing) = c {
                if &**existing == s {
                    return i as ConstIdx;
                }
            }
        }
        let rc: Rc<str> = Rc::from(s);
        let idx = f.chunk.consts.len() as ConstIdx;
        f.chunk.consts.push(Const::Str(rc));
        if f.chunk.consts.len() > CONST_DEDUP_THRESHOLD {
            let mut map =
                FastHashMap::with_capacity_and_hasher(f.chunk.consts.len() * 2, Default::default());
            for (i, c) in f.chunk.consts.iter().enumerate() {
                if let Const::Str(existing) = c {
                    map.insert(existing.clone(), i as ConstIdx);
                }
            }
            f.str_consts = Some(Box::new(map));
        }
        idx
    }

    pub(super) fn const_num(&mut self, n: f64) -> ConstIdx {
        let bits = n.to_bits();
        let f = self.cur();
        if let Some(ref mut map) = f.num_consts {
            if let Some(&idx) = map.get(&bits) {
                return idx;
            }
            let idx = f.chunk.consts.len() as ConstIdx;
            f.chunk.consts.push(Const::Number(n));
            map.insert(bits, idx);
            return idx;
        }
        for (i, c) in f.chunk.consts.iter().enumerate() {
            if let Const::Number(existing) = c {
                if existing.to_bits() == bits {
                    return i as ConstIdx;
                }
            }
        }
        let idx = f.chunk.consts.len() as ConstIdx;
        f.chunk.consts.push(Const::Number(n));
        if f.chunk.consts.len() > CONST_DEDUP_THRESHOLD {
            let mut map =
                FastHashMap::with_capacity_and_hasher(f.chunk.consts.len() * 2, Default::default());
            for (i, c) in f.chunk.consts.iter().enumerate() {
                if let Const::Number(existing) = c {
                    map.insert(existing.to_bits(), i as ConstIdx);
                }
            }
            f.num_consts = Some(Box::new(map));
        }
        idx
    }

    pub(super) fn patch_jump(&mut self, idx: usize, target: u32) {
        let op = &mut self.cur().chunk.code[idx];
        *op = match *op {
            OpCode::Jump(_) => OpCode::Jump(target),
            OpCode::JumpIfFalse(_) => OpCode::JumpIfFalse(target),
            OpCode::JumpIfTrue(_) => OpCode::JumpIfTrue(target),
            OpCode::LtJumpIfFalse(_) => OpCode::LtJumpIfFalse(target),
            OpCode::LtEqJumpIfFalse(_) => OpCode::LtEqJumpIfFalse(target),
            OpCode::GtJumpIfFalse(_) => OpCode::GtJumpIfFalse(target),
            OpCode::GtEqJumpIfFalse(_) => OpCode::GtEqJumpIfFalse(target),
            OpCode::StrictEqJumpIfFalse(_) => OpCode::StrictEqJumpIfFalse(target),
            OpCode::StrictNotEqJumpIfFalse(_) => OpCode::StrictNotEqJumpIfFalse(target),
            OpCode::JumpIfNullish(_) => OpCode::JumpIfNullish(target),
            OpCode::JumpIfNullishCall(_) => OpCode::JumpIfNullishCall(target),
            OpCode::JumpIfFalsyKeep(_) => OpCode::JumpIfFalsyKeep(target),
            OpCode::JumpIfTruthyKeep(_) => OpCode::JumpIfTruthyKeep(target),
            OpCode::JumpIfNotNullishKeep(_) => OpCode::JumpIfNotNullishKeep(target),
            OpCode::JumpIfNotUndefinedKeep(_) => OpCode::JumpIfNotUndefinedKeep(target),
            other => panic!("patch_jump: ジャンプ命令ではありません: {other:?}"),
        };
    }

    pub(super) fn declare_local(&mut self, name: &str) -> LocalIdx {
        let slot = self.cur().alloc_slot();
        self.cur().locals.push(LocalVar {
            name: Rc::from(name),
            slot,
        });
        slot
    }

    pub(super) fn temp_slot(&mut self) -> LocalIdx {
        self.declare_local("%tmp")
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

    pub(super) fn resolve(&mut self, name: &str) -> VarLoc {
        let cur = self.funcs.len() - 1;
        if let Some(slot) = self.funcs[cur].find_local(name) {
            return VarLoc::Local(slot);
        }
        if let Some(uv) = self.resolve_upvalue(cur, name) {
            return VarLoc::Upvalue(uv);
        }
        VarLoc::Global
    }

    pub(super) fn emit_get_var(&mut self, name: &str, line: u32) {
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

    pub(super) fn emit_set_var(&mut self, name: &str, line: u32) {
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

    pub(super) fn emit_bind_var(&mut self, name: &str, line: u32) {
        match self.resolve(name) {
            VarLoc::Local(s) => {
                self.emit(OpCode::BindLocal(s), line);
            }
            VarLoc::Upvalue(_) | VarLoc::Global => {
                let c = self.add_const_str(name);
                self.emit(OpCode::BindGlobal(c), line);
            }
        }
    }
}
