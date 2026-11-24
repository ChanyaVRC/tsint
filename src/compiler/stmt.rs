//! 文・ブロックスコープ・巻き上げ・制御フロー（ループ / switch / try-catch-finally）のコンパイル。

use std::rc::Rc;

use crate::ast::{self, *};
use crate::bytecode::*;

use super::expr::expr_contains_closure;
use super::pattern::pattern_names;
use super::{err, CResult, Compiler, Ctrl};

impl Compiler {
    /// ブロック（文リスト）を新しいレキシカルスコープでコンパイルする。
    pub(super) fn compile_block_new_scope(&mut self, stmts: &[Stmt]) -> CResult<()> {
        let saved_len = self.cur().locals.len();
        self.hoist_and_compile(stmts)?;
        self.cur().locals.truncate(saved_len);
        Ok(())
    }

    /// モジュール/関数のトップレベル文列。
    pub(super) fn compile_top_level_stmts(&mut self, stmts: &[Stmt]) -> CResult<()> {
        self.hoist_and_compile(stmts)
    }

    /// 文リストの巻き上げ（1st: 名前予約, 2nd: 関数宣言初期化, 3rd: 文コンパイル）。
    pub(super) fn hoist_and_compile(&mut self, stmts: &[Stmt]) -> CResult<()> {
        for s in stmts {
            self.hoist_stmt_names(s)?;
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

    pub(super) fn compile_stmt(&mut self, s: &Stmt) -> CResult<()> {
        let line = s.span.start.line;
        match &s.kind {
            StmtKind::Empty | StmtKind::Debugger => {}
            StmtKind::Expr(e) => {
                self.compile_expr_discard(e)?;
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
        self.compile_stmt(body)?;
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
        self.cur().locals.truncate(saved_len);
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
        self.compile_expr(right)?;
        if is_of {
            self.emit(OpCode::MakeIterArray, line);
        } else {
            self.emit(OpCode::ObjectKeysArray, line);
        }
        let arr_slot = self.declare_local("%iter_arr");
        self.emit(OpCode::BindLocal(arr_slot), line);
        self.emit(OpCode::LoadSmallInt(0), line);
        let idx_slot = self.declare_local("%iter_idx");
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
        self.emit(OpCode::IncLocal(idx_slot), line);
        self.emit(OpCode::Jump(test_addr), line);

        let end = self.cur_addr();
        self.patch_jump(jf, end);
        self.pop_loop_breaks(end);
        self.cur().locals.truncate(saved_len);
        Ok(())
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

    fn compile_switch(&mut self, disc: &Expr, cases: &[SwitchCase], line: u32) -> CResult<()> {
        self.compile_expr(disc)?;
        let saved_len = self.cur().locals.len();
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
            for idx in break_patches {
                self.patch_jump(idx, end);
            }
        }
        self.cur().locals.truncate(saved_len);
        Ok(())
    }

    /// コントロールスタックを逆順に辿り、対象エントリの位置と通過した `finally` 節を収集する。
    fn unwind_control_to(
        &mut self,
        is_break: bool,
        label: Option<&str>,
        line: u32,
    ) -> CResult<usize> {
        let mut finallies: Vec<(usize, Rc<[Stmt]>)> = Vec::new();
        let mut target_idx = None;
        for i in (0..self.cur().control.len()).rev() {
            match &self.cur().control[i] {
                Ctrl::TryFinally { finally } => finallies.push((i, finally.clone())),
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
                if is_break {
                    "break の対象が見つかりません"
                } else {
                    "continue の対象が見つかりません"
                },
            )
        })?;
        for (idx, fb) in &finallies {
            self.compile_finally_copy(*idx, fb)?;
        }
        Ok(target)
    }

    fn compile_break(&mut self, label: Option<&str>, line: u32) -> CResult<()> {
        let target_idx = self.unwind_control_to(true, label, line)?;
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
        let target_idx = self.unwind_control_to(false, label, line)?;
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
}
