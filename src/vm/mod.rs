//! スタックベース VM 本体。
//!
//! ## サブモジュール構成
//! - `dispatch`: `run_until` バイトコードディスパッチループと例外巻き戻し (`unwind`)
//! - `call`: 関数/メソッド呼び出し・`new`・クロージャ/クラス生成
//! - `props`: プロパティ/添字アクセス・プロトタイプ探索・`instanceof`・イテレータ実体化
//! - `async_rt`: Promise・マイクロタスクキュー・`await` 中断/再開・イベントループ
//! - `gc`: マーク＋trial-deletion による循環参照 GC
//! - `ops`: JS 変換・算術/比較/等価演算の純粋関数群

mod async_rt;
mod call;
mod dispatch;
mod gc;
mod ops;
mod props;
#[cfg(test)]
mod tests;

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::{Rc, Weak};

use crate::bytecode::*;
use crate::value::*;

pub use ops::{
    array_values, is_callable, loose_eq, new_regex_value, number_to_js_string, to_int32, to_number,
    to_primitive, to_property_key_string, to_string_basic, to_uint32,
};
pub(crate) use ops::{number_to_js_string_into, to_string_into};
pub(crate) use props::own_enumerable_keys;

/// 自動循環参照 GC の最小トリガー閾値（新規オブジェクト確保数）。
pub(super) const GC_MIN_THRESHOLD: usize = 2048;
/// フレームプールの最大保持数。
const MAX_FRAME_POOL: usize = 64;
/// 引数バッファプールの最大保持数。
const MAX_ARGS_POOL: usize = 32;

thread_local! {
    /// ASCII 単一文字の `Rc<str>` キャッシュ（`str[i]` のゼロ確保高速パス用）。
    static ASCII_CHARS: [Rc<str>; 128] = std::array::from_fn(|b| {
        let ch = [b as u8];
        Rc::from(std::str::from_utf8(&ch).unwrap())
    });
}

#[inline]
pub(crate) fn ascii_char_rc(b: u8) -> Rc<str> {
    ASCII_CHARS.with(|tbl| tbl[b as usize].clone())
}

/// ローカル変数スロットの表現。
///
/// 内側クロージャにキャプチャされないローカル変数は `Direct(Value)` として直接保持し、
/// `Rc<RefCell<Value>>` のヒープ確保・参照カウント操作・borrow チェックを完全に省く。
#[derive(Clone)]
pub enum LocalSlot {
    Direct(Value),
    Boxed(Rc<RefCell<Value>>),
}

impl LocalSlot {
    #[inline]
    pub fn get(&self) -> Value {
        match self {
            LocalSlot::Direct(v) => v.clone(),
            LocalSlot::Boxed(cell) => cell.borrow().clone(),
        }
    }

    #[inline]
    pub fn set(&mut self, value: Value) {
        match self {
            LocalSlot::Direct(v) => *v = value,
            LocalSlot::Boxed(cell) => *cell.borrow_mut() = value,
        }
    }

    #[inline]
    pub fn is_direct(&self) -> bool {
        matches!(self, LocalSlot::Direct(_))
    }

    #[inline]
    pub fn is_boxed(&self) -> bool {
        matches!(self, LocalSlot::Boxed(_))
    }

    #[inline]
    pub fn as_boxed_cell(&self) -> Rc<RefCell<Value>> {
        match self {
            LocalSlot::Boxed(cell) => cell.clone(),
            LocalSlot::Direct(_) => {
                panic!("内部エラー: 非キャプチャ（Direct）のローカルスロットがクロージャに捕捉されました")
            }
        }
    }
}

/// 1 関数呼び出し分のフレーム。すべてヒープ上（`Vm.frames` という `Vec` の要素）。
pub struct Frame {
    pub chunk: Rc<Chunk>,
    pub ip: usize,
    pub locals: Vec<LocalSlot>,
    pub upvalues: Rc<Vec<Rc<RefCell<Value>>>>,
    pub stack: Vec<Value>,
    pub raw_args: Rc<Vec<Value>>,
}

