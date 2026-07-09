//! スタックベース VM 本体。
//!
//! 「明示スタック」設計: 関数呼び出しは Rust の再帰呼び出しを使わず、
//! `Vm.frames: Vec<Frame>`（ヒープ上の `Vec`）に `Frame` を push/pop する
//! ことで実現する。`Vm::run_until` はただの `loop { self.step() }` であり、
//! Rust のコールスタックの深さは「同時にネストしているユーザー関数呼び出しの
//! 個数」に依存しない（せいぜい `Vm::run_until` のネストに依存するだけで、
//! それはネイティブ関数がコールバックを呼ぶ場合など限られた場面のみ）。
//! これにより、T3 で「特定の命令境界（await 式など）で `run_until` を
//! 抜けて `Vm`（frames を含む）をまるごと保存し、後で再開する」という
//! 拡張が自然に可能になる。

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::bytecode::*;
use crate::value::*;

/// 1 関数呼び出し分のフレーム。すべてヒープ上（`Vm.frames` という `Vec` の要素）。
pub struct Frame {
    pub chunk: Rc<Chunk>,
    pub ip: usize,
    pub locals: Vec<Rc<RefCell<Value>>>,
    pub upvalues: Rc<Vec<Rc<RefCell<Value>>>>,
    pub stack: Vec<Value>,
    pub raw_args: Rc<Vec<Value>>,
}

impl Frame {
    fn new(
        chunk: Rc<Chunk>,
        upvalues: Rc<Vec<Rc<RefCell<Value>>>>,
        raw_args: Rc<Vec<Value>>,
    ) -> Self {
        let num_locals = chunk.num_locals;
        Frame {
            chunk,
            ip: 0,
            locals: (0..num_locals)
                .map(|_| Rc::new(RefCell::new(Value::Uninit)))
                .collect(),
            upvalues,
            stack: Vec::new(),
            raw_args,
        }
    }
}

/// ビルトインメソッドのディスパッチ先を区別するタグ（`builtins.rs` 参照）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum KindTag {
    Array,
    Str,
    Map,
    Set,
    Regex,
}

pub struct Vm {
    pub frames: Vec<Frame>,
    pub globals: HashMap<Rc<str>, Rc<RefCell<Value>>>,
    /// `console.log` 相当の出力フック。既定では標準出力へ。
    pub on_log: Box<dyn FnMut(&str)>,
    /// `Date` 等、非決定的な値をホストが注入するためのフック置き場。
    pub now_fn: Option<Rc<dyn Fn() -> f64>>,
    /// モジュールのトップレベルフレームが `Return` した瞬間に export 値を
    /// 確定させておくための一時置き場（フレームは Return 直後に pop されるため）。
    pending_module_exports: Option<Vec<(Rc<str>, Value)>>,
}

impl Vm {
    pub fn new() -> Self {
        let mut vm = Vm {
            frames: Vec::new(),
            globals: HashMap::new(),
            on_log: Box::new(|s: &str| println!("{s}")),
            now_fn: None,
            pending_module_exports: None,
        };
        crate::builtins::install(&mut vm);
        vm
    }

    pub fn register_native(&mut self, name: &str, arity: usize, f: NativeFn) {
        let v = Value::native_fn(name, arity, f);
        self.globals
            .insert(Rc::from(name), Rc::new(RefCell::new(v)));
    }

    pub fn define_global(&mut self, name: &str, value: Value) {
        self.globals
            .insert(Rc::from(name), Rc::new(RefCell::new(value)));
    }

    // =================================================================
    // モジュール実行
    // =================================================================

    /// モジュールのトップレベルチャンクを実行し、export された値の一覧を返す。
    pub fn run_module(&mut self, chunk: Rc<Chunk>) -> Result<Vec<(Rc<str>, Value)>, Value> {
        let frame = Frame::new(chunk, Rc::new(Vec::new()), Rc::new(Vec::new()));
        self.frames.push(frame);
        self.run_until(0)?;
        Ok(self.pending_module_exports.take().unwrap_or_default())
    }

    // =================================================================
    // 実行ループ
    // =================================================================

    /// `self.frames.len()` が `stop_depth` 以下になるまでステップ実行する。
    /// 例外は各フレームの例外テーブルで処理を試み、処理できなければ
    /// フレームを 1 つ捨てて外側へ伝播する。`stop_depth` より外側まで
    /// 伝播したら `Err` で返す。
    pub fn run_until(&mut self, stop_depth: usize) -> Result<Value, Value> {
        let mut last_value = Value::Undefined;
        loop {
            if self.frames.len() <= stop_depth {
                return Ok(last_value);
            }
            match self.step() {
                Ok(Some(v)) => {
                    // 関数が 1 つ完了した。まだこの run_until の管轄内
                    // （stop_depth より深い位置）にフレームが残っているなら、
                    // それは通常のバイトコード `Call`/`New` から戻ってきた
                    // ケースなので、呼び出し元フレームのオペランドスタックへ
                    // 戻り値を積む（`Call` 命令が期待している場所）。
                    // ここで frames が stop_depth まで縮んでいた場合は、
                    // ネイティブ関数からの `call_value`/`call_function_obj`
                    // 呼び出しが完了したケースであり、その「呼び出し元」は
                    // バイトコードではなく Rust 側で待っているので、
                    // スタックには積まず `last_value` として run_until の
                    // 戻り値にする。
                    if self.frames.len() > stop_depth {
                        self.frames.last_mut().unwrap().stack.push(v);
                    } else {
                        last_value = v;
                    }
                }
                Ok(None) => {}
                Err(thrown) => {
                    self.unwind(thrown, stop_depth)?;
                }
            }
        }
    }

    /// 例外を現在のフレームから外側へ向かって処理できる場所を探す。
    fn unwind(&mut self, thrown: Value, stop_depth: usize) -> Result<(), Value> {
        loop {
            if self.frames.len() <= stop_depth {
                return Err(thrown);
            }
            let frame = self.frames.last_mut().unwrap();
            let fault_ip = frame.ip.saturating_sub(1) as u32;
            let handler = frame
                .chunk
                .exception_table
                .iter()
                .filter(|e| e.try_start <= fault_ip && fault_ip < e.try_end)
                .min_by_key(|e| e.try_end - e.try_start)
                .map(|e| e.handler_addr);
            if let Some(addr) = handler {
                frame.stack.clear();
                frame.stack.push(thrown);
                frame.ip = addr as usize;
                return Ok(());
            }
            self.frames.pop();
        }
    }

