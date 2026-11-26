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
use crate::hash::FastHashMap;
use crate::value::*;

pub use ops::{
    array_values, is_callable, loose_eq, new_regex_value, number_to_js_string, to_int32, to_number,
    to_primitive, to_property_key_string, to_string_basic, to_uint32,
};
pub(crate) use ops::{
    number_to_js_string_into, to_string_into, to_string_into_with_root, with_str_buf_rc,
};
pub(crate) use props::{is_internal_prop_key, own_enumerable_keys};

/// 自動循環参照 GC の最小トリガー閾値（新規オブジェクト確保数）。
pub(super) const GC_MIN_THRESHOLD: usize = 2048;
/// フレームプールの最大保持数。
const MAX_FRAME_POOL: usize = 64;
/// 引数バッファプールの最大保持数。
const MAX_ARGS_POOL: usize = 32;
/// ネイティブ再帰呼び出し（`new`・コールバック・スプレッド・同期 async 起動）の安全深さ上限（T-4）。
/// Windows の既定 1MB スタック上の debug ビルドでもスタックオーバーフローしない安全値。
pub(super) const MAX_NATIVE_CALL_DEPTH: u32 = 40;
/// JS 呼び出しフレーム数の上限（T-4）。
pub(super) const MAX_CALL_FRAMES: usize = 16_384;
/// 密配列の最大要素数（T-12）。
pub(crate) const MAX_ARRAY_LEN: usize = 1 << 24;
/// 文字列構築の最大バイト長（T-12）。
pub(crate) const MAX_STRING_BYTES: usize = 1 << 26;