impl Frame {
    pub(super) fn new(
        chunk: Rc<Chunk>,
        upvalues: Rc<Vec<Rc<RefCell<Value>>>>,
        raw_args: Rc<Vec<Value>>,
    ) -> Self {
        let num_locals = chunk.num_locals;
        let locals = if !chunk.has_captured_locals {
            vec![LocalSlot::Direct(Value::Uninit); num_locals]
        } else {
            (0..num_locals)
                .map(|i| {
                    if chunk.captured_locals.get(i).copied().unwrap_or(false) {
                        LocalSlot::Boxed(Rc::new(RefCell::new(Value::Uninit)))
                    } else {
                        LocalSlot::Direct(Value::Uninit)
                    }
                })
                .collect()
        };
        Frame {
            chunk,
            ip: 0,
            locals,
            upvalues,
            stack: Vec::new(),
            raw_args,
        }
    }

    #[inline]
    fn reinit_locals(&mut self) {
        let num_locals = self.chunk.num_locals;
        self.locals.clear();
        if !self.chunk.has_captured_locals {
            self.locals
                .resize(num_locals, LocalSlot::Direct(Value::Uninit));
        } else {
            self.locals.reserve(num_locals);
            for i in 0..num_locals {
                if self.chunk.captured_locals.get(i).copied().unwrap_or(false) {
                    self.locals
                        .push(LocalSlot::Boxed(Rc::new(RefCell::new(Value::Uninit))));
                } else {
                    self.locals.push(LocalSlot::Direct(Value::Uninit));
                }
            }
        }
    }
}

/// `Vm::run_until` の結果。
pub enum RunOutcome {
    Value(Value),
    Suspended,
}

/// ビルトインメソッドのディスパッチ先を区別するタグ（`builtins` 参照）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum KindTag {
    Array,
    Str,
    Map,
    Set,
    Regex,
    Promise,
}

/// T3: マイクロタスクキューに積まれる仕事の単位。
pub enum MicrotaskJob {
    Then {
        on_fulfilled: Option<Value>,
        on_rejected: Option<Value>,
        result_promise: ObjRef,
        outcome: Result<Value, Value>,
    },
    ResolveThenable {
        promise: ObjRef,
        thenable: Value,
        then_fn: Value,
    },
    ResumeAwait {
        cont: AwaitContinuation,
        outcome: Result<Value, Value>,
    },
}

/// `run_event_loop` の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PumpState {
    Idle,
    Waiting,
}

/// `Vm::start_module_task` の結果（T5: モジュールシステム）。
#[derive(Debug)]
pub enum ModuleTaskState {
    Settled(Vec<(Rc<str>, Value)>),
    Pending,
}

/// ホストが後から解決/棄却できる Promise のハンドル。
pub struct HostPromise {
    pub(super) promise: ObjRef,
}

impl HostPromise {
    pub fn resolve(&self, vm: &mut Vm, value: Value) {
        vm.settle_host_promise(self.promise.clone(), Ok(value));
    }
    pub fn reject(&self, vm: &mut Vm, value: Value) {
        vm.settle_host_promise(self.promise.clone(), Err(value));
    }
}

pub(super) enum CallOutcome {
    Immediate(Value),
    FramePushed,
}

/// 呼び出し元オペランドスタック上に `callee` スロットが積まれているか
/// （`OpCode::Call` は `[this, callee, args...]`、`OpCode::CallMethod` は `[obj, args...]`）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum StackCalleeSlot {
    Present,
    Omitted,
}

pub struct Vm {
    pub frames: Vec<Frame>,
    pub globals: HashMap<Rc<str>, Rc<RefCell<Value>>>,
    /// `console.log` 相当の出力フック。既定では標準出力へ。
    pub on_log: Box<dyn FnMut(&str)>,
    /// `Date` 等、非決定的な値をホストが注入するためのフック置き場。
    pub now_fn: Option<Rc<dyn Fn() -> f64>>,
    pub(super) pending_module_exports: Option<Vec<(Rc<str>, Value)>>,

