//! 式・演算子・代入・テンプレート・呼び出しチェーンのコンパイルと純粋性解析。

use std::borrow::Cow;
use std::rc::Rc;

use crate::ast::*;
use crate::bytecode::*;

use super::{err, CResult, Compiler, VarLoc};

impl Compiler {
    /// 条件式 `cond` を評価し、偽ならジャンプする命令を発行してパッチ位置を返す。
    /// `<` `<=` `>` `>=` `===` `!==` は融合比較ジャンプ命令に落とす。
    pub(super) fn compile_cond_jump_false(&mut self, cond: &Expr, line: u32) -> CResult<usize> {
        if let ExprKind::Binary { op, left, right } = &cond.kind {
            let fused = match op {
                BinaryOp::Lt => Some(OpCode::LtJumpIfFalse(0)),
                BinaryOp::LtEq => Some(OpCode::LtEqJumpIfFalse(0)),
                BinaryOp::Gt => Some(OpCode::GtJumpIfFalse(0)),
                BinaryOp::GtEq => Some(OpCode::GtEqJumpIfFalse(0)),
                BinaryOp::StrictEq => Some(OpCode::StrictEqJumpIfFalse(0)),
                BinaryOp::StrictNotEq => Some(OpCode::StrictNotEqJumpIfFalse(0)),
                _ => None,
            };
            if let Some(op) = fused {
                self.compile_expr(left)?;
                self.compile_expr(right)?;
                return Ok(self.emit(op, line));
            }
        }
        self.compile_expr(cond)?;
        Ok(self.emit_jump(OpCode::JumpIfFalse, line))
    }

    /// 式を評価して結果値を捨てる文脈（式文・`for` の init/update）用。
    pub(super) fn compile_expr_discard(&mut self, e: &Expr) -> CResult<()> {
        let line = e.span.start.line;
        match &e.kind {
            ExprKind::Update { op, arg, .. } => {
                if let ExprKind::Ident(name) = &arg.kind {
                    if let VarLoc::Local(slot) = self.resolve(name) {
                        let code = match op {
                            UpdateOp::Inc => OpCode::IncLocal(slot),
                            UpdateOp::Dec => OpCode::DecLocal(slot),
                        };
                        self.emit(code, line);
                        return Ok(());
                    }
                }
            }
            ExprKind::Assign { op, target, value } => {
                if let ExprKind::Ident(name) = &target.kind {
                    if let VarLoc::Local(slot) = self.resolve(name) {
                        if *op == AssignOp::Assign {
                            self.compile_expr(value)?;
                            self.emit(OpCode::BindLocal(slot), line);
                            return Ok(());
                        }
                        if let CompoundAssignOp::Binary(plain_op) = classify_compound_assign(*op) {
                            self.emit(OpCode::GetLocal(slot), line);
                            self.compile_expr(value)?;
                            self.emit(plain_op, line);
                            self.emit(OpCode::BindLocal(slot), line);
                            return Ok(());
                        }
                    }
                }
            }
            ExprKind::Sequence(list) => {
                for item in list {
                    self.compile_expr_discard(item)?;
                }
                return Ok(());
            }
            _ => {}
        }
        self.compile_expr(e)?;
        self.emit(OpCode::Pop, line);
        Ok(())
    }