thread_local! {
    /// ASCII 単一文字の `Rc<str>` キャッシュ（`str[i]` のゼロ確保高速パス用）。
    static ASCII_CHARS: [Rc<str>; 128] = std::array::from_fn(|b| {
        let ch = [b as u8];
        Rc::from(std::str::from_utf8(&ch).expect("0..128 は常に有効な ASCII UTF-8"))
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
    pub(super) settled: Cell<bool>,
    pub(super) pending_host_count: Rc<Cell<usize>>,
}

impl HostPromise {
    /// 紐づく JS `Promise` を指定値で解決（fulfill）する。
    pub fn resolve(&self, vm: &mut Vm, value: Value) {
        if self.settled.replace(true) {
            return;
        }
        self.pending_host_count
            .set(self.pending_host_count.get().saturating_sub(1));
        vm.resolve_promise(self.promise.clone(), value);
    }

    /// 紐づく JS `Promise` を指定値で棄却（reject）する。
    pub fn reject(&self, vm: &mut Vm, value: Value) {
        if self.settled.replace(true) {
            return;
        }
        self.pending_host_count
            .set(self.pending_host_count.get().saturating_sub(1));
        vm.settle_promise(self.promise.clone(), Err(value));
    }
}

impl Drop for HostPromise {
    fn drop(&mut self) {
        if !self.settled.replace(true) {
            self.pending_host_count
                .set(self.pending_host_count.get().saturating_sub(1));
        }
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
    /// async タスク起動や継続再開中に退避している呼び出し元フレーム列（GC ルートとして可視化、T-1）。
    pub(super) suspended_callers: Vec<Vec<Frame>>,
    pub globals: HashMap<Rc<str>, Rc<RefCell<Value>>>,
    /// `console.log` 相当の出力フック。既定では標準出力へ。
    pub on_log: Box<dyn FnMut(&str)>,
    /// `Date` 等、非決定的な値をホストが注入するためのフック置き場。
    pub now_fn: Option<Rc<dyn Fn() -> f64>>,
    /// `Math.random()` 用の乱数フック（未設定時は決定的 PRNG を使用、T-15）。
    pub random_fn: Option<Box<dyn FnMut() -> f64>>,
    rng_state: Cell<u64>,
    /// モジュール owner Promise ごとに紐づけた export 一覧（T-16）。
    pub(super) pending_module_exports: FastHashMap<usize, Vec<(Rc<str>, Value)>>,
    pub(super) awaited_module_owners: Vec<usize>,
    /// `run_module_graph` 呼び出しを跨いで単調増加するモジュール名前空間カウンタ（T-19）。
    pub(crate) module_ns_counter: u32,

    // ---------------- T3: Promise / イベントループ ----------------
    pub microtasks: VecDeque<MicrotaskJob>,
    pub(super) maybe_unhandled: Vec<ObjRef>,
    pub on_unhandled_rejection: Box<dyn FnMut(&Value)>,
    pub(super) has_custom_unhandled_hook: bool,
    pub(super) pending_host_count: Rc<Cell<usize>>,
    pub(super) native_call_depth: u32,
    /// 実際の Rust スタック再帰深さ（`run_nested_call` / `spawn_async_task_from_function` / `resume_continuation`）。
    pub(super) rust_call_depth: u32,
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
            suspended_callers: Vec::new(),
            globals: HashMap::new(),
            on_log: Box::new(|s: &str| println!("{s}")),
            now_fn: None,
            random_fn: None,
            rng_state: Cell::new(0x243f_6a88_85a3_08d3),
            pending_module_exports: FastHashMap::default(),
            awaited_module_owners: Vec::new(),
            module_ns_counter: 0,
            microtasks: VecDeque::new(),
            maybe_unhandled: Vec::new(),
            on_unhandled_rejection: Box::new(|_v: &Value| {}),
            has_custom_unhandled_hook: false,
            pending_host_count: Rc::new(Cell::new(0)),
            native_call_depth: 0,
            rust_call_depth: 0,
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

    pub fn set_unhandled_rejection_hook(&mut self, f: impl FnMut(&Value) + 'static) {
        self.on_unhandled_rejection = Box::new(f);
        self.has_custom_unhandled_hook = true;
    }

    /// `Math.random()` 用の乱数値を返す（カスタムフック優先、未設定時は決定的 SplitMix64）。
    pub(crate) fn next_random(&mut self) -> f64 {
        if let Some(ref mut f) = self.random_fn {
            return f();
        }
        let mut z = self.rng_state.get().wrapping_add(0x9e37_79b9_7f4a_7c15);
        self.rng_state.set(z);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^= z >> 31;
        ((z >> 11) as f64) * (1.0 / ((1u64 << 53) as f64))
    }

    // ---------------- プール・フレーム管理 ----------------

    #[inline]
    pub(super) fn take_args_buf(&mut self) -> Vec<Value> {
        self.args_pool.pop().unwrap_or_default()
    }

    #[inline]
    pub(super) fn put_args_buf(&mut self, mut buf: Vec<Value>) {
        buf.clear();
        if self.args_pool.len() < MAX_ARGS_POOL && buf.capacity() <= 64 {
            self.args_pool.push(buf);
        }
    }

    #[inline]
    pub(super) fn release_frame(&mut self, mut frame: Frame) {
        frame.stack.clear();
        frame.locals.clear();
        if let Some(v) = Rc::get_mut(&mut frame.raw_args) {
            if v.capacity() <= 64 {
                v.clear();
            } else {
                frame.raw_args = self.empty_args.clone();
            }
        } else {
            frame.raw_args = self.empty_args.clone();
        }
        frame.upvalues = self.empty_upvalues.clone();
        if self.frame_pool.len() < MAX_FRAME_POOL
            && frame.stack.capacity() <= 128
            && frame.locals.capacity() <= 128
        {
            self.frame_pool.push(frame);
        }
    }

    #[inline]
    fn acquire_pooled_frame(&mut self, fo: &Rc<FunctionObj>) -> Frame {
        if let Some(mut frame) = self.frame_pool.pop() {
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
        }
    }

    #[inline]
    fn fill_frame_raw_args(
        &mut self,
        frame: &mut Frame,
        needs_raw: bool,
        fill: impl FnOnce(&mut Vec<Value>),
    ) {
        if needs_raw {
            if !Rc::ptr_eq(&frame.raw_args, &self.empty_args) {
                if let Some(v) = Rc::get_mut(&mut frame.raw_args) {
                    v.clear();
                    fill(v);
                    return;
                }
            }
            let mut v = self.take_args_buf();
            fill(&mut v);
            frame.raw_args = Rc::new(v);
        } else if !frame.raw_args.is_empty() {
            if let Some(v) = Rc::get_mut(&mut frame.raw_args) {
                v.clear();
            } else {
                frame.raw_args = self.empty_args.clone();
            }
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
        let mut frame = self.acquire_pooled_frame(f);
        self.fill_frame_raw_args(&mut frame, needs_raw, |v| v.extend_from_slice(args));
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
    ) -> Result<(), Value> {
        if self.frames.len() >= MAX_CALL_FRAMES {
            let caller = self.frames.last_mut().unwrap();
            let drop_cnt = argc + usize::from(callee_slot == StackCalleeSlot::Present) + 1;
            let new_len = caller.stack.len().saturating_sub(drop_cnt);
            caller.stack.truncate(new_len);
            return Err(self.range_error("Maximum call stack size exceeded"));
        }
        let needs_raw = fo.chunk.needs_raw_args && argc > 0;
        let mut frame = self.acquire_pooled_frame(fo);
        let sp = fo.chunk.simple_param_count as usize;
        let param_base = usize::from(!fo.is_arrow && !frame.locals.is_empty());

        let this = {
            let caller = self.frames.last_mut().unwrap();
            let arg_start = caller.stack.len() - argc;
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
        Ok(())
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

    pub(crate) fn alloc_plain_object_with_cap(&self, cap: usize) -> Value {
        self.alloc_obj(ObjData::with_props_cap(ObjKind::Plain, cap))
    }

    pub(crate) fn alloc_array(&self, items: Vec<Value>) -> Value {
        self.alloc_obj(ObjData::new(ObjKind::Array(items)))
    }

    pub(crate) fn alloc_regex_value(&self, re: Rc<crate::regex::Regex>) -> Value {
        // `lastIndex` は読み取り既定値が 0.0 のため遅延確保する（非 global 正規表現での PropMap 確保ゼロ）。
        self.alloc_obj(ObjData::new(ObjKind::Regex(re)))
    }

    pub(crate) fn alloc_native_with_captures(
        &self,
        name: &str,
        arity: usize,
        captures: Vec<ObjRef>,
        f: NativeFn,
    ) -> Value {
        self.alloc_obj(ObjData::new(ObjKind::Native(Box::new(NativeObj {
            name: name.into(),
            arity,
            f,
            captures: captures.into_boxed_slice(),
            ctor_tag: None,
        }))))
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
        match self.start_module_task_inner(chunk, true)? {
            ModuleTaskState::Settled(exports) => Ok(exports),
            ModuleTaskState::Pending => Err(self.type_error(
                "トップレベル await が解決しませんでした（保留中のホスト Promise があります。\
                 run_module は同期 API のため続行できません — run_event_loop を使ってください）",
            )),
        }
    }

    pub fn start_module_task(&mut self, chunk: Rc<Chunk>) -> Result<ModuleTaskState, Value> {
        self.start_module_task_inner(chunk, false)
    }

    fn start_module_task_inner(
        &mut self,
        chunk: Rc<Chunk>,
        cancel_if_pending: bool,
    ) -> Result<ModuleTaskState, Value> {
        let mut frame = Frame::new(chunk, self.empty_upvalues.clone(), self.empty_args.clone());
        // モジュールトップレベルの `this`（スロット 0）は undefined（T-23）。
        if !frame.locals.is_empty() {
            frame.locals[0].set(Value::Undefined);
        }
        let caller_frames = std::mem::replace(&mut self.frames, vec![frame]);
        self.suspended_callers.push(caller_frames);

        let (_, owner) = self.new_pending_promise();
        // 同期完了（または初回イベントループ内での失敗）は戻り値として直接返すため、
        // 二重報告を防ぐ目的で最初は handled = true にしておく（T-20）。
        if let ObjKind::Promise(ref mut pd) = owner.borrow_mut().kind {
            pd.handled = true;
        }
        let owner_ptr = Rc::as_ptr(&owner) as usize;
        self.awaited_module_owners.push(owner_ptr);

        self.drive_current_task(owner.clone());
        self.frames = self.suspended_callers.pop().unwrap_or_default();
        self.run_event_loop();

        if let Some(pos) = self
            .awaited_module_owners
            .iter()
            .rposition(|&p| p == owner_ptr)
        {
            self.awaited_module_owners.remove(pos);
        }
        let exports = self
            .pending_module_exports
            .remove(&owner_ptr)
            .unwrap_or_default();

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
            Some(Ok(_)) => Ok(ModuleTaskState::Settled(exports)),
            Some(Err(e)) => Err(e),
            None => {
                if cancel_if_pending {
                    // `run_module` が Pending で失敗する場合、保留中のタスクが後から
                    // 副作用を実行しないよう owner をキャンセル済み状態にする（T-16）。
                    let err = self.type_error(
                        "トップレベル await が解決しませんでした（保留中のホスト Promise があります。\
                         run_module は同期 API のため続行できません — run_event_loop を使ってください）",
                    );
                    if let ObjKind::Promise(ref mut pd) = owner.borrow_mut().kind {
                        pd.state = PromiseState::Rejected(err.clone());
                        pd.handled = true;
                        pd.reactions.clear();
                    }
                    Err(err)
                } else {
                    // Pending のまま返す場合、後続の非同期失敗は未捕捉 reject として
                    // ホストへ通知されるよう handled を戻す（T-16 / T-20）。
                    if let ObjKind::Promise(ref mut pd) = owner.borrow_mut().kind {
                        pd.handled = false;
                    }
                    Ok(ModuleTaskState::Pending)
                }
            }
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
        use std::fmt::Write;
        let mut o = ObjData::new(ObjKind::Error);
        o.props.set_value(Rc::from("name"), Value::str(name));
        o.props
            .set_value(Rc::from("message"), Value::str(msg.clone()));
        let mut stack = format!("{name}: {msg}");
        for frame in self.frames.iter().rev() {
            let fault_ip = frame.ip.saturating_sub(1);
            let line = frame.chunk.lines.get(fault_ip).copied().unwrap_or(0);
            let chunk_name = if frame.chunk.name.is_empty() {
                "<anonymous>"
            } else {
                &frame.chunk.name
            };
            let _ = write!(stack, "\n    at {chunk_name}:{line}");
        }
        o.props.set_value(Rc::from("stack"), Value::str(stack));
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

impl Drop for Vm {
    fn drop(&mut self) {
        // VM が保持する全ルートを先に解放する。循環参照を持たないオブジェクトは
        // この時点で通常の Rc::drop により即座に解放される。
        self.frames.clear();
        self.suspended_callers.clear();
        self.globals.clear();
        self.pending_module_exports.clear();
        self.awaited_module_owners.clear();
        self.microtasks.clear();
        self.maybe_unhandled.clear();
        self.current_owner_promise = None;
        self.frame_pool.clear();
        self.args_pool.clear();

        // ルート解放後も生存している追跡オブジェクト（循環参照または外部 Rust 保持）が
        // 存在する場合のみ trial-deletion GC を走らせ、到達不能な循環参照を確実に回収する。
        // 外部 Rust コードが保持している Value は strong_count 検査によりそのまま保護される。
        let has_remaining = self
            .gc_objects
            .borrow()
            .iter()
            .any(|w| w.strong_count() > 0);
        if has_remaining {
            self.collect_garbage();
        }
    }
}