    // ---------------- T3: Promise / イベントループ ----------------
    pub microtasks: VecDeque<MicrotaskJob>,
    pub(super) maybe_unhandled: Vec<ObjRef>,
    pub on_unhandled_rejection: Box<dyn FnMut(&Value)>,
    pub(super) pending_host_count: usize,
    pub(super) native_call_depth: u32,
    pub(super) current_owner_promise: Option<ObjRef>,

    // ---------------- 循環参照回収 (GC) ----------------
    pub(super) gc_objects: RefCell<Vec<Weak<RefCell<ObjData>>>>,
    pub(super) gc_allocs_since_last: Cell<usize>,
    pub(super) gc_threshold: usize,

    // ---------------- フレーム / 引数プール ----------------
    frame_pool: Vec<Frame>,
    args_pool: Vec<Vec<Value>>,
    empty_upvalues: Rc<Vec<Rc<RefCell<Value>>>>,
    empty_args: Rc<Vec<Value>>,
}

impl Vm {
    pub fn new() -> Self {
        let mut vm = Vm {
            frames: Vec::new(),
            globals: HashMap::new(),
            on_log: Box::new(|s: &str| println!("{s}")),
            now_fn: None,
            pending_module_exports: None,
            microtasks: VecDeque::new(),
            maybe_unhandled: Vec::new(),
            on_unhandled_rejection: Box::new(|_v: &Value| {}),
            pending_host_count: 0,
            native_call_depth: 0,
            current_owner_promise: None,
            gc_objects: RefCell::new(Vec::new()),
            gc_allocs_since_last: Cell::new(0),
            gc_threshold: GC_MIN_THRESHOLD,
            frame_pool: Vec::new(),
            args_pool: Vec::new(),
            empty_upvalues: Rc::new(Vec::new()),
            empty_args: Rc::new(Vec::new()),
        };
        crate::builtins::install(&mut vm);
        vm
    }

    // ---------------- プール・フレーム管理 ----------------

    #[inline]
    pub(super) fn take_args_buf(&mut self) -> Vec<Value> {
        self.args_pool.pop().unwrap_or_default()
    }

    #[inline]
    pub(super) fn put_args_buf(&mut self, mut buf: Vec<Value>) {
        buf.clear();
        if self.args_pool.len() < MAX_ARGS_POOL {
            self.args_pool.push(buf);
        }
    }

    #[inline]
    pub(super) fn release_frame(&mut self, mut frame: Frame) {
        frame.stack.clear();
        frame.locals.clear();
        if let Some(v) = Rc::get_mut(&mut frame.raw_args) {
            v.clear();
        } else {
            frame.raw_args = self.empty_args.clone();
        }
        frame.upvalues = self.empty_upvalues.clone();
        if self.frame_pool.len() < MAX_FRAME_POOL {
            self.frame_pool.push(frame);
        }
    }

    #[inline]
    pub(super) fn acquire_frame_from_slice(
        &mut self,
        f: &Rc<FunctionObj>,
        this: Value,
        args: &[Value],
    ) -> Frame {
        let needs_raw = f.chunk.needs_raw_args && !args.is_empty();
        let mut frame = if let Some(mut frame) = self.frame_pool.pop() {
            frame.chunk = f.chunk.clone();
            frame.ip = 0;
            frame.upvalues = f.upvalues.clone();
            frame.stack.clear();
            if needs_raw {
                if !Rc::ptr_eq(&frame.raw_args, &self.empty_args) {
                    if let Some(v) = Rc::get_mut(&mut frame.raw_args) {
                        v.clear();
                        v.extend_from_slice(args);
                    } else {
                        let mut v = self.take_args_buf();
                        v.extend_from_slice(args);
                        frame.raw_args = Rc::new(v);
                    }
                } else {
                    let mut v = self.take_args_buf();
                    v.extend_from_slice(args);
                    frame.raw_args = Rc::new(v);
                }
            } else if let Some(v) = Rc::get_mut(&mut frame.raw_args) {
                v.clear();
            } else {
                frame.raw_args = self.empty_args.clone();
            }
            frame.reinit_locals();
            frame
        } else {
            let raw_args = if needs_raw {
                let mut v = self.take_args_buf();
                v.extend_from_slice(args);
                Rc::new(v)
            } else {
                self.empty_args.clone()
            };
            Frame::new(f.chunk.clone(), f.upvalues.clone(), raw_args)
        };
        let param_base = if !f.is_arrow && !frame.locals.is_empty() {
            frame.locals[0].set(this);
            1usize
        } else {
            0usize
        };
        let sp = f.chunk.simple_param_count as usize;
        for i in 0..sp {
            let val = args.get(i).cloned().unwrap_or(Value::Undefined);
            frame.locals[param_base + i].set(val);
        }
        frame
    }

