//! 文・ブロックスコープ・巻き上げ・制御フロー（ループ / switch / try-catch-finally）のコンパイル。

use std::rc::Rc;

use crate::ast::{self, *};
use crate::bytecode::*;

use super::expr::expr_contains_closure;
use super::pattern::pattern_names;
use super::{err, CResult, Compiler, Ctrl};

#[derive(Clone, Copy, PartialEq, Eq)]
enum JumpControl {
    Break,
    Continue,
}

impl Compiler {
    /// ブロック（文リスト）を新しいレキシカルスコープでコンパイルする。
    pub(super) fn compile_block_new_scope(&mut self, stmts: &[Stmt]) -> CResult<()> {
        let saved_len = self.cur().locals.len();
        let saved_next_slot = self.cur().next_slot;
        self.hoist_and_compile(stmts)?;
        self.restore_scope(saved_len, saved_next_slot);
        Ok(())
    }

    /// モジュール/関数のトップレベル文列（`var` 宣言の関数/モジュールスコープ巻き上げ込み）。
    pub(super) fn compile_top_level_stmts(&mut self, stmts: &[Stmt]) -> CResult<()> {
        self.hoist_function_vars(stmts)?;
        self.hoist_and_compile(stmts)
    }

    /// 関数・モジュールスコープ内のすべての `var` 宣言を巻き上げ、`undefined` で初期化する（T-23）。
    pub(super) fn hoist_function_vars(&mut self, stmts: &[Stmt]) -> CResult<()> {
        for s in stmts {
            self.hoist_vars_in_stmt(s)?;
        }
        Ok(())
    }

    fn hoist_var_decls(&mut self, decls: &[VarDeclarator], line: u32) -> CResult<()> {
        for d in decls {
            for name in pattern_names(&d.name) {
                if self.cur().find_local(name).is_none() {
                    let slot = self.declare_local(name, line)?;
                    self.emit(OpCode::LoadUndefined, line);
                    self.emit(OpCode::BindLocal(slot), line);
                }
            }
        }
        Ok(())
    }

    fn hoist_vars_in_stmt(&mut self, s: &Stmt) -> CResult<()> {
        let line = s.span.start.line;
        self.enter_depth(line)?;
        let res = (|| -> CResult<()> {
            match &s.kind {
                StmtKind::VarDecl {
                    kind: VarKind::Var,
                    decls,
                } => self.hoist_var_decls(decls, line)?,
                StmtKind::Block(stmts) | StmtKind::TsModule { body: stmts, .. } => {
                    for st in stmts {
                        self.hoist_vars_in_stmt(st)?;
                    }
                }
                StmtKind::If { cons, alt, .. } => {
                    self.hoist_vars_in_stmt(cons)?;
                    if let Some(a) = alt {
                        self.hoist_vars_in_stmt(a)?;
                    }
                }
                StmtKind::While { body, .. }
                | StmtKind::DoWhile { body, .. }
                | StmtKind::Labeled { body, .. } => self.hoist_vars_in_stmt(body)?,
                StmtKind::For { init, body, .. } => {
                    if let Some(init) = init {
                        if let ForInit::VarDecl(VarKind::Var, decls) = init.as_ref() {
                            self.hoist_var_decls(decls, line)?;
                        }
                    }
                    self.hoist_vars_in_stmt(body)?;
                }
                StmtKind::ForIn { left, body, .. } => {
                    if let ForInit::VarDecl(VarKind::Var, decls) = left.as_ref() {
                        self.hoist_var_decls(decls, line)?;
                    }
                    self.hoist_vars_in_stmt(body)?;
                }
                StmtKind::Switch { cases, .. } => {
                    for c in cases {
                        for st in &c.body {
                            self.hoist_vars_in_stmt(st)?;
                        }
                    }
                }
                StmtKind::Try {
                    block,
                    handler,
                    finalizer,
                } => {
                    for st in block {
                        self.hoist_vars_in_stmt(st)?;
                    }
                    if let Some(h) = handler {
                        for st in &h.body {
                            self.hoist_vars_in_stmt(st)?;
                        }
                    }
                    if let Some(fin) = finalizer {
                        for st in fin {
                            self.hoist_vars_in_stmt(st)?;
                        }
                    }
                }
                StmtKind::ExportDecl(inner) => self.hoist_vars_in_stmt(inner)?,
                _ => {}
            }
            Ok(())
        })();
        self.leave_depth();
        res
    }