    /// 1 命令だけ実行する。`Return` でフレームが完全に返った場合のみ
    /// その戻り値を `Ok(Some(v))` で伝える（呼び出し元 `run_until` のトップレベル
    /// 戻り値を組み立てるため）。
    fn step(&mut self) -> Result<Option<Value>, Value> {
        let frame = self.frames.last_mut().unwrap();
        let ip = frame.ip;
        let op = frame.chunk.code[ip];
        frame.ip = ip + 1;

        macro_rules! bin_num {
            ($f:expr) => {{
                let b = self.pop();
                let a = self.pop();
                let r = $f(to_number(&a), to_number(&b));
                self.push(Value::Number(r));
            }};
        }
        macro_rules! bin_cmp {
            ($f:expr) => {{
                let b = self.pop();
                let a = self.pop();
                let r = js_compare(&a, &b);
                self.push(Value::Bool(r.map($f).unwrap_or(false)));
            }};
        }
        macro_rules! bin_int {
            ($f:expr) => {{
                let b = self.pop();
                let a = self.pop();
                let ai = to_int32(&a);
                let bi = to_int32(&b);
                self.push(Value::Number($f(ai, bi) as f64));
            }};
        }

        match op {
            OpCode::Pop => {
                self.pop();
            }
            OpCode::Dup => {
                let v = self.top().clone();
                self.push(v);
            }
            OpCode::Swap => {
                let f = self.frames.last_mut().unwrap();
                let n = f.stack.len();
                f.stack.swap(n - 1, n - 2);
            }
            OpCode::LoadConst(idx) => {
                let v = self.frames.last().unwrap().chunk.const_value(idx);
                self.push(v);
            }
            OpCode::LoadUndefined => self.push(Value::Undefined),
            OpCode::LoadNull => self.push(Value::Null),
            OpCode::LoadTrue => self.push(Value::Bool(true)),
            OpCode::LoadFalse => self.push(Value::Bool(false)),
            OpCode::LoadThis => unreachable!("This は Ident 解決経由でコンパイルされます"),

            OpCode::GetLocal(slot) => {
                let cell = self.frames.last().unwrap().locals[slot as usize].clone();
                let v = cell.borrow().clone();
                if matches!(v, Value::Uninit) {
                    return Err(
                        self.reference_error("変数が初期化される前にアクセスされました（TDZ）")
                    );
                }
                self.push(v);
            }
            OpCode::SetLocal(slot) => {
                let v = self.top().clone();
                *self.frames.last().unwrap().locals[slot as usize].borrow_mut() = v;
            }
            OpCode::BindLocal(slot) => {
                let v = self.pop();
                *self.frames.last().unwrap().locals[slot as usize].borrow_mut() = v;
            }
            OpCode::GetUpvalue(idx) => {
                let cell = self.frames.last().unwrap().upvalues[idx as usize].clone();
                let v = cell.borrow().clone();
                if matches!(v, Value::Uninit) {
                    return Err(
                        self.reference_error("変数が初期化される前にアクセスされました（TDZ）")
                    );
                }
                self.push(v);
            }
            OpCode::SetUpvalue(idx) => {
                let v = self.top().clone();
                *self.frames.last().unwrap().upvalues[idx as usize].borrow_mut() = v;
            }
            OpCode::GetGlobal(cidx) => {
                let name = self.frames.last().unwrap().chunk.const_str(cidx);
                match self.globals.get(&name) {
                    Some(cell) => {
                        let v = cell.borrow().clone();
                        if matches!(v, Value::Uninit) {
                            return Err(self.reference_error(format!(
                                "'{name}' は初期化される前に参照されました"
                            )));
                        }
                        self.push(v);
                    }
                    None => {
                        return Err(self.reference_error(format!("'{name}' は定義されていません")))
                    }
                }
            }
            OpCode::SetGlobal(cidx) => {
                let name = self.frames.last().unwrap().chunk.const_str(cidx);
                let v = self.top().clone();
                match self.globals.get(&name) {
                    Some(cell) => *cell.borrow_mut() = v,
                    None => {
                        return Err(self.reference_error(format!("'{name}' は定義されていません")))
                    }
                }
            }
            OpCode::BindGlobal(cidx) => {
                let name = self.frames.last().unwrap().chunk.const_str(cidx);
                let v = self.pop();
                self.globals.insert(name, Rc::new(RefCell::new(v)));
            }

            OpCode::GetArg(idx) => {
                let v = self
                    .frames
                    .last()
                    .unwrap()
                    .raw_args
                    .get(idx as usize)
                    .cloned()
                    .unwrap_or(Value::Undefined);
                self.push(v);
            }
            OpCode::GetRestArgs(from) => {
                let args = &self.frames.last().unwrap().raw_args;
                let rest = if (from as usize) < args.len() {
                    args[from as usize..].to_vec()
                } else {
                    Vec::new()
                };
                self.push(Value::new_array(rest));
            }

            OpCode::GetProp(cidx) => {
                let name = self.frames.last().unwrap().chunk.const_str(cidx);
                let obj = self.pop();
                let v = self.get_prop(&obj, &name)?;
                self.push(v);
            }
            OpCode::SetProp(cidx) => {
                let name = self.frames.last().unwrap().chunk.const_str(cidx);
                let value = self.pop();
                let obj = self.pop();
                self.set_prop(&obj, &name, value.clone())?;
                self.push(value);
            }
            OpCode::GetIndex => {
                let key = self.pop();
                let obj = self.pop();
                let name = to_property_key_string(&key);
                let v = self.get_prop(&obj, &name)?;
                self.push(v);
            }
            OpCode::SetIndex => {
                let value = self.pop();
                let key = self.pop();
                let obj = self.pop();
                let name = to_property_key_string(&key);
                self.set_prop(&obj, &name, value.clone())?;
                self.push(value);
            }
            OpCode::DeleteProp(cidx) => {
                let name = self.frames.last().unwrap().chunk.const_str(cidx);
                let obj = self.pop();
                let ok = self.delete_prop(&obj, &name);
                self.push(Value::Bool(ok));
            }
            OpCode::DeleteIndex => {
                let key = self.pop();
                let obj = self.pop();
                let name = to_property_key_string(&key);
                let ok = self.delete_prop(&obj, &name);
                self.push(Value::Bool(ok));
            }
            OpCode::JumpIfNullish(addr) => {
                if self.top().is_nullish() {
                    self.pop();
                    self.push(Value::Undefined);
                    self.frames.last_mut().unwrap().ip = addr as usize;
                }
            }
            OpCode::JumpIfNullishCall(addr) => {
                if self.top().is_nullish() {
                    self.pop(); // callee
                    self.pop(); // this
                    self.push(Value::Undefined);
                    self.frames.last_mut().unwrap().ip = addr as usize;
                }
            }

            OpCode::Add => {
                let b = self.pop();
                let a = self.pop();
                self.push(js_add(&a, &b));
            }
            OpCode::Sub => bin_num!(|a: f64, b: f64| a - b),
            OpCode::Mul => bin_num!(|a: f64, b: f64| a * b),
            OpCode::Div => bin_num!(|a: f64, b: f64| a / b),
            OpCode::Mod => bin_num!(|a: f64, b: f64| a % b),
            OpCode::Pow => bin_num!(|a: f64, b: f64| a.powf(b)),
            OpCode::Eq => {
                let b = self.pop();
                let a = self.pop();
                self.push(Value::Bool(loose_eq(&a, &b)));
            }
            OpCode::NotEq => {
                let b = self.pop();
                let a = self.pop();
                self.push(Value::Bool(!loose_eq(&a, &b)));
            }
            OpCode::StrictEq => {
                let b = self.pop();
                let a = self.pop();
                self.push(Value::Bool(strict_eq(&a, &b)));
            }
            OpCode::StrictNotEq => {
                let b = self.pop();
                let a = self.pop();
                self.push(Value::Bool(!strict_eq(&a, &b)));
            }
            OpCode::Lt => bin_cmp!(|o: std::cmp::Ordering| o == std::cmp::Ordering::Less),
            OpCode::Gt => bin_cmp!(|o: std::cmp::Ordering| o == std::cmp::Ordering::Greater),
            OpCode::LtEq => bin_cmp!(|o: std::cmp::Ordering| o != std::cmp::Ordering::Greater),
            OpCode::GtEq => bin_cmp!(|o: std::cmp::Ordering| o != std::cmp::Ordering::Less),
            OpCode::Shl => bin_int!(|a: i32, b: i32| a.wrapping_shl(b as u32 & 31)),
            OpCode::Shr => bin_int!(|a: i32, b: i32| a.wrapping_shr(b as u32 & 31)),
            OpCode::UShr => {
                let b = self.pop();
                let a = self.pop();
                let au = to_uint32(&a);
                let bu = to_int32(&b) as u32 & 31;
                self.push(Value::Number((au.wrapping_shr(bu)) as f64));
            }
            OpCode::BitAnd => bin_int!(|a: i32, b: i32| a & b),
            OpCode::BitOr => bin_int!(|a: i32, b: i32| a | b),
            OpCode::BitXor => bin_int!(|a: i32, b: i32| a ^ b),
            OpCode::Instanceof => {
                let b = self.pop();
                let a = self.pop();
                let r = self.op_instanceof(&a, &b)?;
                self.push(r);
            }
            OpCode::In => {
                let obj = self.pop();
                let key = self.pop();
                let Value::Object(o) = &obj else {
                    return Err(
                        self.type_error("in 演算子の右辺はオブジェクトである必要があります")
                    );
                };
                let name = to_property_key_string(&key);
                self.push(Value::Bool(self.has_prop(o, &name)));
            }
            OpCode::Neg => {
                let v = self.pop();
                self.push(Value::Number(-to_number(&v)));
            }
            OpCode::Pos => {
                let v = self.pop();
                self.push(Value::Number(to_number(&v)));
            }
            OpCode::Not => {
                let v = self.pop();
                self.push(Value::Bool(!v.to_bool()));
            }
            OpCode::BitNot => {
                let v = self.pop();
                self.push(Value::Number(!to_int32(&v) as f64));
            }
            OpCode::TypeOf => {
                let v = self.pop();
                self.push(Value::str(v.type_of()));
            }
            OpCode::TypeOfGlobal(cidx) => {
                let name = self.frames.last().unwrap().chunk.const_str(cidx);
                let s = match self.globals.get(&name) {
                    Some(cell) => {
                        let v = cell.borrow().clone();
                        if matches!(v, Value::Uninit) {
                            "undefined"
                        } else {
                            v.type_of()
                        }
                    }
                    None => "undefined",
                };
                self.push(Value::str(s));
            }
            OpCode::ToStringOp => {
                let v = self.pop();
                let s = self.to_display_string(&v);
                self.push(Value::Str(s.into()));
            }

            // 以下 3 命令は「条件を満たせば値を保持したままジャンプ、
            // 満たさなければ pop せずフォールスルーする」。フォールスルー後に
            // 値を捨てるかどうかはコンパイラ側が明示的な `Pop` を発行して
            // 決める（`compile_logical`/`compile_assign` 参照）。
            OpCode::JumpIfFalsyKeep(addr) => {
                if !self.top().to_bool() {
                    self.frames.last_mut().unwrap().ip = addr as usize;
                }
            }
            OpCode::JumpIfTruthyKeep(addr) => {
                if self.top().to_bool() {
                    self.frames.last_mut().unwrap().ip = addr as usize;
                }
            }
            OpCode::JumpIfNotNullishKeep(addr) => {
                if !self.top().is_nullish() {
                    self.frames.last_mut().unwrap().ip = addr as usize;
                }
            }
            // これは唯一「フォールスルー時は自身で pop する」命令
            // （デフォルト値/デフォルト引数の評価に使われ、呼び出し側は
            // 明示的な Pop を発行しない設計のため）。
            OpCode::JumpIfNotUndefinedKeep(addr) => {
                if !matches!(self.top(), Value::Undefined) {
                    self.frames.last_mut().unwrap().ip = addr as usize;
                } else {
                    self.pop();
                }
            }

            OpCode::Jump(addr) => self.frames.last_mut().unwrap().ip = addr as usize,
            OpCode::JumpIfFalse(addr) => {
                let v = self.pop();
                if !v.to_bool() {
                    self.frames.last_mut().unwrap().ip = addr as usize;
                }
            }
            OpCode::JumpIfTrue(addr) => {
                let v = self.pop();
                if v.to_bool() {
                    self.frames.last_mut().unwrap().ip = addr as usize;
                }
            }
            OpCode::Return => {
                let v = self.pop();
                if self.frames.len() == 1 {
                    // モジュールのトップレベルフレームが終了する。export 値を
                    // pop してしまう前にここで確定させておく（`run_module` 参照）。
                    let frame = self.frames.last().unwrap();
                    let exports = frame
                        .chunk
                        .exports
                        .iter()
                        .map(|(name, slot)| {
                            (name.clone(), frame.locals[*slot as usize].borrow().clone())
                        })
                        .collect();
                    self.pending_module_exports = Some(exports);
                }
                self.frames.pop();
                return Ok(Some(v));
            }
            OpCode::Throw => {
                let v = self.pop();
                return Err(v);
            }

            OpCode::MakeClosure(cidx) => {
                let tpl = {
                    let f = self.frames.last().unwrap();
                    match &f.chunk.consts[cidx as usize] {
                        Const::FunctionTemplate(t) => t.clone(),
                        _ => panic!("MakeClosure: 定数がFunctionTemplateではありません"),
                    }
                };
                let fo = self.build_closure(&tpl);
                self.push(wrap_function(fo));
            }
            OpCode::Call(argc) => self.exec_call(argc as usize)?,
            OpCode::CallSpread => {
                let args_arr = self.pop();
                let callee = self.pop();
                let this = self.pop();
                let args = array_values(&args_arr);
                let r = self.call_value(callee, this, &args)?;
                self.push(r);
            }
            OpCode::New(argc) => self.exec_new(argc as usize)?,
            OpCode::NewSpread => {
                let args_arr = self.pop();
                let callee = self.pop();
                let args = array_values(&args_arr);
                let r = self.dispatch_new(callee, args)?;
                self.push(r);
            }
            OpCode::NewObject => self.push(Value::new_plain_object()),
            OpCode::NewArray => self.push(Value::new_array(Vec::new())),
            OpCode::ArrayPush => {
                let v = self.pop();
                let arr = self.top().clone();
                if let Value::Object(o) = &arr {
                    if let ObjKind::Array(items) = &mut o.borrow_mut().kind {
                        items.push(v);
                    }
                }
            }
            OpCode::SpreadAppendArray => {
                let src = self.pop();
                let items = self.materialize_iter_array(&src)?;
                let arr = self.top().clone();
                if let Value::Object(o) = &arr {
                    if let ObjKind::Array(dst) = &mut o.borrow_mut().kind {
                        dst.extend(items);
                    }
                }
            }
            OpCode::ObjectSetProp(cidx) => {
                let name = self.frames.last().unwrap().chunk.const_str(cidx);
                let v = self.pop();
                let obj = self.top().clone();
                if let Value::Object(o) = &obj {
                    o.borrow_mut().props.set_value(name, v);
                }
            }
            OpCode::ObjectSetComputed => {
                let v = self.pop();
                let key = self.pop();
                let obj = self.top().clone();
                if let Value::Object(o) = &obj {
                    let name = to_property_key_string(&key);
                    o.borrow_mut().props.set_value(Rc::from(name.as_str()), v);
                }
            }
            OpCode::ObjectSetMethod(cidx) => {
                let name = self.frames.last().unwrap().chunk.const_str(cidx);
                let f = self.pop();
                let obj = self.top().clone();
                if let Value::Object(o) = &obj {
                    o.borrow_mut().props.set_value(name, f);
                }
            }
            OpCode::ObjectDefineGetter(cidx) => {
                let name = self.frames.last().unwrap().chunk.const_str(cidx);
                let f = self.pop();
                let obj = self.top().clone();
                if let Value::Object(o) = &obj {
                    o.borrow_mut().props.define_accessor(name, true, f);
                }
            }
            OpCode::ObjectDefineSetter(cidx) => {
                let name = self.frames.last().unwrap().chunk.const_str(cidx);
                let f = self.pop();
                let obj = self.top().clone();
                if let Value::Object(o) = &obj {
                    o.borrow_mut().props.define_accessor(name, false, f);
                }
            }
            OpCode::ObjectSpread => {
                let src = self.pop();
                let obj = self.top().clone();
                if let (Value::Object(dst), Value::Object(src_o)) = (&obj, &src) {
                    let entries: Vec<(Rc<str>, Value)> = src_o
                        .borrow()
                        .props
                        .iter()
                        .filter_map(|(k, s)| match s {
                            PropSlot::Value(v) => Some((k.clone(), v.clone())),
                            PropSlot::Accessor { .. } => None,
                        })
                        .collect();
                    for (k, v) in entries {
                        dst.borrow_mut().props.set_value(k, v);
                    }
                    if let ObjKind::Array(items) = &src_o.borrow().kind {
                        for (i, it) in items.iter().enumerate() {
                            dst.borrow_mut()
                                .props
                                .set_value(Rc::from(i.to_string().as_str()), it.clone());
                        }
                    }
                }
            }
            OpCode::MakeIterArray => {
                let v = self.pop();
                let items = self.materialize_iter_array(&v)?;
                self.push(Value::new_array(items));
            }
            OpCode::ObjectKeysArray => {
                let v = self.pop();
                let keys = self.own_keys_array(&v);
                self.push(Value::new_array(keys));
            }
            OpCode::ObjectRestExclude(n) => {
                let mut exclude = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    exclude.push(to_property_key_string(&self.pop()));
                }
                let src = self.pop();
                let mut out = ObjData::new(ObjKind::Plain);
                if let Value::Object(o) = &src {
                    for (k, s) in o.borrow().props.iter() {
                        if exclude.iter().any(|e| e == &**k) {
                            continue;
                        }
                        if let PropSlot::Value(v) = s {
                            out.props.set_value(k.clone(), v.clone());
                        }
                    }
                }
                self.push(Value::Object(Rc::new(RefCell::new(out))));
            }
            OpCode::ArraySliceFrom => {
                let start = self.pop();
                let arr = self.pop();
                let start_idx = to_number(&start) as usize;
                let items = array_values(&arr);
                let sliced = if start_idx < items.len() {
                    items[start_idx..].to_vec()
                } else {
                    Vec::new()
                };
                self.push(Value::new_array(sliced));
            }

            OpCode::MakeClass(cidx) => {
                let tpl = {
                    let f = self.frames.last().unwrap();
                    match &f.chunk.consts[cidx as usize] {
                        Const::ClassTemplate(t) => t.clone(),
                        _ => panic!("MakeClass: 定数が ClassTemplate ではありません"),
                    }
                };
                let v = self.build_class(&tpl)?;
                self.push(v);
            }
            OpCode::LoadRegex(cidx) => {
                let re = {
                    let f = self.frames.last().unwrap();
                    match &f.chunk.consts[cidx as usize] {
                        Const::Regex(r) => r.clone(),
                        _ => panic!("LoadRegex: 定数が Regex ではありません"),
                    }
                };
                let o = ObjData::new(ObjKind::Regex(re));
                self.push(Value::Object(Rc::new(RefCell::new(o))));
            }
            OpCode::Await => {
                let v = self.pop();
                if is_thenable(&v) {
                    return Err(self.type_error(
                        "await によるサスペンドは T3 で実装予定です（T2 は同期実行のみ）",
                    ));
                }
                self.push(v);
            }
        }
        Ok(None)
    }

    // =================================================================
    // スタックヘルパー
    // =================================================================

    fn push(&mut self, v: Value) {
        self.frames.last_mut().unwrap().stack.push(v);
    }
    fn pop(&mut self) -> Value {
        self.frames
            .last_mut()
            .unwrap()
            .stack
            .pop()
            .expect("オペランドスタックが空です")
    }
    fn top(&self) -> &Value {
        self.frames
            .last()
            .unwrap()
            .stack
            .last()
            .expect("オペランドスタックが空です")
    }

    // =================================================================
    // 呼び出し / 生成
    // =================================================================

    fn build_closure(&self, tpl: &Rc<FunctionTemplate>) -> Rc<FunctionObj> {
        let frame = self.frames.last().unwrap();
        let mut ups = Vec::with_capacity(tpl.upvalues.len());
        for d in &tpl.upvalues {
            let cell = match d {
                UpvalueDesc::ParentLocal(slot) => frame.locals[*slot as usize].clone(),
                UpvalueDesc::ParentUpvalue(idx) => frame.upvalues[*idx as usize].clone(),
            };
            ups.push(cell);
        }
        Rc::new(FunctionObj {
            name: tpl.name.clone(),
            chunk: tpl.chunk.clone(),
            upvalues: Rc::new(ups),
            is_arrow: tpl.is_arrow,
        })
    }

    fn build_class(&mut self, tpl: &Rc<ClassTemplate>) -> Result<Value, Value> {
        let ctor = tpl.ctor.as_ref().map(|c| self.build_closure(c));
        let mut proto = ObjData::new(ObjKind::Plain);
        for m in &tpl.methods {
            if m.is_static {
                continue;
            }
            let fo = self.build_closure(&m.func);
            proto.props.set_value(m.name.clone(), wrap_function(fo));
        }
        for a in &tpl.accessors {
            if a.is_static {
                continue;
            }
            let fo = self.build_closure(&a.func);
            proto
                .props
                .define_accessor(a.name.clone(), a.is_get, wrap_function(fo));
        }
        let proto_rc: ObjRef = Rc::new(RefCell::new(proto));

        let instance_fields: Vec<(Rc<str>, Option<Rc<FunctionObj>>)> = tpl
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
        let class_rc: ObjRef = Rc::new(RefCell::new(ObjData::new(ObjKind::Class(class_data))));
        proto_rc
            .borrow_mut()
            .props
            .set_value(Rc::from("constructor"), Value::Object(class_rc.clone()));
        class_rc
            .borrow_mut()
            .props
            .set_value(Rc::from("name"), Value::Str(tpl.name.clone()));
        class_rc
            .borrow_mut()
            .props
            .set_value(Rc::from("prototype"), Value::Object(proto_rc.clone()));

        for m in &tpl.methods {
            if !m.is_static {
                continue;
            }
            let fo = self.build_closure(&m.func);
            class_rc
                .borrow_mut()
                .props
                .set_value(m.name.clone(), wrap_function(fo));
        }
        for a in &tpl.accessors {
            if !a.is_static {
                continue;
            }
            let fo = self.build_closure(&a.func);
            class_rc.borrow_mut().props.define_accessor(
                a.name.clone(),
                a.is_get,
                wrap_function(fo),
            );
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

    fn exec_call(&mut self, argc: usize) -> Result<(), Value> {
        let frame = self.frames.last_mut().unwrap();
        let n = frame.stack.len();
        let args: Vec<Value> = frame.stack.split_off(n - argc);
        let callee = frame.stack.pop().unwrap();
        let this = frame.stack.pop().unwrap();
        match self.dispatch_call(callee, this, args)? {
            CallOutcome::Immediate(v) => self.push(v),
            CallOutcome::FramePushed => {}
        }
        Ok(())
    }

    fn exec_new(&mut self, argc: usize) -> Result<(), Value> {
        let frame = self.frames.last_mut().unwrap();
        let n = frame.stack.len();
        let args: Vec<Value> = frame.stack.split_off(n - argc);
        let callee = frame.stack.pop().unwrap();
        let v = self.dispatch_new(callee, args)?;
        self.push(v);
        Ok(())
    }

    /// 値の呼び出し。ネイティブは即座に実行し、バイトコード関数は
    /// フレームを push して現在の実行ループに委ねる（Rust 再帰を使わない）。
    pub fn call_value(
        &mut self,
        callee: Value,
        this: Value,
        args: &[Value],
    ) -> Result<Value, Value> {
        let depth_before = self.frames.len();
        match self.dispatch_call(callee, this, args.to_vec())? {
            CallOutcome::Immediate(v) => Ok(v),
            CallOutcome::FramePushed => self.run_until(depth_before),
        }
    }

    pub fn call_function_obj(
        &mut self,
        f: Rc<FunctionObj>,
        this: Value,
        args: &[Value],
    ) -> Result<Value, Value> {
        let depth_before = self.frames.len();
        self.push_function_frame(&f, this, args.to_vec());
        self.run_until(depth_before)
    }

    fn push_function_frame(&mut self, f: &Rc<FunctionObj>, this: Value, args: Vec<Value>) {
        let raw_args = Rc::new(args);
        let mut frame = Frame::new(f.chunk.clone(), f.upvalues.clone(), raw_args);
        if !f.is_arrow && !frame.locals.is_empty() {
            frame.locals[0] = Rc::new(RefCell::new(this));
        }
        self.frames.push(frame);
    }

    fn dispatch_call(
        &mut self,
        callee: Value,
        this: Value,
        args: Vec<Value>,
    ) -> Result<CallOutcome, Value> {
        match &callee {
            Value::Object(o) => {
                let kind_is_fn = matches!(o.borrow().kind, ObjKind::Function(_));
                if kind_is_fn {
                    let fo = match &o.borrow().kind {
                        ObjKind::Function(f) => f.clone(),
                        _ => unreachable!(),
                    };
                    self.push_function_frame(&fo, this, args);
                    return Ok(CallOutcome::FramePushed);
                }
                let is_native = matches!(o.borrow().kind, ObjKind::Native(_));
                if is_native {
                    let nf = match &o.borrow().kind {
                        ObjKind::Native(n) => n.f.clone(),
                        _ => unreachable!(),
                    };
                    let v = nf(self, this, &args)?;
                    return Ok(CallOutcome::Immediate(v));
                }
                if matches!(o.borrow().kind, ObjKind::Class(_)) {
                    return Err(
                        self.type_error("クラスのコンストラクタは 'new' なしで呼び出せません")
                    );
                }
                Err(self.type_error("これは関数ではありません"))
            }
            _ => Err(self.type_error("これは関数ではありません")),
        }
    }

    fn dispatch_new(&mut self, callee: Value, args: Vec<Value>) -> Result<Value, Value> {
        let Value::Object(co) = &callee else {
            return Err(self.type_error("これはコンストラクタではありません"));
        };
        let is_class = matches!(co.borrow().kind, ObjKind::Class(_));
        if is_class {
            let (ctor, proto, fields) = {
                let b = co.borrow();
                let ObjKind::Class(cd) = &b.kind else {
                    unreachable!()
                };
                (
                    cd.ctor.clone(),
                    cd.prototype.clone(),
                    cd.instance_fields.clone(),
                )
            };
            let instance = ObjData {
                kind: ObjKind::Plain,
                props: PropMap::new(),
                proto: Some(proto),
            };
            let inst_rc: ObjRef = Rc::new(RefCell::new(instance));
            let inst_val = Value::Object(inst_rc.clone());
            for (name, init) in &fields {
                let v = match init {
                    Some(f) => self.call_function_obj(f.clone(), inst_val.clone(), &[])?,
                    None => Value::Undefined,
                };
                inst_rc.borrow_mut().props.set_value(name.clone(), v);
            }
            if let Some(ctor) = ctor {
                let ret = self.call_function_obj(ctor, inst_val.clone(), &args)?;
                if matches!(ret, Value::Object(_)) {
                    return Ok(ret);
                }
            }
            return Ok(inst_val);
        }
        let is_native = matches!(co.borrow().kind, ObjKind::Native(_));
        if is_native {
            let nf = match &co.borrow().kind {
                ObjKind::Native(n) => n.f.clone(),
                _ => unreachable!(),
            };
            return nf(self, Value::Undefined, &args);
        }
        let is_fn = matches!(co.borrow().kind, ObjKind::Function(_));
        if is_fn {
            let fo = match &co.borrow().kind {
                ObjKind::Function(f) => f.clone(),
                _ => unreachable!(),
            };
            let proto = match co.borrow().props.get("prototype") {
                Some(PropSlot::Value(Value::Object(p))) => Some(p.clone()),
                _ => None,
            };
            let inst_rc: ObjRef = Rc::new(RefCell::new(ObjData {
                kind: ObjKind::Plain,
                props: PropMap::new(),
                proto,
            }));
            let inst_val = Value::Object(inst_rc.clone());
            let ret = self.call_function_obj(fo, inst_val.clone(), &args)?;
            if matches!(ret, Value::Object(_)) {
                return Ok(ret);
            }
            return Ok(inst_val);
        }
        Err(self.type_error("これはコンストラクタではありません"))
    }

    // =================================================================
    // プロパティアクセス
    // =================================================================

    pub fn get_prop(&mut self, obj: &Value, name: &str) -> Result<Value, Value> {
        match obj {
            Value::Undefined | Value::Null | Value::Uninit => Err(self.type_error(format!(
                "'{}' のプロパティ '{name}' は読み取れません（undefined/null）",
                if matches!(obj, Value::Null) {
                    "null"
                } else {
                    "undefined"
                }
            ))),
            Value::Str(s) => crate::builtins::string_get_prop(self, s, name),
            Value::Number(_) | Value::Bool(_) => Ok(Value::Undefined),
            Value::Object(o) => self.get_prop_object(o, o, name),
        }
    }

    /// `receiver` はプロパティアクセスの元になったオブジェクト（getter 呼び出し
    /// 時の `this` になる）、`obj_rc` はプロトタイプチェーンを辿る過程で
    /// 「いま自前プロパティを探している対象」。両者は再帰の最初は同じだが、
    /// プロトタイプへ委譲するにつれて `obj_rc` だけが変わっていく
    /// （getter/setter の `this` を常に本来の受信者に保つため必須の区別）。
    fn get_prop_object(
        &mut self,
        receiver: &ObjRef,
        obj_rc: &ObjRef,
        name: &str,
    ) -> Result<Value, Value> {
        let own = obj_rc.borrow().props.get(name).cloned();
        if let Some(slot) = own {
            return match slot {
                PropSlot::Value(v) => Ok(v),
                PropSlot::Accessor { get, .. } => match get {
                    Some(f) => self.call_value(f, Value::Object(receiver.clone()), &[]),
                    None => Ok(Value::Undefined),
                },
            };
        }
        let kind_hit = {
            let b = obj_rc.borrow();
            match &b.kind {
                ObjKind::Array(items) => {
                    if name == "length" {
                        Some(Value::Number(items.len() as f64))
                    } else if let Ok(idx) = name.parse::<usize>() {
                        Some(items.get(idx).cloned().unwrap_or(Value::Undefined))
                    } else {
                        None
                    }
                }
                ObjKind::Map(m) => (name == "size").then(|| Value::Number(m.entries.len() as f64)),
                ObjKind::Set(s) => (name == "size").then(|| Value::Number(s.values.len() as f64)),
                ObjKind::Regex(re) => match name {
                    "source" => Some(Value::str(re.source.clone())),
                    "flags" => Some(Value::str(re.flags.clone())),
                    _ => None,
                },
                _ => None,
            }
        };
        if let Some(v) = kind_hit {
            return Ok(v);
        }
        let tag = {
            let b = obj_rc.borrow();
            match &b.kind {
                ObjKind::Array(_) => Some(KindTag::Array),
                ObjKind::Map(_) => Some(KindTag::Map),
                ObjKind::Set(_) => Some(KindTag::Set),
                ObjKind::Regex(_) => Some(KindTag::Regex),
                _ => None,
            }
        };
        if let Some(tag) = tag {
            if let Some(f) = crate::builtins::method_for(tag, name) {
                return Ok(f);
            }
        }
        let proto = obj_rc.borrow().proto.clone();
        if let Some(p) = proto {
            return self.get_prop_object(receiver, &p, name);
        }
        Ok(Value::Undefined)
    }

    pub fn set_prop(&mut self, obj: &Value, name: &str, value: Value) -> Result<(), Value> {
        let Value::Object(o) = obj else {
            if obj.is_nullish() {
                return Err(self.type_error(format!(
                    "プロパティ '{name}' を設定できません（undefined/null）"
                )));
            }
            return Ok(()); // プリミティブへの代入は黙って無視（strict mode の細部は簡略化）。
        };
        self.set_prop_object(o, name, value)
    }

    fn set_prop_object(&mut self, obj_rc: &ObjRef, name: &str, value: Value) -> Result<(), Value> {
        {
            let mut b = obj_rc.borrow_mut();
            if let Some(slot) = b.props.get_mut(name) {
                match slot {
                    PropSlot::Value(v) => {
                        *v = value;
                        return Ok(());
                    }
                    PropSlot::Accessor { set, .. } => {
                        let s = set.clone();
                        drop(b);
                        if let Some(f) = s {
                            self.call_value(f, Value::Object(obj_rc.clone()), &[value])?;
                        }
                        return Ok(());
                    }
                }
            }
        }
        {
            let mut b = obj_rc.borrow_mut();
            if let ObjKind::Array(items) = &mut b.kind {
                if name == "length" {
                    let n = to_number(&value).max(0.0) as usize;
                    items.resize(n, Value::Undefined);
                    return Ok(());
                } else if let Ok(idx) = name.parse::<usize>() {
                    if idx >= items.len() {
                        items.resize(idx + 1, Value::Undefined);
                    }
                    items[idx] = value;
                    return Ok(());
                }
            }
        }
        let proto = obj_rc.borrow().proto.clone();
        if let Some(p) = &proto {
            if let Some(setter) = self.find_proto_accessor_setter(p, name) {
                self.call_value(setter, Value::Object(obj_rc.clone()), &[value])?;
                return Ok(());
            }
        }
        obj_rc.borrow_mut().props.set_value(Rc::from(name), value);
        Ok(())
    }

    fn find_proto_accessor_setter(&self, obj_rc: &ObjRef, name: &str) -> Option<Value> {
        let b = obj_rc.borrow();
        if let Some(PropSlot::Accessor { set, .. }) = b.props.get(name) {
            return set.clone();
        }
        if b.props.get(name).is_some() {
            return None; // 通常のデータプロパティが継承元にあるだけなら own を作る。
        }
        let proto = b.proto.clone();
        drop(b);
        proto.and_then(|p| self.find_proto_accessor_setter(&p, name))
    }

    fn delete_prop(&mut self, obj: &Value, name: &str) -> bool {
        if let Value::Object(o) = obj {
            if let ObjKind::Array(items) = &mut o.borrow_mut().kind {
                if let Ok(idx) = name.parse::<usize>() {
                    if idx < items.len() {
                        items[idx] = Value::Undefined;
                        return true;
                    }
                }
            }
            return o.borrow_mut().props.delete(name);
        }
        true
    }

    pub fn has_prop(&self, o: &ObjRef, name: &str) -> bool {
        let b = o.borrow();
        if b.props.get(name).is_some() {
            return true;
        }
        if let ObjKind::Array(items) = &b.kind {
            if name == "length" {
                return true;
            }
            if let Ok(i) = name.parse::<usize>() {
                return i < items.len();
            }
        }
        if let Some(p) = &b.proto {
            return self.has_prop(p, name);
        }
        false
    }

    fn own_keys_array(&self, v: &Value) -> Vec<Value> {
        match v {
            Value::Object(o) => {
                let b = o.borrow();
                match &b.kind {
                    ObjKind::Array(items) => (0..items.len())
                        .map(|i| Value::str(i.to_string()))
                        .collect(),
                    _ => b
                        .props
                        .iter()
                        .filter(|(_, s)| matches!(s, PropSlot::Value(_)))
                        .map(|(k, _)| Value::Str(k.clone()))
                        .collect(),
                }
            }
            _ => Vec::new(),
        }
    }

    /// for-of / スプレッドのための実体化。Array はそのまま、String は
    /// 1 文字ずつの配列、Map は `[key, value]` の配列、Set は値の配列にする。
    pub fn materialize_iter_array(&mut self, v: &Value) -> Result<Vec<Value>, Value> {
        match v {
            Value::Object(o) => {
                let b = o.borrow();
                match &b.kind {
                    ObjKind::Array(items) => Ok(items.clone()),
                    ObjKind::Map(m) => Ok(m
                        .entries
                        .iter()
                        .map(|(k, val)| Value::new_array(vec![k.clone(), val.clone()]))
                        .collect()),
                    ObjKind::Set(s) => Ok(s.values.clone()),
                    _ => Err(self.type_error(
                        "この値は反復できません（for...of/スプレッド対象ではありません）",
                    )),
                }
            }
            Value::Str(s) => Ok(s.chars().map(|c| Value::str(c.to_string())).collect()),
            _ => {
                Err(self
                    .type_error("この値は反復できません（for...of/スプレッド対象ではありません）"))
            }
        }
    }

    fn op_instanceof(&self, left: &Value, right: &Value) -> Result<Value, Value> {
        let proto_target = match right {
            Value::Object(o) => {
                let b = o.borrow();
                match &b.kind {
                    ObjKind::Class(cd) => Some(cd.prototype.clone()),
                    _ => match b.props.get("prototype") {
                        Some(PropSlot::Value(Value::Object(p))) => Some(p.clone()),
                        _ => None,
                    },
                }
            }
            _ => None,
        };
        let Some(proto) = proto_target else {
            return Err(self.type_error("instanceof の右辺はコンストラクタではありません"));
        };
        let Value::Object(lo) = left else {
            return Ok(Value::Bool(false));
        };
        let mut cur = lo.borrow().proto.clone();
        while let Some(p) = cur {
            if Rc::ptr_eq(&p, &proto) {
                return Ok(Value::Bool(true));
            }
            cur = p.borrow().proto.clone();
        }
        Ok(Value::Bool(false))
    }

    // =================================================================
    // 変換・エラー生成
    // =================================================================

    pub fn to_display_string(&mut self, v: &Value) -> String {
        to_string_basic(v)
    }

    pub fn make_error(&self, name: &str, msg: String) -> Value {
        let mut o = ObjData::new(ObjKind::Error);
        o.props.set_value(Rc::from("name"), Value::str(name));
        o.props
            .set_value(Rc::from("message"), Value::str(msg.clone()));
        o.props
            .set_value(Rc::from("stack"), Value::str(format!("{name}: {msg}")));
        Value::Object(Rc::new(RefCell::new(o)))
    }
    pub fn type_error(&self, msg: impl Into<String>) -> Value {
        self.make_error("TypeError", msg.into())
    }
    pub fn range_error(&self, msg: impl Into<String>) -> Value {
        self.make_error("RangeError", msg.into())
    }
    pub fn reference_error(&self, msg: impl Into<String>) -> Value {
        self.make_error("ReferenceError", msg.into())
    }
}

impl Default for Vm {
    fn default() -> Self {
        Self::new()
    }
}

enum CallOutcome {
    Immediate(Value),
    FramePushed,
}

fn wrap_function(fo: Rc<FunctionObj>) -> Value {
    Value::Object(Rc::new(RefCell::new(ObjData::new(ObjKind::Function(fo)))))
}

fn is_thenable(v: &Value) -> bool {
    if let Value::Object(o) = v {
        matches!(
            o.borrow().props.get("then"),
            Some(PropSlot::Value(Value::Object(_)))
        )
    } else {
        false
    }
}

pub fn array_values(v: &Value) -> Vec<Value> {
    if let Value::Object(o) = v {
        if let ObjKind::Array(items) = &o.borrow().kind {
            return items.clone();
        }
    }
    Vec::new()
}

// =====================================================================
// JS セマンティクス寄りの変換関数群（Vm を必要としない純粋関数）
// =====================================================================

pub fn to_number(v: &Value) -> f64 {
    match v {
        Value::Undefined | Value::Uninit => f64::NAN,
        Value::Null => 0.0,
        Value::Bool(b) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        Value::Number(n) => *n,
        Value::Str(s) => {
            let t = s.trim();
            if t.is_empty() {
                0.0
            } else if t == "Infinity" || t == "+Infinity" {
                f64::INFINITY
            } else if t == "-Infinity" {
                f64::NEG_INFINITY
            } else if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
                i64::from_str_radix(hex, 16)
                    .map(|n| n as f64)
                    .unwrap_or(f64::NAN)
            } else {
                t.parse::<f64>().unwrap_or(f64::NAN)
            }
        }
        Value::Object(_) => to_number_from_primitive(&to_primitive(v)),
    }
}

