//! 関数・メソッド呼び出し、`new`、クロージャおよびクラス生成。

use std::rc::Rc;

use crate::bytecode::*;
use crate::value::*;

use super::props::kind_tag_of;
use super::{
    CallOutcome, KindTag, RunOutcome, StackCalleeSlot, Vm, MAX_CALL_FRAMES, MAX_NATIVE_CALL_DEPTH,
};

impl Vm {
    pub(super) fn build_closure(&self, tpl: &Rc<FunctionTemplate>) -> Rc<FunctionObj> {
        let upvalues = if tpl.upvalues.is_empty() {
            self.empty_upvalues.clone()
        } else {
            let frame = self.frames.last().unwrap();
            let mut ups = Vec::with_capacity(tpl.upvalues.len());
            for d in &tpl.upvalues {
                let cell = match d {
                    UpvalueDesc::ParentLocal(slot) => frame.locals[*slot as usize].as_boxed_cell(),
                    UpvalueDesc::ParentUpvalue(idx) => frame.upvalues[*idx as usize].clone(),
                };
                ups.push(cell);
            }
            Rc::new(ups)
        };
        Rc::new(FunctionObj {
            name: tpl.name.clone(),
            chunk: tpl.chunk.clone(),
            upvalues,
            is_arrow: tpl.is_arrow,
            is_async: tpl.is_async,
        })
    }

    fn install_class_members(&self, target: &mut PropMap, tpl: &ClassTemplate, is_static: bool) {
        for m in tpl.methods.iter().filter(|m| m.is_static == is_static) {
            let fo = self.build_closure(&m.func);
            target.set_value(m.name.clone(), self.wrap_function(fo));
        }
        for a in tpl.accessors.iter().filter(|a| a.is_static == is_static) {
            let fo = self.build_closure(&a.func);
            target.define_accessor(a.name.clone(), a.is_get, self.wrap_function(fo));
        }
    }

    pub(super) fn build_class(&mut self, tpl: &Rc<ClassTemplate>) -> Result<Value, Value> {
        let ctor = tpl.ctor.as_ref().map(|c| self.build_closure(c));
        let mut proto = ObjData::new(ObjKind::Plain);
        self.install_class_members(&mut proto.props, tpl, false);
        let proto_rc: ObjRef = self.alloc_obj_ref(proto);

        let instance_fields: InstanceFields = tpl
            .instance_fields
            .iter()
            .map(|(n, f)| (n.clone(), f.as_ref().map(|t| self.build_closure(t))))
            .collect();

        let class_data = ClassData {
            name: tpl.name.clone(),
            ctor,
            prototype: proto_rc.clone(),
            instance_fields,
        };
        let class_rc: ObjRef =
            self.alloc_obj_ref(ObjData::new(ObjKind::Class(Box::new(class_data))));
        proto_rc
            .borrow_mut()
            .props
            .set_value(Rc::from("constructor"), Value::Object(class_rc.clone()));
        {
            let mut cb = class_rc.borrow_mut();
            cb.props
                .set_value(Rc::from("name"), Value::Str(tpl.name.clone()));
            cb.props
                .set_value(Rc::from("prototype"), Value::Object(proto_rc.clone()));
            self.install_class_members(&mut cb.props, tpl, true);
        }
        for (name, init) in &tpl.static_fields {
            let v = match init {
                Some(f) => {
                    let fo = self.build_closure(f);
                    self.call_function_obj(fo, Value::Object(class_rc.clone()), &[])?
                }
                None => Value::Undefined,
            };
            class_rc.borrow_mut().props.set_value(name.clone(), v);
        }

        Ok(Value::Object(class_rc))
    }

    /// 呼び出しフレームのスタックから `argc` 個の引数と（任意で callee スロットを捨てて）`this` を取り出す。
    #[inline]
    fn pop_call_args_and_this(
        &mut self,
        argc: usize,
        callee_slot: StackCalleeSlot,
    ) -> (Value, Vec<Value>) {
        let mut args = self.take_args_buf();
        let this = {
            let caller = self.frames.last_mut().unwrap();
            let n = caller.stack.len();
            args.extend(caller.stack.drain(n - argc..));
            if callee_slot == StackCalleeSlot::Present {
                caller.stack.pop(); // callee
            }
            caller.stack.pop().unwrap() // this / receiver
        };
        (this, args)
    }

