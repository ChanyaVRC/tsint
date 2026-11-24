//! 分割代入（宣言/代入先）・関数・クラスのコンパイル。

use std::rc::Rc;

use crate::ast::*;
use crate::bytecode::*;

use super::expr::property_key_static_name;
use super::{err, CResult, Compiler, FuncCtx, LocalVar};

impl Compiler {
    // ---------------- 分割代入（宣言側: Pattern） ----------------

    pub(super) fn compile_bind_pattern(&mut self, pat: &Pattern, line: u32) -> CResult<()> {
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
                            let c = self.add_const_str(&name);
                            if rest.is_some() {
                                used_keys.push(name.into_owned());
                            }
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
                        self.emit(OpCode::LoadSmallInt(i as i32), line);
                        self.emit(OpCode::GetIndex, line);
                        self.compile_bind_pattern(subpat, line)?;
                    }
                }
                if let Some(rest) = rest {
                    self.emit(OpCode::Dup, line);
                    self.emit(OpCode::LoadSmallInt(elements.len() as i32), line);
                    self.emit(OpCode::ArraySliceFrom, line);
                    self.compile_bind_pattern(rest, line)?;
                }
                self.emit(OpCode::Pop, line);
            }
        }
        Ok(())
    }

    // ---------------- 分割代入（代入式側: Expr を疑似パターンとして扱う） ----------------

    pub(super) fn compile_assign_target_expr(&mut self, target: &Expr, line: u32) -> CResult<()> {
        match &target.kind {
            ExprKind::ArrayLit(elements) => {
                self.emit(OpCode::MakeIterArray, line);
                for (idx, el) in elements.iter().enumerate() {
                    if let Some(e) = el {
                        if let ExprKind::Spread(inner) = &e.kind {
                            self.emit(OpCode::Dup, line);
                            self.emit(OpCode::LoadSmallInt(idx as i32), line);
                            self.emit(OpCode::ArraySliceFrom, line);
                            self.compile_assign_target_expr(inner, line)?;
                            continue;
                        }
                        self.emit(OpCode::Dup, line);
                        self.emit(OpCode::LoadSmallInt(idx as i32), line);
                        self.emit(OpCode::GetIndex, line);
                        self.compile_assign_target_leaf(e, line)?;
                    }
                }
                self.emit(OpCode::Pop, line);
            }
            ExprKind::ObjectLit(props) => {
                let has_spread = props.iter().any(|p| matches!(p, ObjectProp::Spread(_)));
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
                                    let c = self.add_const_str(&name);
                                    if has_spread {
                                        used_keys.push(name.into_owned());
                                    }
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

    pub(super) fn compile_function(
        &mut self,
        f: &Function,
        name: &str,
    ) -> CResult<FunctionTemplate> {
        self.funcs.push(FuncCtx::new(name));
        if !f.is_arrow {
            let this_slot = self.cur().alloc_slot();
            self.cur().locals.push(LocalVar {
                name: "%this".into(),
                slot: this_slot,
            });
        }

        let mut has_rest = false;
        let mut simple_prefix = 0usize;
        for p in &f.params {
            if !p.rest && p.default.is_none() {
                if let Pattern::Ident(pname) = &p.pattern {
                    if self.cur().find_local(pname).is_none() {
                        self.declare_local(pname);
                        simple_prefix += 1;
                        continue;
                    }
                }
            }
            break;
        }
        self.cur().chunk.simple_param_count = simple_prefix as u16;
        self.cur().chunk.needs_raw_args = simple_prefix < f.params.len();

        for (i, p) in f.params.iter().enumerate().skip(simple_prefix) {
            let line = f.span.start.line;
            if p.rest {
                has_rest = true;
                self.emit(OpCode::GetRestArgs(i as u16), line);
                for name in pattern_names(&p.pattern) {
                    self.declare_local(name);
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
                self.declare_local(name);
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
            chunk: Rc::new(fctx.chunk.finish()),
            upvalues: fctx.upvalues.into_boxed_slice(),
            param_count: f.params.iter().filter(|p| !p.rest).count() as u16,
            is_arrow: f.is_arrow,
            is_async: f.is_async,
            has_rest,
        })
    }

    // ---------------- クラス ----------------

    pub(super) fn compile_class_value(&mut self, c: &Class, line: u32) -> CResult<()> {
        if c.super_class.is_some() {
            return Err(err(
                line,
                "class の extends は T2 では未対応です（コーパスに無いため省略）",
            ));
        }
        let name = c.name.as_deref().unwrap_or("<anonymous class>");
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
                    let tpl = self.compile_function(f, "constructor")?;
                    ctor_tpl = Some(Rc::new(tpl));
                }
                ClassMemberKind::Method => {
                    let f = m.function.as_ref().unwrap();
                    let tpl = self.compile_function(f, &mname)?;
                    methods.push(ClassMethodTemplate {
                        name: Rc::from(mname.as_ref()),
                        is_static: m.is_static,
                        func: Rc::new(tpl),
                    });
                }
                ClassMemberKind::Getter | ClassMemberKind::Setter => {
                    let f = m.function.as_ref().unwrap();
                    let tpl = self.compile_function(f, &mname)?;
                    accessors.push(ClassAccessorTemplate {
                        name: Rc::from(mname.as_ref()),
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
                        static_fields.push((Rc::from(mname.as_ref()), init_tpl));
                    } else {
                        instance_fields.push((Rc::from(mname.as_ref()), init_tpl));
                    }
                }
            }
        }

        let template = ClassTemplate {
            name: Rc::from(name),
            ctor: ctor_tpl,
            methods: methods.into_boxed_slice(),
            accessors: accessors.into_boxed_slice(),
            static_fields: static_fields.into_boxed_slice(),
            instance_fields: instance_fields.into_boxed_slice(),
        };
        let cidx = self.add_const(Const::ClassTemplate(Rc::new(template)));
        self.emit(OpCode::MakeClass(cidx), line);
        Ok(())
    }

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
            chunk: Rc::new(fctx.chunk.finish()),
            upvalues: fctx.upvalues.into_boxed_slice(),
            param_count: 0,
            is_arrow: false,
            is_async: false,
            has_rest: false,
        }))
    }
}

pub(super) fn pattern_names(pat: &Pattern) -> Vec<&str> {
    let mut out = Vec::new();
    collect_pattern_names(pat, &mut out);
    out
}

fn collect_pattern_names<'a>(pat: &'a Pattern, out: &mut Vec<&'a str>) {
    match pat {
        Pattern::Ident(n) => out.push(n.as_str()),
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