    pub(super) fn compile_expr(&mut self, e: &Expr) -> CResult<()> {
        let line = e.span.start.line;
        match &e.kind {
            ExprKind::NumberLit(n) => {
                if n.fract() == 0.0
                    && n.is_finite()
                    && !(*n == 0.0 && n.is_sign_negative())
                    && *n >= (i32::MIN as f64)
                    && *n <= (i32::MAX as f64)
                {
                    self.emit(OpCode::LoadSmallInt(*n as i32), line);
                } else {
                    let c = self.const_num(*n);
                    self.emit(OpCode::LoadConst(c), line);
                }
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
                    err(line, format!("正規表現 /{pattern}/ を解釈できません: {e2}"))
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
                let name = f.name.as_deref().unwrap_or("");
                let tpl = self.compile_function(f, name)?;
                let c = self.add_const(Const::FunctionTemplate(Rc::new(tpl)));
                self.emit(OpCode::MakeClosure(c), line);
            }
            ExprKind::Arrow(f) => {
                let tpl = self.compile_function(f, "")?;
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
                let jf = self.compile_cond_jump_false(test, line)?;
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
        if exprs.is_empty() {
            let c0 = self.add_const_str(&quasis[0]);
            self.emit(OpCode::LoadConst(c0), line);
            return Ok(());
        }
        let mut count = 0usize;
        if !quasis[0].is_empty() {
            let c0 = self.add_const_str(&quasis[0]);
            self.emit(OpCode::LoadConst(c0), line);
            count += 1;
        }
        for (i, e) in exprs.iter().enumerate() {
            self.compile_expr(e)?;
            count += 1;
            if !quasis[i + 1].is_empty() {
                let cq = self.add_const_str(&quasis[i + 1]);
                self.emit(OpCode::LoadConst(cq), line);
                count += 1;
            }
        }
        if count == 0 {
            let c0 = self.add_const_str("");
            self.emit(OpCode::LoadConst(c0), line);
        } else if count <= u16::MAX as usize {
            self.emit(OpCode::ConcatStrings(count as u16), line);
        } else {
            for _ in 1..count {
                self.emit(OpCode::ConcatStrings(2), line);
            }
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
                    let tpl = self.compile_function(function, &name)?;
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

    fn compile_update(&mut self, op: UpdateOp, arg: &Expr, prefix: bool, line: u32) -> CResult<()> {
        let delta = match op {
            UpdateOp::Inc => 1i32,
            UpdateOp::Dec => -1i32,
        };
        match &arg.kind {
            ExprKind::Ident(name) => {
                self.emit_get_var(name, line);
                self.emit(OpCode::Pos, line);
                if !prefix {
                    self.emit(OpCode::Dup, line);
                }
                self.emit(OpCode::LoadSmallInt(delta), line);
                self.emit(OpCode::Add, line);
                self.emit_set_var(name, line);
                if !prefix {
                    self.emit(OpCode::Pop, line);
                }
            }
            ExprKind::Member { obj, prop, .. } => {
                enum MemberKey {
                    Static(ConstIdx),
                    Computed(LocalIdx),
                }
                let tmp_obj = self.temp_slot();
                self.compile_expr(obj)?;
                self.emit(OpCode::BindLocal(tmp_obj), line);
                let member_key = match prop {
                    PropertyKey::Computed(ke) => {
                        let t = self.temp_slot();
                        self.compile_expr(ke)?;
                        self.emit(OpCode::BindLocal(t), line);
                        MemberKey::Computed(t)
                    }
                    _ => {
                        let name = property_key_static_name(prop);
                        MemberKey::Static(self.add_const_str(&name))
                    }
                };
                self.emit(OpCode::GetLocal(tmp_obj), line);
                match member_key {
                    MemberKey::Static(c) => {
                        self.emit(OpCode::GetProp(c), line);
                    }
                    MemberKey::Computed(t) => {
                        self.emit(OpCode::GetLocal(t), line);
                        self.emit(OpCode::GetIndex, line);
                    }
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
                self.emit(OpCode::LoadSmallInt(delta), line);
                self.emit(OpCode::Add, line);
                let tmp_new = self.temp_slot();
                self.emit(OpCode::BindLocal(tmp_new), line);
                self.emit(OpCode::GetLocal(tmp_obj), line);
                match member_key {
                    MemberKey::Static(c) => {
                        self.emit(OpCode::GetLocal(tmp_new), line);
                        self.emit(OpCode::SetProp(c), line);
                    }
                    MemberKey::Computed(t) => {
                        self.emit(OpCode::GetLocal(t), line);
                        self.emit(OpCode::GetLocal(tmp_new), line);
                        self.emit(OpCode::SetIndex, line);
                    }
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

        let compound = classify_compound_assign(op);
        match &target.kind {
            ExprKind::Ident(name) => match compound {
                CompoundAssignOp::Logical(jump_op) => {
                    self.emit_get_var(name, line);
                    let j = self.emit_jump(jump_op, line);
                    self.emit(OpCode::Pop, line);
                    self.compile_expr(value)?;
                    self.emit_set_var(name, line);
                    let end = self.cur_addr();
                    self.patch_jump(j, end);
                }
                CompoundAssignOp::Binary(plain_op) => {
                    self.emit_get_var(name, line);
                    self.compile_expr(value)?;
                    self.emit(plain_op, line);
                    self.emit_set_var(name, line);
                }
            },
            ExprKind::Member { obj, prop, .. } => {
                if let PropertyKey::Computed(ke) = prop {
                    let tmp_obj = self.temp_slot();
                    self.compile_expr(obj)?;
                    self.emit(OpCode::BindLocal(tmp_obj), line);
                    let tmp_key = self.temp_slot();
                    self.compile_expr(ke)?;
                    self.emit(OpCode::BindLocal(tmp_key), line);
                    self.emit(OpCode::GetLocal(tmp_obj), line);
                    self.emit(OpCode::GetLocal(tmp_key), line);
                    self.emit(OpCode::GetIndex, line);
                    match compound {
                        CompoundAssignOp::Logical(jump_op) => {
                            let j = self.emit_jump(jump_op, line);
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
                        }
                        CompoundAssignOp::Binary(plain_op) => {
                            self.compile_expr(value)?;
                            self.emit(plain_op, line);
                            let tmp_new = self.temp_slot();
                            self.emit(OpCode::BindLocal(tmp_new), line);
                            self.emit(OpCode::GetLocal(tmp_obj), line);
                            self.emit(OpCode::GetLocal(tmp_key), line);
                            self.emit(OpCode::GetLocal(tmp_new), line);
                            self.emit(OpCode::SetIndex, line);
                        }
                    }
                    return Ok(());
                }
                let name = property_key_static_name(prop);
                let c = self.add_const_str(&name);
                let tmp_obj = self.temp_slot();
                self.compile_expr(obj)?;
                self.emit(OpCode::BindLocal(tmp_obj), line);
                self.emit(OpCode::GetLocal(tmp_obj), line);
                self.emit(OpCode::GetProp(c), line);
                match compound {
                    CompoundAssignOp::Logical(jump_op) => {
                        let j = self.emit_jump(jump_op, line);
                        self.emit(OpCode::Pop, line);
                        self.emit(OpCode::GetLocal(tmp_obj), line);
                        self.compile_expr(value)?;
                        self.emit(OpCode::SetProp(c), line);
                        let end = self.cur_addr();
                        self.patch_jump(j, end);
                    }
                    CompoundAssignOp::Binary(plain_op) => {
                        self.compile_expr(value)?;
                        self.emit(plain_op, line);
                        self.emit(OpCode::GetLocal(tmp_obj), line);
                        self.emit(OpCode::Swap, line);
                        self.emit(OpCode::SetProp(c), line);
                    }
                }
            }
            _ => return Err(err(line, "代入の左辺として使えない式です")),
        }
        Ok(())
    }

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
                if !*optional && !args.iter().any(|a| matches!(a.kind, ExprKind::Spread(_))) {
                    if let ExprKind::Member {
                        obj,
                        prop,
                        optional: false,
                    } = &callee.kind
                    {
                        if !matches!(prop, PropertyKey::Computed(_))
                            && args.len() <= u16::MAX as usize
                            && args.iter().all(is_pure_call_arg)
                        {
                            let name = property_key_static_name(prop);
                            let c = self.add_const_str(&name);
                            if c <= u16::MAX as u32 {
                                self.compile_chain_or_base(obj, patches)?;
                                for a in args {
                                    self.compile_expr(a)?;
                                }
                                self.emit(OpCode::CallMethod(c as u16, args.len() as u16), line);
                                return Ok(());
                            }
                        }
                    }
                }
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
                self.compile_call_args(args, OpCode::Call, OpCode::CallSpread, line)?;
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

    fn compile_call_args(
        &mut self,
        args: &[Expr],
        normal_op: fn(u16) -> OpCode,
        spread_op: OpCode,
        line: u32,
    ) -> CResult<()> {
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
            self.emit(spread_op, line);
        } else {
            for a in args {
                self.compile_expr(a)?;
            }
            self.emit(normal_op(args.len() as u16), line);
        }
        Ok(())
    }

    fn compile_new(&mut self, callee: &Expr, args: &[Expr], line: u32) -> CResult<()> {
        self.compile_expr(callee)?;
        self.compile_call_args(args, OpCode::New, OpCode::NewSpread, line)
    }
}

/// 複合代入演算子の分類（論理短絡ジャンプか、二項演算命令か）。
enum CompoundAssignOp {
    Logical(fn(JumpAddr) -> OpCode),
    Binary(OpCode),
}

fn classify_compound_assign(op: AssignOp) -> CompoundAssignOp {
    match op {
        AssignOp::AddAssign => CompoundAssignOp::Binary(OpCode::Add),
        AssignOp::SubAssign => CompoundAssignOp::Binary(OpCode::Sub),
        AssignOp::MulAssign => CompoundAssignOp::Binary(OpCode::Mul),
        AssignOp::DivAssign => CompoundAssignOp::Binary(OpCode::Div),
        AssignOp::ModAssign => CompoundAssignOp::Binary(OpCode::Mod),
        AssignOp::PowAssign => CompoundAssignOp::Binary(OpCode::Pow),
        AssignOp::ShlAssign => CompoundAssignOp::Binary(OpCode::Shl),
        AssignOp::ShrAssign => CompoundAssignOp::Binary(OpCode::Shr),
        AssignOp::UShrAssign => CompoundAssignOp::Binary(OpCode::UShr),
        AssignOp::BitAndAssign => CompoundAssignOp::Binary(OpCode::BitAnd),
        AssignOp::BitOrAssign => CompoundAssignOp::Binary(OpCode::BitOr),
        AssignOp::BitXorAssign => CompoundAssignOp::Binary(OpCode::BitXor),
        AssignOp::AndAssign => CompoundAssignOp::Logical(OpCode::JumpIfFalsyKeep),
        AssignOp::OrAssign => CompoundAssignOp::Logical(OpCode::JumpIfTruthyKeep),
        AssignOp::NullishAssign => CompoundAssignOp::Logical(OpCode::JumpIfNotNullishKeep),
        AssignOp::Assign => unreachable!("単純代入は呼び出し側で処理済み"),
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

pub(super) fn property_key_static_name(key: &PropertyKey) -> Cow<'_, str> {
    match key {
        PropertyKey::Ident(s) | PropertyKey::Str(s) => Cow::Borrowed(s.as_str()),
        PropertyKey::Num(n) => Cow::Owned(crate::vm::number_to_js_string(*n)),
        PropertyKey::Computed(_) => panic!("computed key を静的名として扱おうとしました"),
    }
}

pub(super) fn expr_contains_closure(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Function(_) | ExprKind::Arrow(_) | ExprKind::Class(_) => true,
        ExprKind::Unary { arg, .. }
        | ExprKind::Update { arg, .. }
        | ExprKind::Spread(arg)
        | ExprKind::Await(arg)
        | ExprKind::TsAs(arg)
        | ExprKind::TsNonNull(arg) => expr_contains_closure(arg),
        ExprKind::Binary { left, right, .. } | ExprKind::Logical { left, right, .. } => {
            expr_contains_closure(left) || expr_contains_closure(right)
        }
        ExprKind::Assign { target, value, .. } => {
            expr_contains_closure(target) || expr_contains_closure(value)
        }
        ExprKind::Conditional { test, cons, alt } => {
            expr_contains_closure(test) || expr_contains_closure(cons) || expr_contains_closure(alt)
        }
        ExprKind::Call { callee, args, .. } | ExprKind::New { callee, args } => {
            expr_contains_closure(callee) || args.iter().any(expr_contains_closure)
        }
        ExprKind::Member { obj, prop, .. } => {
            expr_contains_closure(obj)
                || matches!(prop, PropertyKey::Computed(k) if expr_contains_closure(k))
        }
        ExprKind::ArrayLit(els) => els.iter().flatten().any(expr_contains_closure),
        ExprKind::ObjectLit(props) => props.iter().any(|p| match p {
            ObjectProp::KeyValue { key, value, .. } => {
                expr_contains_closure(value)
                    || matches!(key, PropertyKey::Computed(k) if expr_contains_closure(k))
            }
            ObjectProp::Method { .. } => true,
            ObjectProp::Spread(s) => expr_contains_closure(s),
        }),
        ExprKind::TemplateLit { exprs, .. } | ExprKind::Sequence(exprs) => {
            exprs.iter().any(expr_contains_closure)
        }
        _ => false,
    }
}

/// `CallMethod` の引数として安全（副作用の観測順序が変わらない）かどうか。
fn is_pure_call_arg(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::NumberLit(_)
        | ExprKind::StringLit(_)
        | ExprKind::BoolLit(_)
        | ExprKind::NullLit
        | ExprKind::UndefinedLit
        | ExprKind::Ident(_)
        | ExprKind::This
        | ExprKind::RegexLit { .. } => true,
        ExprKind::Unary { op, arg } => !matches!(op, UnaryOp::Delete) && is_pure_call_arg(arg),
        ExprKind::Binary { op, left, right } => {
            !matches!(op, BinaryOp::In | BinaryOp::Instanceof)
                && is_pure_call_arg(left)
                && is_pure_call_arg(right)
        }
        ExprKind::Logical { left, right, .. } => is_pure_call_arg(left) && is_pure_call_arg(right),
        ExprKind::Conditional { test, cons, alt } => {
            is_pure_call_arg(test) && is_pure_call_arg(cons) && is_pure_call_arg(alt)
        }
        ExprKind::TemplateLit { exprs, .. } => exprs.iter().all(is_pure_call_arg),
        ExprKind::TsAs(inner) | ExprKind::TsNonNull(inner) => is_pure_call_arg(inner),
        _ => false,
    }
}