    fn finish_method_fallback(
        &mut self,
        obj: Value,
        key: &str,
        args: Vec<Value>,
    ) -> Result<(), Value> {
        let callee = self.get_prop(&obj, key)?;
        if !super::ops::is_callable(&callee) {
            self.put_args_buf(args);
            if matches!(
                &callee,
                Value::Object(o) if matches!(o.borrow().kind, ObjKind::Class(_))
            ) {
                return Err(self.type_error(format!(
                    "'{key}' はクラスコンストラクタのため 'new' なしで呼び出せません"
                )));
            }
            return Err(self.type_error(format!(
                "'{key}' は {} のメソッドとして定義されていません（関数ではありません）",
                obj.type_of()
            )));
        }
        let out = self.dispatch_call_slice(callee, obj, &args);
        self.put_args_buf(args);
        match out? {
            CallOutcome::Immediate(v) => self.push(v),
            CallOutcome::FramePushed => {}
        }
        Ok(())
    }

    #[inline]
    fn invoke_js_fn_from_stack(
        &mut self,
        fo: Rc<FunctionObj>,
        argc: usize,
        slot: StackCalleeSlot,
    ) -> Result<(), Value> {
        if fo.is_async {
            let (this, args) = self.pop_call_args_and_this(argc, slot);
            let res = self.spawn_async_task_from_function(&fo, this, &args);
            self.put_args_buf(args);
            self.push(res?);
        } else {
            self.push_frame_from_caller_stack(&fo, argc, slot)?;
        }
        Ok(())
    }

    #[inline]
    fn invoke_native_from_stack(
        &mut self,
        nf: NativeFn,
        argc: usize,
        slot: StackCalleeSlot,
    ) -> Result<(), Value> {
        let (this, args) = self.pop_call_args_and_this(argc, slot);
        let res = nf(self, this, &args);
        self.put_args_buf(args);
        let v = res?;
        self.track_value_tree(&v);
        self.push(v);
        Ok(())
    }