    #[inline]
    pub(super) fn push_frame_from_caller_stack(
        &mut self,
        fo: &Rc<FunctionObj>,
        argc: usize,
        callee_slot: StackCalleeSlot,
    ) {
        let needs_raw = fo.chunk.needs_raw_args && argc > 0;
        let mut frame = if let Some(mut frame) = self.frame_pool.pop() {
            frame.chunk = fo.chunk.clone();
            frame.ip = 0;
            frame.upvalues = fo.upvalues.clone();
            frame.stack.clear();
            frame.reinit_locals();
            frame
        } else {
            Frame::new(
                fo.chunk.clone(),
                fo.upvalues.clone(),
                self.empty_args.clone(),
            )
        };
        let sp = fo.chunk.simple_param_count as usize;
        let param_base = usize::from(!fo.is_arrow && !frame.locals.is_empty());

        let this = {
            let caller = self.frames.last_mut().unwrap();
            let n = caller.stack.len();
            let arg_start = n - argc;
            if needs_raw {
                let mut reused = false;
                if !Rc::ptr_eq(&frame.raw_args, &self.empty_args) {
                    if let Some(v) = Rc::get_mut(&mut frame.raw_args) {
                        v.clear();
                        v.extend(caller.stack.drain(arg_start..));
                        reused = true;
                    }
                }
                if !reused {
                    let mut v = self.args_pool.pop().unwrap_or_default();
                    v.extend(caller.stack.drain(arg_start..));
                    frame.raw_args = Rc::new(v);
                }
                for i in 0..sp {
                    let val = frame.raw_args.get(i).cloned().unwrap_or(Value::Undefined);
                    frame.locals[param_base + i].set(val);
                }
            } else {
                if !frame.raw_args.is_empty() {
                    if let Some(v) = Rc::get_mut(&mut frame.raw_args) {
                        v.clear();
                    } else {
                        frame.raw_args = self.empty_args.clone();
                    }
                }
                let direct_copy = argc.min(sp);
                for (i, val) in caller.stack.drain(arg_start..).enumerate() {
                    if i < direct_copy {
                        frame.locals[param_base + i].set(val);
                    }
                }
                for i in direct_copy..sp {
                    frame.locals[param_base + i].set(Value::Undefined);
                }
            }
            if callee_slot == StackCalleeSlot::Present {
                caller.stack.pop(); // callee
            }
            caller.stack.pop().unwrap() // this
        };
        if param_base == 1 {
            frame.locals[0].set(this);
        }
        self.frames.push(frame);
    }

    // ---------------- オブジェクト確保ヘルパー ----------------

    pub(crate) fn alloc_obj_ref(&self, data: ObjData) -> ObjRef {
        let rc = Rc::new(RefCell::new(data));
        self.track_object(&rc);
        rc
    }

    pub(crate) fn alloc_obj(&self, data: ObjData) -> Value {
        Value::Object(self.alloc_obj_ref(data))
    }

    pub(crate) fn alloc_plain_object(&self) -> Value {
        self.alloc_obj(ObjData::new(ObjKind::Plain))
    }