    /// 文リストの巻き上げ（1st: 名前予約, 2nd: ループ内 FreshLocal, 3rd: 関数宣言初期化, 4th: 文コンパイル）。
    pub(super) fn hoist_and_compile(&mut self, stmts: &[Stmt]) -> CResult<()> {
        let mut hoisted_slots: Vec<(LocalIdx, u32)> = Vec::new();
        for s in stmts {
            self.hoist_stmt_names(s, &mut hoisted_slots)?;
        }
        // ループ本体（およびループ内の入れ子ブロック）で宣言されるレキシカル変数は
        // 反復ごとに新しいセル＋TDZ へリセットする（T-2）。
        if self.cur().loop_depth > 0 {
            for &(slot, line) in &hoisted_slots {
                self.emit(OpCode::FreshLocal(slot), line);
            }
        }
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
        for s in stmts {
            self.compile_stmt(s)?;
        }
        Ok(())
    }

    fn hoist_stmt_names(
        &mut self,
        s: &Stmt,
        hoisted_slots: &mut Vec<(LocalIdx, u32)>,
    ) -> CResult<()> {
        let line = s.span.start.line;
        match &s.kind {
            StmtKind::VarDecl { kind, decls } => {
                // `var` は関数/モジュールスコープへ巻き上げ済み（T-23）。
                if *kind != VarKind::Var {
                    for d in decls {
                        for name in pattern_names(&d.name) {
                            let slot = self.declare_local(name, line)?;
                            hoisted_slots.push((slot, line));
                        }
                    }
                }
            }
            StmtKind::FunctionDecl(f) => {
                if let Some(name) = &f.name {
                    let slot = self.declare_local(name, line)?;
                    hoisted_slots.push((slot, line));
                }
            }
            StmtKind::ClassDecl(c) => {
                if let Some(name) = &c.name {
                    let slot = self.declare_local(name, line)?;
                    hoisted_slots.push((slot, line));
                }
            }
            StmtKind::ExportDecl(inner) => self.hoist_stmt_names(inner, hoisted_slots)?,
            StmtKind::ExportDefault(_) => {
                let slot = self.declare_local("default", line)?;
                hoisted_slots.push((slot, line));
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) fn compile_stmt(&mut self, s: &Stmt) -> CResult<()> {
        let line = s.span.start.line;
        self.enter_depth(line)?;
        let res = self.compile_stmt_inner(s, line);
        self.leave_depth();
        res
    }

    fn compile_stmt_inner(&mut self, s: &Stmt, line: u32) -> CResult<()> {
        match &s.kind {
            StmtKind::Empty | StmtKind::Debugger => {}
            StmtKind::Expr(e) => {
                self.compile_expr_discard(e)?;
            }
            StmtKind::Block(b) => self.compile_block_new_scope(b)?,
            StmtKind::VarDecl { kind, decls } => {
                for d in decls {
                    if let Some(init) = &d.init {
                        self.compile_expr(init)?;
                        self.compile_bind_pattern(&d.name, line)?;
                    } else if *kind != VarKind::Var {
                        self.emit(OpCode::LoadUndefined, line);
                        self.compile_bind_pattern(&d.name, line)?;
                    }
                }
            }
            StmtKind::FunctionDecl(_) => {}
            StmtKind::ClassDecl(c) => {
                self.compile_class_value(c, line)?;
                if let Some(name) = &c.name {
                    self.emit_bind_var(name, line);
                }
            }
            StmtKind::If { test, cons, alt } => {
                let jf = self.compile_cond_jump_false(test, line)?;
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
            StmtKind::Switch { disc, cases } => self.compile_switch(disc, cases, line)?,
            StmtKind::Try {
                block,
                handler,
                finalizer,
            } => self.compile_try(block, handler.as_deref(), finalizer.as_deref(), line)?,
            StmtKind::Throw(e) => {
                self.compile_expr(e)?;
                self.emit(OpCode::Throw, line);
            }
            StmtKind::Return(e) => self.compile_return(e.as_deref(), line)?,
            StmtKind::Break(label) => {
                self.compile_jump_control(JumpControl::Break, label.as_deref(), line)?
            }
            StmtKind::Continue(label) => {
                self.compile_jump_control(JumpControl::Continue, label.as_deref(), line)?
            }
            StmtKind::Labeled { label, body } => self.compile_labeled(label, body, line)?,
            StmtKind::Import(_) => return Err(err(line, "import 文は未対応です")),
            StmtKind::ExportNamed { .. } | StmtKind::ExportAll { .. } => {}
            StmtKind::ExportDefault(d) => {
                match d.as_ref() {
                    DefaultExportable::Expr(e) => self.compile_expr(e)?,
                    DefaultExportable::Function(f) => {
                        let name = f.name.as_deref().unwrap_or("");
                        let tpl = self.compile_function(f, name)?;
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
        let name = f.name.as_deref().unwrap_or("");
        let template = self.compile_function(f, name)?;
        let cidx = self.add_const(Const::FunctionTemplate(Rc::new(template)));
        self.emit(OpCode::MakeClosure(cidx), line);
        self.emit_bind_var(name, line);
        Ok(())
    }

    // ---------------- ループ ----------------

    fn compile_while(&mut self, label: Option<Rc<str>>, test: &Expr, body: &Stmt) -> CResult<()> {
        let line = test.span.start.line;
        let test_addr = self.cur_addr();
        let jf = self.compile_cond_jump_false(test, line)?;
        self.cur().control.push(Ctrl::Loop {
            label,
            break_patches: Vec::new(),
            continue_patches: Vec::new(),
        });
        self.cur().loop_depth += 1;
        let body_res = self.compile_stmt(body);
        self.cur().loop_depth -= 1;
        body_res?;
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
        self.cur().loop_depth += 1;
        let body_res = self.compile_stmt(body);
        self.cur().loop_depth -= 1;
        body_res?;
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
        let saved_next_slot = self.cur().next_slot;
        let in_outer_loop = self.cur().loop_depth > 0;
        let mut per_iter_slots: Vec<LocalIdx> = Vec::new();
        if let Some(init) = init {
            match init {
                ForInit::VarDecl(kind, decls) => {
                    for d in decls {
                        if *kind != ast::VarKind::Var {
                            for name in pattern_names(&d.name) {
                                let slot = self.declare_local(name, line)?;
                                if in_outer_loop {
                                    self.emit(OpCode::FreshLocal(slot), line);
                                }
                                per_iter_slots.push(slot);
                            }
                        }
                        if let Some(e) = &d.init {
                            self.compile_expr(e)?;
                            self.compile_bind_pattern(&d.name, line)?;
                        } else if *kind != ast::VarKind::Var {
                            self.emit(OpCode::LoadUndefined, line);
                            self.compile_bind_pattern(&d.name, line)?;
                        }
                    }
                }
                ForInit::Expr(e) => {
                    self.compile_expr_discard(e)?;
                }
            }
        }
        for &slot in &per_iter_slots {
            self.emit(OpCode::RebindLocal(slot), line);
        }
        let test_addr = self.cur_addr();
        let jf = if let Some(test) = test {
            Some(self.compile_cond_jump_false(test, line)?)
        } else {
            None
        };
        self.cur().control.push(Ctrl::Loop {
            label,
            break_patches: Vec::new(),
            continue_patches: Vec::new(),
        });
        self.cur().loop_depth += 1;
        let body_res = self.compile_stmt(body);
        self.cur().loop_depth -= 1;
        body_res?;
        let cont_addr = self.cur_addr();
        self.resolve_loop_continue(cont_addr);
        let update_has_closure = update.is_some_and(expr_contains_closure);
        for &slot in &per_iter_slots {
            if self.cur().chunk.captured_locals[slot as usize] || update_has_closure {
                self.emit(OpCode::RebindLocal(slot), line);
            }
        }
        if let Some(u) = update {
            self.compile_expr_discard(u)?;
        }
        self.emit(OpCode::Jump(test_addr), line);
        let end = self.cur_addr();
        if let Some(jf) = jf {
            self.patch_jump(jf, end);
        }
        self.pop_loop_breaks(end);
        self.restore_scope(saved_len, saved_next_slot);
        Ok(())
    }

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
        let saved_next_slot = self.cur().next_slot;
        self.compile_expr(right)?;
        if is_of {
            self.emit(OpCode::MakeIterArray, line);
        } else {
            self.emit(OpCode::ObjectKeysArray, line);
        }
        let arr_slot = self.declare_local("%iter_arr", line)?;
        self.emit(OpCode::BindLocal(arr_slot), line);
        self.emit(OpCode::LoadSmallInt(0), line);
        let idx_slot = self.declare_local("%iter_idx", line)?;
        self.emit(OpCode::BindLocal(idx_slot), line);

        let test_addr = self.cur_addr();
        self.emit(OpCode::GetLocal(idx_slot), line);
        self.emit(OpCode::GetLocal(arr_slot), line);
        let len_c = self.add_const_str("length");
        self.emit(OpCode::GetProp(len_c), line);
        let jf = self.emit_jump(OpCode::LtJumpIfFalse, line);

        self.cur().control.push(Ctrl::Loop {
            label,
            break_patches: Vec::new(),
            continue_patches: Vec::new(),
        });
        let body_saved_len = self.cur().locals.len();
        let body_saved_next_slot = self.cur().next_slot;
        self.emit(OpCode::GetLocal(arr_slot), line);
        self.emit(OpCode::GetLocal(idx_slot), line);
        self.emit(OpCode::GetIndex, line);
        match left {
            ForInit::VarDecl(kind, decls) => {
                let pat = &decls[0].name;
                if *kind != ast::VarKind::Var {
                    for name in pattern_names(pat) {
                        let slot = self.declare_local(name, line)?;
                        // 反復ごとに新しいセル＋TDZ にリセット（T-2）。
                        self.emit(OpCode::FreshLocal(slot), line);
                    }
                }
                self.compile_bind_pattern(pat, line)?;
            }
            ForInit::Expr(e) => self.compile_assign_target_expr(e, line)?,
        }
        self.cur().loop_depth += 1;
        let body_res = self.compile_stmt(body);
        self.cur().loop_depth -= 1;
        body_res?;
        self.restore_scope(body_saved_len, body_saved_next_slot);
        let cont_addr = self.cur_addr();
        self.resolve_loop_continue(cont_addr);
        self.emit(OpCode::IncLocal(idx_slot), line);
        self.emit(OpCode::Jump(test_addr), line);

        let end = self.cur_addr();
        self.patch_jump(jf, end);
        self.pop_loop_breaks(end);
        self.restore_scope(saved_len, saved_next_slot);
        Ok(())
    }

    fn resolve_loop_continue(&mut self, addr: u32) {
        if let Some(Ctrl::Loop {
            continue_patches, ..
        }) = self.cur().control.last_mut()
        {
            let patches = std::mem::take(continue_patches);
            self.patch_all(patches, addr);
        }
    }

    fn pop_loop_breaks(&mut self, end_addr: u32) {
        if let Some(Ctrl::Loop { break_patches, .. }) = self.cur().control.pop() {
            self.patch_all(break_patches, end_addr);
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
                    self.patch_all(break_patches, end);
                }
                Ok(())
            }
        }
    }

    fn compile_switch(&mut self, disc: &Expr, cases: &[SwitchCase], line: u32) -> CResult<()> {
        self.compile_expr(disc)?;
        let saved_len = self.cur().locals.len();
        let saved_next_slot = self.cur().next_slot;
        let mut hoisted_slots = Vec::new();
        for c in cases {
            for s in &c.body {
                self.hoist_stmt_names(s, &mut hoisted_slots)?;
            }
        }
        if self.cur().loop_depth > 0 {
            for &(slot, sline) in &hoisted_slots {
                self.emit(OpCode::FreshLocal(slot), sline);
            }
        }
        for c in cases {
            for s in &c.body {
                if let StmtKind::FunctionDecl(f) = &s.kind {
                    self.compile_function_decl_value(f, s.span.start.line)?;
                }
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
        let end_jump_if_no_match = self.emit_jump(OpCode::Jump, line);

        // case ごとのトランポリン（判別値を 1 回だけ Pop してから本体先頭へ飛ぶ）。
        let mut trampolines: Vec<usize> = Vec::new();
        for _ in cases {
            self.emit(OpCode::Pop, line);
            trampolines.push(self.emit_jump(OpCode::Jump, line));
        }
        let end_trampoline_pop = self.cur_addr();
        self.emit(OpCode::Pop, line);
        let end_trampoline_jump = self.emit_jump(OpCode::Jump, line);

        let mut ti = 0usize;
        for (i, c) in cases.iter().enumerate() {
            let tramp_addr = trampolines[i].saturating_sub(1) as u32;
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
            self.patch_all(break_patches, end);
        }
        self.restore_scope(saved_len, saved_next_slot);
        Ok(())
    }

    /// コントロールスタックを逆順に辿り、対象エントリの位置と通過した `finally` 節を収集する。
    fn unwind_control_to(
        &mut self,
        kind: JumpControl,
        label: Option<&str>,
        line: u32,
    ) -> CResult<usize> {
        let is_break = kind == JumpControl::Break;
        let mut finallies: Vec<(usize, Rc<[Stmt]>)> = Vec::new();
        let mut target_idx = None;
        for i in (0..self.cur().control.len()).rev() {
            match &self.cur().control[i] {
                Ctrl::TryFinally { finally, .. } => finallies.push((i, finally.clone())),
                Ctrl::Loop { label: l, .. } if label.is_none() || l.as_deref() == label => {
                    target_idx = Some(i);
                    break;
                }
                Ctrl::Switch { .. } if is_break && label.is_none() => {
                    target_idx = Some(i);
                    break;
                }
                Ctrl::Labeled { label: l, .. }
                    if is_break && label.is_some() && Some(&**l) == label =>
                {
                    target_idx = Some(i);
                    break;
                }
                Ctrl::FuncBoundary => break,
                _ => {}
            }
        }
        let target = target_idx.ok_or_else(|| {
            err(
                line,
                match kind {
                    JumpControl::Break => "break の対象が見つかりません",
                    JumpControl::Continue => "continue の対象が見つかりません",
                },
            )
        })?;
        for (idx, fb) in &finallies {
            self.compile_finally_copy(*idx, fb)?;
        }
        Ok(target)
    }

    fn compile_jump_control(
        &mut self,
        kind: JumpControl,
        label: Option<&str>,
        line: u32,
    ) -> CResult<()> {
        let target_idx = self.unwind_control_to(kind, label, line)?;
        let jidx = self.emit_jump(OpCode::Jump, line);
        match (kind, &mut self.cur().control[target_idx]) {
            (
                JumpControl::Break,
                Ctrl::Loop { break_patches, .. }
                | Ctrl::Switch { break_patches }
                | Ctrl::Labeled { break_patches, .. },
            ) => break_patches.push(jidx),
            (
                JumpControl::Continue,
                Ctrl::Loop {
                    continue_patches, ..
                },
            ) => continue_patches.push(jidx),
            _ => unreachable!(),
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
                Ctrl::TryFinally { finally, .. } => finallies.push((i, finally.clone())),
                Ctrl::FuncBoundary => break,
                _ => {}
            }
        }
        if !finallies.is_empty() {
            // `finally` 内で inner `try { throw } catch {}` が起きても戻り値が
            // オペランドスタック巻き戻しで消えないよう専用ローカルへ退避する（T-3）。
            let saved_next = self.cur().next_slot;
            let ret_slot = self.cur().alloc_slot(line)?;
            self.emit(OpCode::BindLocal(ret_slot), line);
            for (idx, fb) in &finallies {
                self.compile_finally_copy(*idx, fb)?;
            }
            self.emit(OpCode::GetLocal(ret_slot), line);
            self.emit(OpCode::Return, line);
            let cur_len = self.cur().locals.len();
            self.restore_scope(cur_len, saved_next);
        } else {
            self.emit(OpCode::Return, line);
        }
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
                excluded_ranges: Vec::new(),
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
            let saved_next_slot = self.cur().next_slot;
            if let Some(pat) = &h.param {
                let in_loop = self.cur().loop_depth > 0;
                for name in pattern_names(pat) {
                    let slot = self.declare_local(name, line)?;
                    if in_loop {
                        self.emit(OpCode::FreshLocal(slot), line);
                    }
                }
                self.compile_bind_pattern(pat, line)?;
            } else {
                self.emit(OpCode::Pop, line);
            }
            self.hoist_and_compile(&h.body)?;
            self.restore_scope(saved_len, saved_next_slot);
            catch_end = self.cur_addr();
            if let (Some(fin), Some(idx)) = (&finally_rc, control_idx) {
                self.compile_finally_copy(idx, fin)?;
            }
            jump_after_catch = Some(self.emit_jump(OpCode::Jump, line));
        }

        let mut excluded_ranges = if let Some(idx) = control_idx {
            match self.cur().control.remove(idx) {
                Ctrl::TryFinally {
                    excluded_ranges, ..
                } => excluded_ranges,
                _ => Vec::new(),
            }
        } else {
            Vec::new()
        };
        excluded_ranges.sort_unstable_by_key(|&(s, _)| s);

        let unwind_addr = if let Some(fin) = &finally_rc {
            let addr = self.cur_addr();
            // try 文ごとに専用の例外退避スロットを確保し、入れ子の巻き戻しで
            // 外側の例外値が上書きされるのを防ぐ（T-13）。
            let saved_next = self.cur().next_slot;
            let exc_slot = self.cur().alloc_slot(line)?;
            self.emit(OpCode::BindLocal(exc_slot), line);
            self.compile_block_new_scope(fin)?;
            self.emit(OpCode::GetLocal(exc_slot), line);
            self.emit(OpCode::Throw, line);
            let cur_len = self.cur().locals.len();
            self.restore_scope(cur_len, saved_next);
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
            push_exception_entries(
                &mut self.cur().chunk.exception_table,
                try_start,
                try_end,
                catch_addr,
                &excluded_ranges,
            );
            if let Some(unwind) = unwind_addr {
                push_exception_entries(
                    &mut self.cur().chunk.exception_table,
                    catch_addr,
                    catch_end,
                    unwind,
                    &excluded_ranges,
                );
            }
        } else if let Some(unwind) = unwind_addr {
            push_exception_entries(
                &mut self.cur().chunk.exception_table,
                try_start,
                try_end,
                unwind,
                &excluded_ranges,
            );
        }
        Ok(())
    }

    fn compile_finally_copy(&mut self, control_idx: usize, finally: &[Stmt]) -> CResult<()> {
        let saved_tail: Vec<Ctrl> = self.cur().control.drain(control_idx..).collect();
        let copy_start = self.cur_addr();
        let r = self.compile_block_new_scope(finally);
        let copy_end = self.cur_addr();
        self.cur().control.extend(saved_tail);
        if copy_end > copy_start {
            for ctrl in &mut self.cur().control[control_idx..] {
                if let Ctrl::TryFinally {
                    excluded_ranges, ..
                } = ctrl
                {
                    excluded_ranges.push((copy_start, copy_end));
                }
            }
        }
        r
    }
}

/// `[range_start, range_end)` から `excluded`（昇順ソート済み）を除いた区間列を
/// 例外テーブルへ登録する（T-13）。
fn push_exception_entries(
    table: &mut Vec<ExceptionEntry>,
    range_start: u32,
    range_end: u32,
    handler_addr: u32,
    excluded: &[(u32, u32)],
) {
    let mut cur = range_start;
    for &(ex_start, ex_end) in excluded {
        if ex_end <= cur || ex_start >= range_end {
            continue;
        }
        let cut_start = ex_start.max(cur);
        if cut_start > cur {
            table.push(ExceptionEntry {
                try_start: cur,
                try_end: cut_start,
                handler_addr,
            });
        }
        cur = ex_end.min(range_end);
        if cur >= range_end {
            break;
        }
    }
    if cur < range_end {
        table.push(ExceptionEntry {
            try_start: cur,
            try_end: range_end,
            handler_addr,
        });
    }
}