fn to_number_from_primitive(v: &Value) -> f64 {
    match v {
        Value::Object(_) => f64::NAN,
        other => to_number(other),
    }
}

pub fn to_int32(v: &Value) -> i32 {
    let n = to_number(v);
    if !n.is_finite() {
        return 0;
    }
    n as i64 as i32
}

pub fn to_uint32(v: &Value) -> u32 {
    let n = to_number(v);
    if !n.is_finite() {
        return 0;
    }
    n as i64 as u32
}

/// ToPrimitive（default/number ヒント相当。配列は join(",")、
/// プレーンオブジェクトは "[object Object]" とみなす簡略版）。
pub fn to_primitive(v: &Value) -> Value {
    match v {
        Value::Object(_) => Value::str(to_string_basic(v)),
        other => other.clone(),
    }
}

pub fn number_to_js_string(n: f64) -> String {
    if n.is_nan() {
        return "NaN".to_string();
    }
    if n.is_infinite() {
        return if n > 0.0 {
            "Infinity".to_string()
        } else {
            "-Infinity".to_string()
        };
    }
    if n == 0.0 {
        return "0".to_string();
    }
    if n.fract() == 0.0 && n.abs() < 1e15 {
        return format!("{}", n as i64);
    }
    let mut s = format!("{n}");
    if s.contains('e') {
        // Rust の指数表記と JS の表記差はコーパス規模では問題にならない前提。
        return s;
    }
    if !s.contains('.') {
        s.push_str(".0");
    }
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

pub fn to_string_basic(v: &Value) -> String {
    match v {
        Value::Undefined | Value::Uninit => "undefined".to_string(),
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => number_to_js_string(*n),
        Value::Str(s) => s.to_string(),
        Value::Object(o) => {
            let b = o.borrow();
            match &b.kind {
                ObjKind::Array(items) => items
                    .iter()
                    .map(|it| match it {
                        Value::Undefined | Value::Null | Value::Uninit => String::new(),
                        other => to_string_basic(other),
                    })
                    .collect::<Vec<_>>()
                    .join(","),
                ObjKind::Function(f) => format!("function {}() {{ [native code] }}", f.name),
                ObjKind::Native(n) => format!("function {}() {{ [native code] }}", n.name),
                ObjKind::Class(c) => format!("class {} {{ }}", c.name),
                ObjKind::Error => {
                    let name = match b.props.get("name") {
                        Some(PropSlot::Value(Value::Str(s))) => s.to_string(),
                        _ => "Error".to_string(),
                    };
                    let msg = match b.props.get("message") {
                        Some(PropSlot::Value(Value::Str(s))) => s.to_string(),
                        _ => String::new(),
                    };
                    if msg.is_empty() {
                        name
                    } else {
                        format!("{name}: {msg}")
                    }
                }
                ObjKind::Regex(re) => format!("/{}/{}", re.source, re.flags),
                ObjKind::Map(_) => "[object Map]".to_string(),
                ObjKind::Set(_) => "[object Set]".to_string(),
                ObjKind::Plain => "[object Object]".to_string(),
            }
        }
    }
}

pub fn to_property_key_string(v: &Value) -> String {
    match v {
        Value::Str(s) => s.to_string(),
        Value::Number(n) => number_to_js_string(*n),
        other => to_string_basic(other),
    }
}

fn js_add(a: &Value, b: &Value) -> Value {
    let pa = to_primitive(a);
    let pb = to_primitive(b);
    if matches!(pa, Value::Str(_)) || matches!(pb, Value::Str(_)) {
        Value::Str(format!("{}{}", to_string_basic(&pa), to_string_basic(&pb)).into())
    } else {
        Value::Number(to_number(&pa) + to_number(&pb))
    }
}

/// `<` `>` `<=` `>=` 共通の比較。NaN が絡む場合は None（常に false になる）。
fn js_compare(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    let pa = to_primitive(a);
    let pb = to_primitive(b);
    if let (Value::Str(x), Value::Str(y)) = (&pa, &pb) {
        return Some(x.as_ref().cmp(y.as_ref()));
    }
    let x = to_number(&pa);
    let y = to_number(&pb);
    x.partial_cmp(&y)
}

/// `==` の主要規則（Symbol は非対応のため考慮しない）。
pub fn loose_eq(a: &Value, b: &Value) -> bool {
    use Value::*;
    match (a, b) {
        (Undefined | Null, Undefined | Null) => true,
        (Number(_), Number(_)) | (Str(_), Str(_)) | (Bool(_), Bool(_)) | (Object(_), Object(_)) => {
            strict_eq(a, b)
        }
        (Number(_), Str(_)) => to_number(a) == to_number(b),
        (Str(_), Number(_)) => to_number(a) == to_number(b),
        (Bool(_), _) => loose_eq(&Value::Number(to_number(a)), b),
        (_, Bool(_)) => loose_eq(a, &Value::Number(to_number(b))),
        (Object(_), Number(_) | Str(_)) => loose_eq(&to_primitive(a), b),
        (Number(_) | Str(_), Object(_)) => loose_eq(a, &to_primitive(b)),
        _ => false,
    }
}