    pub(crate) fn alloc_array(&self, items: Vec<Value>) -> Value {
        self.alloc_obj(ObjData::new(ObjKind::Array(items)))
    }

    pub(crate) fn alloc_regex_value(&self, re: Rc<crate::regex::Regex>) -> Value {
        thread_local! {
            static KEY_LAST_INDEX: Rc<str> = Rc::from("lastIndex");
        }
        let mut o = ObjData::new(ObjKind::Regex(re));
        o.props
            .set_value(KEY_LAST_INDEX.with(|k| k.clone()), Value::Number(0.0));
        self.alloc_obj(o)
    }

    pub(crate) fn alloc_native_with_captures(
        &self,
        name: &str,
        arity: usize,
        captures: Vec<ObjRef>,
        f: NativeFn,
    ) -> Value {
        self.alloc_obj(ObjData::new(ObjKind::Native(NativeObj {
            name: name.into(),
            arity,
            f,
            captures,
        })))
    }

    pub(super) fn wrap_function(&self, fo: Rc<FunctionObj>) -> Value {
        self.alloc_obj(ObjData::new(ObjKind::Function(fo)))
    }

    // ---------------- グローバル登録・モジュール実行 ----------------

    pub fn register_native(&mut self, name: &str, arity: usize, f: NativeFn) {
        let v = Value::native_fn(name, arity, f);
        self.track_value_tree(&v);
        self.globals
            .insert(Rc::from(name), Rc::new(RefCell::new(v)));
    }

    pub fn define_global(&mut self, name: &str, value: Value) {
        self.track_value_tree(&value);
        self.globals
            .insert(Rc::from(name), Rc::new(RefCell::new(value)));
    }

    pub fn run_module(&mut self, chunk: Rc<Chunk>) -> Result<Vec<(Rc<str>, Value)>, Value> {
        match self.start_module_task(chunk)? {
            ModuleTaskState::Settled(exports) => Ok(exports),
            ModuleTaskState::Pending => Err(self.type_error(
                "トップレベル await が解決しませんでした（保留中のホスト Promise があります。\
                 run_module は同期 API のため続行できません — run_event_loop を使ってください）",
            )),
        }
    }

    pub fn start_module_task(&mut self, chunk: Rc<Chunk>) -> Result<ModuleTaskState, Value> {
        let frame = Frame::new(chunk, self.empty_upvalues.clone(), self.empty_args.clone());
        self.frames = vec![frame];
        let (_, owner) = self.new_pending_promise();
        self.drive_current_task(owner.clone());
        self.run_event_loop();
        let state = {
            let b = owner.borrow();
            match &b.kind {
                ObjKind::Promise(pd) => match &pd.state {
                    PromiseState::Pending => None,
                    PromiseState::Fulfilled(v) => Some(Ok(v.clone())),
                    PromiseState::Rejected(e) => Some(Err(e.clone())),
                },
                _ => None,
            }
        };
        match state {
            Some(Ok(_)) => Ok(ModuleTaskState::Settled(
                self.pending_module_exports.take().unwrap_or_default(),
            )),
            Some(Err(e)) => Err(e),
            None => Ok(ModuleTaskState::Pending),
        }
    }

    // ---------------- スタック・変換・エラー生成 ----------------

    #[inline]
    pub(super) fn push(&mut self, v: Value) {
        self.frames.last_mut().unwrap().stack.push(v);
    }

    #[inline]
    pub(super) fn pop(&mut self) -> Value {
        self.frames
            .last_mut()
            .unwrap()
            .stack
            .pop()
            .expect("オペランドスタックが空です")
    }

    #[inline]
    pub(super) fn top(&self) -> &Value {
        self.frames
            .last()
            .unwrap()
            .stack
            .last()
            .expect("オペランドスタックが空です")
    }

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
        self.alloc_obj(o)
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

    pub fn syntax_error(&self, msg: impl Into<String>) -> Value {
        self.make_error("SyntaxError", msg.into())
    }
}

impl Default for Vm {
    fn default() -> Self {
        Self::new()
    }
}