    pub(super) fn exec_call(&mut self, argc: usize) -> Result<(), Value> {
        let caller = self.frames.last().unwrap();
        let n = caller.stack.len();
        let callee_kind = match &caller.stack[n - argc - 1] {
            Value::Object(o) => {
                let b = o.borrow();
                match &b.kind {
                    ObjKind::Function(f) => Some(Ok(f.clone())),
                    ObjKind::Native(nf) => Some(Err(nf.f.clone())),
                    ObjKind::Class(_) => {
                        drop(b);
                        let caller = self.frames.last_mut().unwrap();
                        caller.stack.truncate(n - argc - 2);
                        return Err(
                            self.type_error("クラスのコンストラクタは 'new' なしで呼び出せません")
                        );
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        let Some(kind) = callee_kind else {
            let caller = self.frames.last_mut().unwrap();
            caller.stack.truncate(n - argc - 2);
            return Err(self.type_error("これは関数ではありません"));
        };
        match kind {
            Ok(fo) => self.invoke_js_fn_from_stack(fo, argc, StackCalleeSlot::Present),
            Err(nf) => self.invoke_native_from_stack(nf, argc, StackCalleeSlot::Present),
        }
    }

    pub(super) fn exec_call_method(
        &mut self,
        cidx: ConstIdx,
        argc: usize,
        op_ip: usize,
    ) -> Result<(), Value> {
        enum MethodTarget {
            Builtin(KindTag),
            Function(Rc<FunctionObj>),
            Native(NativeFn),
            Fallback,
        }
        fn classify_fn_obj(fn_o: &ObjRef) -> MethodTarget {
            match &fn_o.borrow().kind {
                ObjKind::Function(f) => MethodTarget::Function(f.clone()),
                ObjKind::Native(nf) => MethodTarget::Native(nf.f.clone()),
                _ => MethodTarget::Fallback,
            }
        }
        let (key, target) = {
            let caller = self.frames.last().unwrap();
            let key = caller.chunk.const_str(cidx);
            let n = caller.stack.len();
            let obj = &caller.stack[n - argc - 1];
            let hint = caller.chunk.ic_hint(op_ip);
            let tgt = match obj {
                Value::Str(_) => MethodTarget::Builtin(KindTag::Str),
                Value::Object(o) => {
                    let b = o.borrow();
                    if let Some((idx, slot)) = b.props.get_with_hint(&key, hint) {
                        caller.chunk.set_ic_hint(op_ip, idx);
                        match slot {
                            PropSlot::Value(Value::Object(fn_o)) => classify_fn_obj(fn_o),
                            _ => MethodTarget::Fallback,
                        }
                    } else if let Some(tag) = kind_tag_of(&b.kind) {
                        MethodTarget::Builtin(tag)
                    } else if let Some(proto) = &b.proto {
                        let pb = proto.borrow();
                        if let Some((idx, PropSlot::Value(Value::Object(fn_o)))) =
                            pb.props.get_with_hint(&key, hint)
                        {
                            caller.chunk.set_ic_hint(op_ip, idx);
                            classify_fn_obj(fn_o)
                        } else {
                            MethodTarget::Fallback
                        }
                    } else {
                        MethodTarget::Fallback
                    }
                }
                _ => MethodTarget::Fallback,
            };
            (key, tgt)
        };

        match target {
            MethodTarget::Builtin(tag) => {
                let (obj, args) = self.pop_call_args_and_this(argc, StackCalleeSlot::Omitted);
                if let Some(res) =
                    crate::builtins::call_builtin_method(self, tag, obj.clone(), &key, &args)
                {
                    self.put_args_buf(args);
                    let v = res?;
                    self.track_value_tree(&v);
                    self.push(v);
                    return Ok(());
                }
                self.finish_method_fallback(obj, &key, args)
            }
            MethodTarget::Function(fo) => {
                self.invoke_js_fn_from_stack(fo, argc, StackCalleeSlot::Omitted)
            }
            MethodTarget::Native(nf) => {
                self.invoke_native_from_stack(nf, argc, StackCalleeSlot::Omitted)
            }
            MethodTarget::Fallback => {
                let (obj, args) = self.pop_call_args_and_this(argc, StackCalleeSlot::Omitted);
                self.finish_method_fallback(obj, &key, args)
            }
        }
    }

    pub(super) fn exec_new(&mut self, argc: usize) -> Result<(), Value> {
        let mut args = self.take_args_buf();
        let callee = {
            let frame = self.frames.last_mut().unwrap();
            let n = frame.stack.len();
            args.extend(frame.stack.drain(n - argc..));
            frame.stack.pop().unwrap()
        };
        let res = self.dispatch_new_slice(callee, &args);
        self.put_args_buf(args);
        let v = res?;
        self.push(v);
        Ok(())
    }

    pub fn call_value(
        &mut self,
        callee: Value,
        this: Value,
        args: &[Value],
    ) -> Result<Value, Value> {
        let depth_before = self.frames.len();
        match self.dispatch_call_slice(callee, this, args)? {
            CallOutcome::Immediate(v) => Ok(v),
            CallOutcome::FramePushed => self.run_nested_call(depth_before),
        }
    }

    pub fn call_function_obj(
        &mut self,
        f: Rc<FunctionObj>,
        this: Value,
        args: &[Value],
    ) -> Result<Value, Value> {
        if self.frames.len() >= MAX_CALL_FRAMES {
            return Err(self.range_error("Maximum call stack size exceeded"));
        }
        let depth_before = self.frames.len();
        let frame = self.acquire_frame_from_slice(&f, this, args);
        self.frames.push(frame);
        self.run_nested_call(depth_before)
    }

    fn run_nested_call(&mut self, depth_before: usize) -> Result<Value, Value> {
        if self.rust_call_depth >= MAX_NATIVE_CALL_DEPTH {
            while self.frames.len() > depth_before {
                if let Some(f) = self.frames.pop() {
                    self.release_frame(f);
                }
            }
            return Err(self.range_error("Maximum call stack size exceeded"));
        }
        self.native_call_depth += 1;
        self.rust_call_depth += 1;
        let r = self.run_until(depth_before);
        self.rust_call_depth -= 1;
        self.native_call_depth -= 1;
        match r {
            Ok(RunOutcome::Value(v)) => Ok(v),
            Ok(RunOutcome::Suspended) => {
                Err(self.type_error("内部エラー: 予期しないタスク中断です"))
            }
            Err(e) => Err(e),
        }
    }

    fn dispatch_call_slice(
        &mut self,
        callee: Value,
        this: Value,
        args: &[Value],
    ) -> Result<CallOutcome, Value> {
        let Value::Object(o) = &callee else {
            return Err(self.type_error("これは関数ではありません"));
        };
        enum CalleeKind {
            Fn(Rc<FunctionObj>),
            Native(NativeFn),
            Class,
            Other,
        }
        let kind = match &o.borrow().kind {
            ObjKind::Function(f) => CalleeKind::Fn(f.clone()),
            ObjKind::Native(n) => CalleeKind::Native(n.f.clone()),
            ObjKind::Class(_) => CalleeKind::Class,
            _ => CalleeKind::Other,
        };
        match kind {
            CalleeKind::Fn(fo) => {
                if fo.is_async {
                    let promise = self.spawn_async_task_from_function(&fo, this, args)?;
                    return Ok(CallOutcome::Immediate(promise));
                }
                if self.frames.len() >= MAX_CALL_FRAMES {
                    return Err(self.range_error("Maximum call stack size exceeded"));
                }
                let frame = self.acquire_frame_from_slice(&fo, this, args);
                self.frames.push(frame);
                Ok(CallOutcome::FramePushed)
            }
            CalleeKind::Native(nf) => {
                let v = nf(self, this, args)?;
                self.track_value_tree(&v);
                Ok(CallOutcome::Immediate(v))
            }
            CalleeKind::Class => {
                Err(self.type_error("クラスのコンストラクタは 'new' なしで呼び出せません"))
            }
            CalleeKind::Other => Err(self.type_error("これは関数ではありません")),
        }
    }

    pub(super) fn dispatch_new(&mut self, callee: Value, args: Vec<Value>) -> Result<Value, Value> {
        self.dispatch_new_slice(callee, &args)
    }

    fn dispatch_new_slice(&mut self, callee: Value, args: &[Value]) -> Result<Value, Value> {
        let Value::Object(co) = &callee else {
            return Err(self.type_error("これはコンストラクタではありません"));
        };
        enum CtorKind {
            Class(Option<Rc<FunctionObj>>, ObjRef, InstanceFields),
            Native(NativeFn),
            Fn(Rc<FunctionObj>, Option<ObjRef>),
            Other,
        }
        let kind = {
            let b = co.borrow();
            match &b.kind {
                ObjKind::Class(cd) => CtorKind::Class(
                    cd.ctor.clone(),
                    cd.prototype.clone(),
                    cd.instance_fields.clone(),
                ),
                ObjKind::Native(n) => CtorKind::Native(n.f.clone()),
                ObjKind::Function(f) => {
                    let proto = match b.props.get("prototype") {
                        Some(PropSlot::Value(Value::Object(p))) => Some(p.clone()),
                        _ => None,
                    };
                    CtorKind::Fn(f.clone(), proto)
                }
                _ => CtorKind::Other,
            }
        };
        match kind {
            CtorKind::Class(ctor, proto, fields) => {
                let instance = ObjData {
                    kind: ObjKind::Plain,
                    props: PropMap::with_capacity(fields.len()),
                    proto: Some(proto),
                };
                let inst_rc: ObjRef = self.alloc_obj_ref(instance);
                let inst_val = Value::Object(inst_rc.clone());
                for (name, init) in fields.iter() {
                    let v = match init {
                        Some(f) => self.call_function_obj(f.clone(), inst_val.clone(), &[])?,
                        None => Value::Undefined,
                    };
                    inst_rc.borrow_mut().props.set_value(name.clone(), v);
                }
                if let Some(ctor) = ctor {
                    let ret = self.call_function_obj(ctor, inst_val.clone(), args)?;
                    if matches!(ret, Value::Object(_)) {
                        return Ok(ret);
                    }
                }
                Ok(inst_val)
            }
            CtorKind::Native(nf) => {
                let v = nf(self, Value::Undefined, args)?;
                self.track_value_tree(&v);
                Ok(v)
            }
            CtorKind::Fn(fo, proto) => {
                let inst_rc: ObjRef = self.alloc_obj_ref(ObjData {
                    kind: ObjKind::Plain,
                    props: PropMap::new(),
                    proto,
                });
                let inst_val = Value::Object(inst_rc.clone());
                let ret = self.call_function_obj(fo, inst_val.clone(), args)?;
                if matches!(ret, Value::Object(_)) {
                    return Ok(ret);
                }
                Ok(inst_val)
            }
            CtorKind::Other => Err(self.type_error("これはコンストラクタではありません")),
        }
    }
}
