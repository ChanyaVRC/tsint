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

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::{Rc, Weak};

use crate::bytecode::*;
use crate::value::*;

/// 自動循環参照 GC の最小トリガー閾値（新規オブジェクト確保数）。
const GC_MIN_THRESHOLD: usize = 2048;

/// ローカル変数スロットの表現。
///
/// 内側クロージャにキャプチャされないローカル変数は `Direct(Value)` として直接保持し、
/// `Rc<RefCell<Value>>` のヒープ確保・参照カウント操作・borrow チェックを完全に省く。
/// キャプチャされる変数のみ `Boxed(Rc<RefCell<Value>>)` とし、クロージャとセルを共有する。
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
    fn new(
        chunk: Rc<Chunk>,
        upvalues: Rc<Vec<Rc<RefCell<Value>>>>,
        raw_args: Rc<Vec<Value>>,
    ) -> Self {
        let num_locals = chunk.num_locals;
        let locals = (0..num_locals)
            .map(|i| {
                if chunk.captured_locals.get(i).copied().unwrap_or(false) {
                    LocalSlot::Boxed(Rc::new(RefCell::new(Value::Uninit)))
                } else {
                    LocalSlot::Direct(Value::Uninit)
                }
            })
            .collect();
        Frame {
            chunk,
            ip: 0,
            locals,
            upvalues,
            stack: Vec::new(),
            raw_args,
        }
    }
}

/// `Vm::step` の結果。
enum StepSignal {
    /// 通常の 1 命令実行（フレームはまだ継続中）。
    None,
    /// 現在のフレームが `Return` で完全に戻った。
    Returned(Value),
    /// `await` によりタスク全体（`self.frames`）が中断された。
    Suspended,
}

/// `Vm::run_until` の結果。
pub enum RunOutcome {
    Value(Value),
    Suspended,
}

/// ビルトインメソッドのディスパッチ先を区別するタグ（`builtins.rs` 参照）。
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
    /// `.then`/`.catch`/`.finally` が登録した反応。
    Then {
        on_fulfilled: Option<Value>,
        on_rejected: Option<Value>,
        result_promise: ObjRef,
        outcome: Result<Value, Value>,
    },
    /// `resolve(thenable)` された時の「相手の then を叩く」ジョブ
    /// （PromiseResolveThenableJob 相当）。
    ResolveThenable {
        promise: ObjRef,
        thenable: Value,
        then_fn: Value,
    },
    /// `await` で中断していたタスクの再開。
    ResumeAwait {
        cont: AwaitContinuation,
        outcome: Result<Value, Value>,
    },
}

/// `run_event_loop` の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PumpState {
    /// マイクロタスクキューが空で、保留中のホスト Promise も無い
    /// （これ以上ホストが何かしない限り進む処理は無い）。
    Idle,
    /// マイクロタスクキューは空だが、`HostPromise` がまだ解決されていない
    /// ものが残っている（ホストが時間を進める・入力を与える等すれば続行できる）。
    Waiting,
}

/// `Vm::start_module_task` の結果（T5: モジュールシステム）。
#[derive(Debug)]
pub enum ModuleTaskState {
    /// トップレベル評価が（トップレベル await も含めて）完全に解決した。
    Settled(Vec<(Rc<str>, Value)>),
    /// ホスト Promise 待ちのまま中断された。
    Pending,
}

/// ホストが後から解決/棄却できる Promise のハンドル
/// （`Interpreter::register_native_async` 参照）。
pub struct HostPromise {
    promise: ObjRef,
}

impl HostPromise {
    pub fn resolve(&self, vm: &mut Vm, value: Value) {
        vm.settle_host_promise(self.promise.clone(), Ok(value));
    }
    pub fn reject(&self, vm: &mut Vm, value: Value) {
        vm.settle_host_promise(self.promise.clone(), Err(value));
    }
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

    // ---------------- T3: Promise / イベントループ ----------------
    /// マイクロタスクキュー（FIFO）。
    pub microtasks: VecDeque<MicrotaskJob>,
    /// reject 時に reaction が 1 つも無かった Promise の一覧。マイクロタスク
    /// キューが完全にはけた時点でまだ handled になっていなければ
    /// 「未捕捉の reject」としてホストへ報告する。
    maybe_unhandled: Vec<ObjRef>,
    /// 未捕捉 reject の報告フック（既定は何もしない）。
    pub on_unhandled_rejection: Box<dyn FnMut(&Value)>,
    /// `HostPromise` のうちまだ解決されていない数（`run_event_loop` が
    /// `PumpState::Waiting` を返すかどうかの判定に使う）。
    pending_host_count: usize,
    /// 現在実行中のタスク（フレーム列）が、ネイティブ関数の Rust 再帰呼び出し
    /// （`call_value`/`call_function_obj`）の内側で動いている深さ。0 より
    /// 大きい間は `await` によるフレーム列の中断ができない（Rust の呼び出し
    /// スタックを巻き戻す手段が無いため）。async 関数呼び出しは常に新しい
    /// 独立したフレーム列で駆動されるため、この深さは async 呼び出しの
    /// たびに 0 へリセットされる（`drive_current_task` 参照）。
    native_call_depth: u32,
    /// 現在駆動中のタスクが完了した時に確定させる Promise
    /// （`await` 中断時に継続へ持たせる）。
    current_owner_promise: Option<ObjRef>,

    // ---------------- 循環参照回収 (GC) ----------------
    /// VM 上で生成・登録された全オブジェクトへの弱参照リスト。
    /// `&self` メソッド（`make_error` 等）からも追跡できるよう `RefCell` で包む。
    gc_objects: RefCell<Vec<Weak<RefCell<ObjData>>>>,
    /// 前回の GC 以降に追跡登録されたオブジェクト数。
    gc_allocs_since_last: Cell<usize>,
    /// 次回自動 GC を発火する確保数閾値（生存数に応じて成長する）。
    gc_threshold: usize,
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
        };
        crate::builtins::install(&mut vm);
        vm
    }

    /// オブジェクトを GC 追跡リストに登録する。
    pub fn track_object(&self, obj: &ObjRef) {
        self.gc_objects.borrow_mut().push(Rc::downgrade(obj));
        self.gc_allocs_since_last
            .set(self.gc_allocs_since_last.get() + 1);
    }

    /// 未追跡（`Rc::weak_count == 0`）のオブジェクトとその配下の未追跡オブジェクトを
    /// GC 追跡リストへ登録する。既に追跡済みのノードは O(1) でスキップする。
    pub fn track_value_tree(&self, v: &Value) {
        let Value::Object(root) = v else {
            return;
        };
        if Rc::weak_count(root) > 0 {
            return;
        }
        let mut stack = vec![root.clone()];
        while let Some(obj) = stack.pop() {
            if Rc::weak_count(&obj) > 0 {
                continue;
            }
            self.track_object(&obj);
            if let Ok(b) = obj.try_borrow() {
                for (_, slot) in b.props.iter() {
                    match slot {
                        PropSlot::Value(Value::Object(child)) => {
                            if Rc::weak_count(child) == 0 {
                                stack.push(child.clone());
                            }
                        }
                        PropSlot::Accessor { get, set } => {
                            if let Some(Value::Object(child)) = get {
                                if Rc::weak_count(child) == 0 {
                                    stack.push(child.clone());
                                }
                            }
                            if let Some(Value::Object(child)) = set {
                                if Rc::weak_count(child) == 0 {
                                    stack.push(child.clone());
                                }
                            }
                        }
                        _ => {}
                    }
                }
                if let Some(proto) = &b.proto {
                    if Rc::weak_count(proto) == 0 {
                        stack.push(proto.clone());
                    }
                }
                match &b.kind {
                    ObjKind::Array(items) => {
                        for it in items {
                            if let Value::Object(child) = it {
                                if Rc::weak_count(child) == 0 {
                                    stack.push(child.clone());
                                }
                            }
                        }
                    }
                    ObjKind::Map(m) => {
                        for (k, val) in &m.entries {
                            if let Value::Object(child) = k {
                                if Rc::weak_count(child) == 0 {
                                    stack.push(child.clone());
                                }
                            }
                            if let Value::Object(child) = val {
                                if Rc::weak_count(child) == 0 {
                                    stack.push(child.clone());
                                }
                            }
                        }
                    }
                    ObjKind::Set(s) => {
                        for val in &s.values {
                            if let Value::Object(child) = val {
                                if Rc::weak_count(child) == 0 {
                                    stack.push(child.clone());
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

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
        let mut o = ObjData::new(ObjKind::Regex(re));
        o.props.set_value(Rc::from("lastIndex"), Value::Number(0.0));
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

    fn wrap_function(&self, fo: Rc<FunctionObj>) -> Value {
        self.alloc_obj(ObjData::new(ObjKind::Function(fo)))
    }

    /// 現在 GC 追跡リスト上で生存しているオブジェクト数を返す（テスト・診断用）。
    pub fn live_tracked_objects(&self) -> usize {
        let mut seen = HashSet::new();
        self.gc_objects
            .borrow()
            .iter()
            .filter(|w| w.strong_count() > 0 && seen.insert(w.as_ptr() as usize))
            .count()
    }

    /// 自動 GC のトリガー閾値を設定する（テスト・チューニング用）。
    pub fn set_gc_threshold(&mut self, threshold: usize) {
        self.gc_threshold = threshold.max(1);
    }

    #[inline]
    fn maybe_collect_garbage(&mut self) {
        if self.gc_allocs_since_last.get() >= self.gc_threshold {
            self.collect_garbage();
        }
    }

    /// マーク＋厳密外部参照カウント検査（trial-deletion）による循環参照回収。
    ///
    /// 1. VM ルート（`frames`, `globals`, `pending_module_exports`, `microtasks`,
    ///    `maybe_unhandled`, `current_owner_promise`）から到達可能な全オブジェクトをマーク。
    /// 2. 弱参照リストから死亡済みエントリを除去し、未マークかつ生存中の候補を収集。
    /// 3. 各候補 `o` の外部参照数 `Rc::strong_count(o) - 1` から、候補集合内部の
    ///    エッジ（共有 `FunctionObj` / アップバリューセル / `raw_args` は重複排除）を
    ///    厳密に差し引く。
    /// 4. 差し引き後に外部参照数が 0 より大きい候補（ホスト Rust コードが直接保持する
    ///    `Value` や `HostPromise`、不透明なネイティブクロージャが保持する参照など）から
    ///    到達可能な候補はすべて生存扱いとして保護する。
    /// 5. 残った「外部参照 0 かつ外部生存集合からも非到達」な候補のみが 100% 到達不能な
    ///    循環ゴミであるため、それらの発信参照（`props`/`proto`/`kind`）を切断して
    ///    `Rc` により即時解放する。
    ///
    /// 戻り値は今回参照切断（回収）した循環オブジェクトの個数。
    pub fn collect_garbage(&mut self) -> usize {
        // ---- Phase 1: VM ルートからのマーク ----
        let mut marked: HashSet<usize> = HashSet::new();
        let mut worklist: Vec<ObjRef> = Vec::new();

        for frame in &self.frames {
            gc_mark_frame(frame, &mut marked, &mut worklist);
        }
        for cell in self.globals.values() {
            if let Ok(v) = cell.try_borrow() {
                gc_mark_value(&v, &mut marked, &mut worklist);
            }
        }
        if let Some(exports) = &self.pending_module_exports {
            for (_, v) in exports {
                gc_mark_value(v, &mut marked, &mut worklist);
            }
        }
        for job in &self.microtasks {
            match job {
                MicrotaskJob::Then {
                    on_fulfilled,
                    on_rejected,
                    result_promise,
                    outcome,
                } => {
                    if let Some(v) = on_fulfilled {
                        gc_mark_value(v, &mut marked, &mut worklist);
                    }
                    if let Some(v) = on_rejected {
                        gc_mark_value(v, &mut marked, &mut worklist);
                    }
                    gc_mark_obj(result_promise, &mut marked, &mut worklist);
                    match outcome {
                        Ok(v) | Err(v) => gc_mark_value(v, &mut marked, &mut worklist),
                    }
                }
                MicrotaskJob::ResolveThenable {
                    promise,
                    thenable,
                    then_fn,
                } => {
                    gc_mark_obj(promise, &mut marked, &mut worklist);
                    gc_mark_value(thenable, &mut marked, &mut worklist);
                    gc_mark_value(then_fn, &mut marked, &mut worklist);
                }
                MicrotaskJob::ResumeAwait { cont, outcome } => {
                    for frame in &cont.frames {
                        gc_mark_frame(frame, &mut marked, &mut worklist);
                    }
                    gc_mark_obj(&cont.owner_promise, &mut marked, &mut worklist);
                    match outcome {
                        Ok(v) | Err(v) => gc_mark_value(v, &mut marked, &mut worklist),
                    }
                }
            }
        }
        for p in &self.maybe_unhandled {
            gc_mark_obj(p, &mut marked, &mut worklist);
        }
        if let Some(p) = &self.current_owner_promise {
            gc_mark_obj(p, &mut marked, &mut worklist);
        }

        while let Some(obj) = worklist.pop() {
            gc_visit_obj_outgoing(&obj, &mut |target| {
                gc_mark_obj(target, &mut marked, &mut worklist);
            });
        }

        // ---- Phase 2: 死亡済み弱参照の除去と未マーク候補の収集 ----
        let raw_weaks = std::mem::take(&mut *self.gc_objects.borrow_mut());
        let mut live_weaks: Vec<Weak<RefCell<ObjData>>> = Vec::with_capacity(raw_weaks.len());
        let mut seen_weaks: HashSet<usize> = HashSet::with_capacity(raw_weaks.len());
        let mut candidates: Vec<(usize, ObjRef)> = Vec::new();
        let mut candidate_idx: HashMap<usize, usize> = HashMap::new();

        for w in raw_weaks {
            if let Some(rc) = w.upgrade() {
                let ptr = Rc::as_ptr(&rc) as usize;
                if !seen_weaks.insert(ptr) {
                    continue;
                }
                live_weaks.push(w);
                if !marked.contains(&ptr) {
                    let idx = candidates.len();
                    candidate_idx.insert(ptr, idx);
                    candidates.push((ptr, rc));
                }
            }
        }

        if candidates.is_empty() {
            let live_count = live_weaks.len();
            *self.gc_objects.borrow_mut() = live_weaks;
            self.gc_allocs_since_last.set(0);
            self.gc_threshold = (live_count * 2).max(GC_MIN_THRESHOLD);
            return 0;
        }

        // ---- Phase 3: 候補集合内部の参照カウントを差し引く（trial-deletion） ----
        let mut external_refs: Vec<usize> = candidates
            .iter()
            .map(|(_, rc)| Rc::strong_count(rc).saturating_sub(1))
            .collect();

        // 共有される中間 Rc コンテナ（FunctionObj / アップバリューセル / raw_args）は
        // 内部に持つ ObjRef の strong_count を 1 回分しか増やさないため重複排除する。
        let mut seen_fns: HashSet<usize> = HashSet::new();
        let mut seen_cells: HashSet<usize> = HashSet::new();
        let mut seen_raw_args: HashSet<usize> = HashSet::new();

        for (i, (_, rc)) in candidates.iter().enumerate() {
            let Ok(b) = rc.try_borrow() else {
                // 現在借用中のオブジェクトは保守的に外部生存扱いとする。
                external_refs[i] = external_refs[i].saturating_add(1);
                continue;
            };
            gc_visit_obj_data_dedup(
                &b,
                &mut seen_fns,
                &mut seen_cells,
                &mut seen_raw_args,
                &mut |target| {
                    let tptr = Rc::as_ptr(target) as usize;
                    if let Some(&tidx) = candidate_idx.get(&tptr) {
                        debug_assert!(external_refs[tidx] > 0);
                        external_refs[tidx] = external_refs[tidx].saturating_sub(1);
                    }
                },
            );
        }

        // ---- Phase 4: 外部参照が残る候補から到達可能なものをすべて生存保護 ----
        let mut externally_alive = vec![false; candidates.len()];
        let mut ext_worklist: Vec<usize> = Vec::new();
        for (i, &ext) in external_refs.iter().enumerate() {
            if ext > 0 {
                externally_alive[i] = true;
                ext_worklist.push(i);
            }
        }
        while let Some(i) = ext_worklist.pop() {
            gc_visit_obj_outgoing(&candidates[i].1, &mut |target| {
                let tptr = Rc::as_ptr(target) as usize;
                if let Some(&j) = candidate_idx.get(&tptr) {
                    if !externally_alive[j] {
                        externally_alive[j] = true;
                        ext_worklist.push(j);
                    }
                }
            });
        }

        // ---- Phase 5: 完全に到達不能な循環ゴミの参照を切断して解放 ----
        let mut collected = 0usize;
        for (i, (_, rc)) in candidates.iter().enumerate() {
            if !externally_alive[i] {
                if let Ok(mut b) = rc.try_borrow_mut() {
                    b.props.clear();
                    b.proto = None;
                    b.kind = ObjKind::Plain;
                    collected += 1;
                }
            }
        }
        // `candidates` を先にドロップすることで、参照切断済みオブジェクトの
        // strong_count が 0 になり即時解放される。
        drop(candidates);
        live_weaks.retain(|w| w.strong_count() > 0);

        let live_count = live_weaks.len();
        *self.gc_objects.borrow_mut() = live_weaks;
        self.gc_allocs_since_last.set(0);
        self.gc_threshold = (live_count * 2).max(GC_MIN_THRESHOLD);
        collected
    }

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

    // =================================================================
    // モジュール実行
    // =================================================================

    /// モジュールのトップレベルチャンクを実行し、export された値の一覧を返す。
    ///
    /// T3: モジュール本体もひとつの「タスク」として駆動する。トップレベル
    /// `await` に出会うと（他の async 関数と同じ仕組みで）中断し、
    /// マイクロタスクキュー（Promise チェーン）を汲み尽くしてから完了状態を
    /// 確認する。トップレベル `await` がホスト側の `HostPromise`
    /// （まだ誰も resolve/reject していないもの）待ちのまま残った場合は
    /// エラーになる（`run_module` は同期 API のため。ホスト駆動の非同期
    /// モジュール実行には `Vm::run_event_loop` を直接使うこと。T5 で
    /// Noiria 統合する際はこちらの経路を使う想定）。
    pub fn run_module(&mut self, chunk: Rc<Chunk>) -> Result<Vec<(Rc<str>, Value)>, Value> {
        match self.start_module_task(chunk)? {
            ModuleTaskState::Settled(exports) => Ok(exports),
            ModuleTaskState::Pending => Err(self.type_error(
                "トップレベル await が解決しませんでした（保留中のホスト Promise があります。\
                 run_module は同期 API のため続行できません — run_event_loop を使ってください）",
            )),
        }
    }

    /// `run_module` の下請け。モジュールのトップレベルチャンクをひとつの
    /// タスクとして起動し、最初の完了/保留状態まで駆動する。`run_module` と
    /// 違い、トップレベル await がホスト Promise 待ちのまま残っても
    /// エラーにせず `ModuleTaskState::Pending` を返す（T5: モジュールグラフの
    /// エントリポイントが実質「終わらないゲームループ」を抱えるケース用。
    /// `src/module.rs` の `Interpreter::run_module_graph` 参照）。続きは
    /// ホスト側が `run_event_loop` を繰り返し呼んで駆動する。
    pub fn start_module_task(&mut self, chunk: Rc<Chunk>) -> Result<ModuleTaskState, Value> {
        let frame = Frame::new(chunk, Rc::new(Vec::new()), Rc::new(Vec::new()));
        self.frames = vec![frame];
        let (_, owner) = self.new_pending_promise();
        self.drive_current_task(owner.clone());
        // モジュール本体が同期的に完了した場合でも、Promise チェーンの
        // マイクロタスクは必ず 1 回汲み尽くす（`Promise.resolve().then(...)`
        // のようなコードが戻る前に実行されることを保証する）。
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

    // =================================================================
    // 実行ループ
    // =================================================================

    /// `self.frames.len()` が `stop_depth` 以下になるまでステップ実行する。
    /// 例外は各フレームの例外テーブルで処理を試み、処理できなければ
    /// フレームを 1 つ捨てて外側へ伝播する。`stop_depth` より外側まで
    /// 伝播したら `Err` で返す。
    ///
    /// T3: `await` によってタスク全体が中断された場合は
    /// `Ok(RunOutcome::Suspended)` を返す（この時点で `self.frames` は
    /// 空になっている）。これは `native_call_depth == 0`（＝ Rust の
    /// 再帰呼び出しの内側ではない、タスク駆動ループ自身の呼び出し）の
    /// 場合にのみ起こる。`call_value`/`call_function_obj` の中では
    /// `native_call_depth` を必ずインクリメントしてから `run_until` を
    /// 呼ぶため、そちらの経路で `Suspended` が返ることは無い。
    pub fn run_until(&mut self, stop_depth: usize) -> Result<RunOutcome, Value> {
        let mut last_value = Value::Undefined;
        loop {
            if self.frames.len() <= stop_depth {
                return Ok(RunOutcome::Value(last_value));
            }
            if self.native_call_depth == 0 {
                self.maybe_collect_garbage();
            }
            match self.step() {
                Ok(StepSignal::Returned(v)) => {
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
                Ok(StepSignal::None) => {}
                Ok(StepSignal::Suspended) => return Ok(RunOutcome::Suspended),
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
    /// その戻り値を `StepSignal::Returned(v)` で伝える（呼び出し元 `run_until` の
    /// トップレベル戻り値を組み立てるため）。`await` によるタスク全体の中断は
    /// `StepSignal::Suspended` で伝える（この時点で `self.frames` は既に
    /// 空になっている — 継続はどこかの Promise の reaction として登録済み）。
    fn step(&mut self) -> Result<StepSignal, Value> {
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
                let v = self.frames.last().unwrap().locals[slot as usize].get();
                if matches!(v, Value::Uninit) {
                    return Err(
                        self.reference_error("変数が初期化される前にアクセスされました（TDZ）")
                    );
                }
                self.push(v);
            }
            OpCode::SetLocal(slot) => {
                let v = self.top().clone();
                self.frames.last_mut().unwrap().locals[slot as usize].set(v);
            }
            OpCode::BindLocal(slot) => {
                let v = self.pop();
                self.frames.last_mut().unwrap().locals[slot as usize].set(v);
            }
            OpCode::RebindLocal(slot) => {
                let f = self.frames.last_mut().unwrap();
                if let LocalSlot::Boxed(cell) = &mut f.locals[slot as usize] {
                    let cur = cell.borrow().clone();
                    *cell = Rc::new(RefCell::new(cur));
                }
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
                let arr = self.alloc_array(rest);
                self.push(arr);
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
                if self.frames.len() == 1 && self.frames.last().unwrap().chunk.is_module {
                    // モジュールのトップレベルフレームが終了する。export 値を
                    // pop してしまう前にここで確定させておく（`run_module` 参照）。
                    // 深さだけでなく `chunk.is_module` も見る理由: T3 では
                    // async 関数呼び出しも専用の（深さ 1 から始まる）フレーム列で
                    // 駆動されるため、深さだけではモジュール本体と区別できない。
                    let frame = self.frames.last().unwrap();
                    let exports = frame
                        .chunk
                        .exports
                        .iter()
                        .map(|(name, slot)| (name.clone(), frame.locals[*slot as usize].get()))
                        .collect();
                    self.pending_module_exports = Some(exports);
                }
                self.frames.pop();
                return Ok(StepSignal::Returned(v));
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
                let v = self.wrap_function(fo);
                self.push(v);
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
            OpCode::NewObject => {
                let o = self.alloc_plain_object();
                self.push(o);
            }
            OpCode::NewArray => {
                let a = self.alloc_array(Vec::new());
                self.push(a);
            }
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
                let arr = self.alloc_array(items);
                self.push(arr);
            }
            OpCode::ObjectKeysArray => {
                let v = self.pop();
                let keys = self.own_keys_array(&v);
                let arr = self.alloc_array(keys);
                self.push(arr);
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
                let val = self.alloc_obj(out);
                self.push(val);
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
                let val = self.alloc_array(sliced);
                self.push(val);
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
                let v = self.alloc_regex_value(re);
                self.push(v);
            }
            OpCode::Await => {
                // ネイティブ関数の Rust 再帰呼び出し（`call_value`/`call_function_obj`）
                // の内側では、Rust の呼び出しスタックを巻き戻す手段が無いため
                // 中断できない（README の既知の制約を参照）。
                if self.native_call_depth > 0 {
                    return Err(self.type_error(
                        "await はネイティブコールバックの内側では中断できません（T3 の既知の制約）",
                    ));
                }
                let v = self.pop();
                let promise = self.coerce_to_promise(v);
                let owner = self
                    .current_owner_promise
                    .clone()
                    .expect("await はタスク駆動ループの外では実行されないはず");
                let frames = std::mem::take(&mut self.frames);
                let cont = AwaitContinuation {
                    frames,
                    owner_promise: owner,
                };
                self.add_reaction(&promise, PromiseReaction::Await(cont));
                return Ok(StepSignal::Suspended);
            }
        }
        Ok(StepSignal::None)
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
                UpvalueDesc::ParentLocal(slot) => frame.locals[*slot as usize].as_boxed_cell(),
                UpvalueDesc::ParentUpvalue(idx) => frame.upvalues[*idx as usize].clone(),
            };
            ups.push(cell);
        }
        Rc::new(FunctionObj {
            name: tpl.name.clone(),
            chunk: tpl.chunk.clone(),
            upvalues: Rc::new(ups),
            is_arrow: tpl.is_arrow,
            is_async: tpl.is_async,
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
            proto
                .props
                .set_value(m.name.clone(), self.wrap_function(fo));
        }
        for a in &tpl.accessors {
            if a.is_static {
                continue;
            }
            let fo = self.build_closure(&a.func);
            proto
                .props
                .define_accessor(a.name.clone(), a.is_get, self.wrap_function(fo));
        }
        let proto_rc: ObjRef = self.alloc_obj_ref(proto);

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
        let class_rc: ObjRef = self.alloc_obj_ref(ObjData::new(ObjKind::Class(class_data)));
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
                .set_value(m.name.clone(), self.wrap_function(fo));
        }
        for a in &tpl.accessors {
            if !a.is_static {
                continue;
            }
            let fo = self.build_closure(&a.func);
            class_rc.borrow_mut().props.define_accessor(
                a.name.clone(),
                a.is_get,
                self.wrap_function(fo),
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
            CallOutcome::FramePushed => {
                // ここから先は「ネイティブ関数の Rust 再帰呼び出しの内側」
                // 扱いになる。await によるタスク中断はサポートしない
                // （`OpCode::Await` 参照）。
                self.native_call_depth += 1;
                let r = self.run_until(depth_before);
                self.native_call_depth -= 1;
                match r {
                    Ok(RunOutcome::Value(v)) => Ok(v),
                    Ok(RunOutcome::Suspended) => {
                        Err(self.type_error("内部エラー: 予期しないタスク中断です"))
                    }
                    Err(e) => Err(e),
                }
            }
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
        self.native_call_depth += 1;
        let r = self.run_until(depth_before);
        self.native_call_depth -= 1;
        match r {
            Ok(RunOutcome::Value(v)) => Ok(v),
            Ok(RunOutcome::Suspended) => {
                Err(self.type_error("内部エラー: 予期しないタスク中断です"))
            }
            Err(e) => Err(e),
        }
    }

    fn make_function_frame(f: &Rc<FunctionObj>, this: Value, args: Vec<Value>) -> Frame {
        let raw_args = Rc::new(args);
        let mut frame = Frame::new(f.chunk.clone(), f.upvalues.clone(), raw_args);
        if !f.is_arrow && !frame.locals.is_empty() {
            frame.locals[0].set(this);
        }
        frame
    }

    fn push_function_frame(&mut self, f: &Rc<FunctionObj>, this: Value, args: Vec<Value>) {
        self.frames.push(Self::make_function_frame(f, this, args));
    }

    /// async 関数呼び出し: 呼び出し元のフレーム列を退避し、この呼び出し
    /// 専用の新しいフレーム列で本体を「最初の await または完了」まで
    /// 同期的に駆動してから、呼び出し元のフレーム列を復元する
    /// （async 関数はここまでを呼び出し時点で同期的に実行し、Promise を
    /// 返した時点で呼び出し元へ制御を戻す、という JS の意味論どおり）。
    fn spawn_async_task_from_function(
        &mut self,
        f: &Rc<FunctionObj>,
        this: Value,
        args: Vec<Value>,
    ) -> Value {
        let frame = Self::make_function_frame(f, this, args);
        let caller_frames = std::mem::replace(&mut self.frames, vec![frame]);
        let (promise_val, promise_obj) = self.new_pending_promise();
        self.drive_current_task(promise_obj);
        self.frames = caller_frames;
        promise_val
    }

    /// `self.frames` に既にセットされているフレーム列（1 つのタスク）を、
    /// 完了または次の `await` での中断まで駆動する。完了したらその結果で
    /// `owner_promise` を確定させる。中断した場合は何もしない（継続は
    /// `OpCode::Await` の処理内で既に登録済み）。
    fn drive_current_task(&mut self, owner_promise: ObjRef) {
        let saved_owner = self.current_owner_promise.replace(owner_promise.clone());
        let saved_depth = self.native_call_depth;
        self.native_call_depth = 0;
        let outcome = self.run_until(0);
        self.native_call_depth = saved_depth;
        self.current_owner_promise = saved_owner;
        match outcome {
            Ok(RunOutcome::Value(v)) => self.resolve_promise(owner_promise, v),
            Ok(RunOutcome::Suspended) => {}
            Err(e) => self.settle_promise(owner_promise, Err(e)),
        }
    }

    /// マイクロタスク `ResumeAwait` の処理本体: 保存しておいたフレーム列を
    /// 復元し、await の結果（解決値 or 例外）を渡してタスクを再開する。
    fn resume_continuation(&mut self, cont: AwaitContinuation, outcome: Result<Value, Value>) {
        let AwaitContinuation {
            frames,
            owner_promise,
        } = cont;
        let caller_frames = std::mem::replace(&mut self.frames, frames);
        let mut already_done = false;
        match outcome {
            Ok(v) => self.push(v),
            Err(e) => {
                // await 地点で投げ直す。unwind が例外テーブルで捕まえられなければ
                // タスク全体が例外で終了する（このタスクの frames は既に
                // 空になっている）。
                if let Err(final_err) = self.unwind(e, 0) {
                    self.settle_promise(owner_promise.clone(), Err(final_err));
                    already_done = true;
                }
            }
        }
        if !already_done {
            self.drive_current_task(owner_promise);
        }
        self.frames = caller_frames;
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
                    if fo.is_async {
                        let promise = self.spawn_async_task_from_function(&fo, this, args);
                        return Ok(CallOutcome::Immediate(promise));
                    }
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
                    self.track_value_tree(&v);
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
            let inst_rc: ObjRef = self.alloc_obj_ref(instance);
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
            let v = nf(self, Value::Undefined, &args)?;
            self.track_value_tree(&v);
            return Ok(v);
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
            let inst_rc: ObjRef = self.alloc_obj_ref(ObjData {
                kind: ObjKind::Plain,
                props: PropMap::new(),
                proto,
            });
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
                    "global" => Some(Value::Bool(re.global)),
                    "ignoreCase" => Some(Value::Bool(re.ignore_case)),
                    "multiline" => Some(Value::Bool(re.multiline)),
                    "dotAll" => Some(Value::Bool(re.dot_all)),
                    "unicode" => Some(Value::Bool(re.unicode)),
                    "sticky" => Some(Value::Bool(false)),
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
                ObjKind::Promise(_) => Some(KindTag::Promise),
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
                        .map(|(k, val)| self.alloc_array(vec![k.clone(), val.clone()]))
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

    // =================================================================
    // T3: Promise / マイクロタスクキュー / イベントループ
    // =================================================================

    /// pending 状態の新しい Promise を作る。
    pub fn new_pending_promise(&mut self) -> (Value, ObjRef) {
        let obj: ObjRef =
            self.alloc_obj_ref(ObjData::new(ObjKind::Promise(PromiseData::pending())));
        (Value::Object(obj.clone()), obj)
    }

    /// ホストが後から resolve/reject する Promise を作る
    /// （`Interpreter::register_native_async` から使う）。
    pub fn new_host_promise(&mut self) -> (Value, HostPromise) {
        let (v, obj) = self.new_pending_promise();
        self.pending_host_count += 1;
        (v, HostPromise { promise: obj })
    }

    fn settle_host_promise(&mut self, promise: ObjRef, outcome: Result<Value, Value>) {
        let was_pending = matches!(
            promise.borrow().kind,
            ObjKind::Promise(ref pd) if matches!(pd.state, PromiseState::Pending)
        );
        if was_pending {
            self.pending_host_count = self.pending_host_count.saturating_sub(1);
        }
        match outcome {
            Ok(v) => self.resolve_promise(promise, v),
            Err(e) => self.settle_promise(promise, Err(e)),
        }
    }

    /// 値を Promise へ強制変換する（`await` と `Promise.resolve` が共有する）。
    /// 既に本物の Promise ならそれ自身を返す。それ以外は新しい Promise を
    /// 作って `resolve_promise` に解決させる。
    pub fn coerce_to_promise(&mut self, v: Value) -> ObjRef {
        if let Value::Object(o) = &v {
            if matches!(o.borrow().kind, ObjKind::Promise(_)) {
                return o.clone();
            }
        }
        let (_, p) = self.new_pending_promise();
        self.resolve_promise(p.clone(), v);
        p
    }

    /// Promise の「解決」手続き（thenable の吸収込み）。
    /// - `value` が本物の Promise なら、その決着を待って自分もそれで決着する
    ///   （チェーン連結）。
    /// - `value` が汎用 thenable（呼び出し可能な `then` プロパティを持つ
    ///   オブジェクト）なら、`then(resolve, reject)` をマイクロタスクとして
    ///   呼び出す。
    /// - それ以外はプレーンな値として即座に fulfill する。
    pub fn resolve_promise(&mut self, promise: ObjRef, value: Value) {
        if let Value::Object(o) = &value {
            if Rc::ptr_eq(o, &promise) {
                let err = self.type_error("Promise を自分自身の解決値にすることはできません");
                self.settle_promise(promise, Err(err));
                return;
            }
            let is_native_promise = matches!(o.borrow().kind, ObjKind::Promise(_));
            if is_native_promise {
                self.add_reaction(
                    o,
                    PromiseReaction::Then {
                        on_fulfilled: None,
                        on_rejected: None,
                        result_promise: promise,
                    },
                );
                return;
            }
            let then_fn = {
                let b = o.borrow();
                match b.props.get("then") {
                    Some(PropSlot::Value(v)) if is_callable(v) => Some(v.clone()),
                    _ => None,
                }
            };
            if let Some(then_fn) = then_fn {
                self.microtasks.push_back(MicrotaskJob::ResolveThenable {
                    promise,
                    thenable: value,
                    then_fn,
                });
                return;
            }
        }
        self.settle_promise(promise, Ok(value));
    }

    /// Promise を直接 fulfilled/rejected へ確定させる（thenable 吸収はしない。
    /// `resolve_promise` の内部実装、および reject 専用パスから使う）。
    /// 既に決着済みなら無視する（二重解決は無効）。
    pub fn settle_promise(&mut self, promise: ObjRef, outcome: Result<Value, Value>) {
        let reactions = {
            let mut b = promise.borrow_mut();
            let ObjKind::Promise(pd) = &mut b.kind else {
                return;
            };
            if !matches!(pd.state, PromiseState::Pending) {
                return;
            }
            pd.state = match &outcome {
                Ok(v) => PromiseState::Fulfilled(v.clone()),
                Err(e) => PromiseState::Rejected(e.clone()),
            };
            std::mem::take(&mut pd.reactions)
        };
        if reactions.is_empty() && outcome.is_err() {
            self.maybe_unhandled.push(promise.clone());
        }
        for r in reactions {
            self.schedule_reaction(r, outcome.clone());
        }
    }

    /// Promise に反応（`.then`/`.catch`/`.finally`/`await`）を登録する。
    /// 既に決着済みならすぐにマイクロタスクとしてスケジュールする。
    pub fn add_promise_reaction(&mut self, promise: &ObjRef, reaction: PromiseReaction) {
        self.add_reaction(promise, reaction);
    }

    fn add_reaction(&mut self, promise: &ObjRef, reaction: PromiseReaction) {
        enum St {
            Pending,
            Settled(Result<Value, Value>),
        }
        let st = {
            let mut b = promise.borrow_mut();
            let ObjKind::Promise(pd) = &mut b.kind else {
                return;
            };
            pd.handled = true;
            match &pd.state {
                PromiseState::Pending => St::Pending,
                PromiseState::Fulfilled(v) => St::Settled(Ok(v.clone())),
                PromiseState::Rejected(e) => St::Settled(Err(e.clone())),
            }
        };
        match st {
            St::Pending => {
                let mut b = promise.borrow_mut();
                if let ObjKind::Promise(pd) = &mut b.kind {
                    pd.reactions.push(reaction);
                }
            }
            St::Settled(outcome) => self.schedule_reaction(reaction, outcome),
        }
    }

    fn schedule_reaction(&mut self, reaction: PromiseReaction, outcome: Result<Value, Value>) {
        match reaction {
            PromiseReaction::Then {
                on_fulfilled,
                on_rejected,
                result_promise,
            } => self.microtasks.push_back(MicrotaskJob::Then {
                on_fulfilled,
                on_rejected,
                result_promise,
                outcome,
            }),
            PromiseReaction::Await(cont) => self
                .microtasks
                .push_back(MicrotaskJob::ResumeAwait { cont, outcome }),
        }
    }

    fn run_microtask_job(&mut self, job: MicrotaskJob) {
        match job {
            MicrotaskJob::Then {
                on_fulfilled,
                on_rejected,
                result_promise,
                outcome,
            } => match outcome {
                Ok(v) => match on_fulfilled {
                    Some(f) => match self.call_value(f, Value::Undefined, &[v]) {
                        Ok(r) => self.resolve_promise(result_promise, r),
                        Err(e) => self.settle_promise(result_promise, Err(e)),
                    },
                    None => self.resolve_promise(result_promise, v),
                },
                Err(e) => match on_rejected {
                    Some(f) => match self.call_value(f, Value::Undefined, &[e]) {
                        Ok(r) => self.resolve_promise(result_promise, r),
                        Err(e2) => self.settle_promise(result_promise, Err(e2)),
                    },
                    None => self.settle_promise(result_promise, Err(e)),
                },
            },
            MicrotaskJob::ResolveThenable {
                promise,
                thenable,
                then_fn,
            } => self.run_resolve_thenable(promise, thenable, then_fn),
            MicrotaskJob::ResumeAwait { cont, outcome } => self.resume_continuation(cont, outcome),
        }
    }

    /// PromiseResolveThenableJob 相当: `thenable.then(resolve, reject)` を
    /// 呼び出す。resolve/reject はどちらか一方が最初の 1 回しか効かない。
    fn run_resolve_thenable(&mut self, promise: ObjRef, thenable: Value, then_fn: Value) {
        let called: Rc<RefCell<bool>> = Rc::new(RefCell::new(false));
        let p1 = Rc::downgrade(&promise);
        let called1 = called.clone();
        let resolve_fn = self.alloc_native_with_captures(
            "",
            1,
            vec![promise.clone()],
            Rc::new(move |vm, _this, args| {
                if *called1.borrow() {
                    return Ok(Value::Undefined);
                }
                *called1.borrow_mut() = true;
                let v = args.first().cloned().unwrap_or(Value::Undefined);
                if let Some(p) = p1.upgrade() {
                    vm.resolve_promise(p, v);
                }
                Ok(Value::Undefined)
            }),
        );
        let p2 = Rc::downgrade(&promise);
        let called2 = called.clone();
        let reject_fn = self.alloc_native_with_captures(
            "",
            1,
            vec![promise.clone()],
            Rc::new(move |vm, _this, args| {
                if *called2.borrow() {
                    return Ok(Value::Undefined);
                }
                *called2.borrow_mut() = true;
                let v = args.first().cloned().unwrap_or(Value::Undefined);
                if let Some(p) = p2.upgrade() {
                    vm.settle_promise(p, Err(v));
                }
                Ok(Value::Undefined)
            }),
        );
        if let Err(e) = self.call_value(then_fn, thenable, &[resolve_fn, reject_fn]) {
            if !*called.borrow() {
                *called.borrow_mut() = true;
                self.settle_promise(promise, Err(e));
            }
        }
    }

    /// マイクロタスクキューを完全に汲み尽くす。キューが空になった時点で
    /// まだ誰にも observe されていない reject があればホストへ報告する。
    /// 戻り値は「これ以上ホストが介入しない限り進められる処理が残っているか」。
    pub fn run_event_loop(&mut self) -> PumpState {
        while let Some(job) = self.microtasks.pop_front() {
            self.run_microtask_job(job);
        }
        if !self.maybe_unhandled.is_empty() {
            let list = std::mem::take(&mut self.maybe_unhandled);
            for p in list {
                let report = {
                    let b = p.borrow();
                    match &b.kind {
                        ObjKind::Promise(pd) => {
                            !pd.handled && matches!(pd.state, PromiseState::Rejected(_))
                        }
                        _ => false,
                    }
                };
                if report {
                    let val = {
                        let b = p.borrow();
                        match &b.kind {
                            ObjKind::Promise(pd) => match &pd.state {
                                PromiseState::Rejected(v) => v.clone(),
                                _ => Value::Undefined,
                            },
                            _ => Value::Undefined,
                        }
                    };
                    (self.on_unhandled_rejection)(&val);
                }
            }
        }
        if self.native_call_depth == 0 {
            self.maybe_collect_garbage();
        }
        if self.pending_host_count > 0 {
            PumpState::Waiting
        } else {
            PumpState::Idle
        }
    }
}

impl Default for Vm {
    fn default() -> Self {
        Self::new()
    }
}

// =====================================================================
// GC トレースヘルパー
// =====================================================================

fn gc_mark_obj(obj: &ObjRef, marked: &mut HashSet<usize>, worklist: &mut Vec<ObjRef>) {
    let ptr = Rc::as_ptr(obj) as usize;
    if marked.insert(ptr) {
        worklist.push(obj.clone());
    }
}

fn gc_mark_value(v: &Value, marked: &mut HashSet<usize>, worklist: &mut Vec<ObjRef>) {
    if let Value::Object(o) = v {
        gc_mark_obj(o, marked, worklist);
    }
}

fn gc_mark_frame(frame: &Frame, marked: &mut HashSet<usize>, worklist: &mut Vec<ObjRef>) {
    for slot in &frame.locals {
        match slot {
            LocalSlot::Direct(v) => gc_mark_value(v, marked, worklist),
            LocalSlot::Boxed(cell) => {
                if let Ok(v) = cell.try_borrow() {
                    gc_mark_value(&v, marked, worklist);
                }
            }
        }
    }
    for cell in frame.upvalues.iter() {
        if let Ok(v) = cell.try_borrow() {
            gc_mark_value(&v, marked, worklist);
        }
    }
    for v in &frame.stack {
        gc_mark_value(v, marked, worklist);
    }
    for v in frame.raw_args.iter() {
        gc_mark_value(v, marked, worklist);
    }
}

fn gc_visit_value(v: &Value, f: &mut impl FnMut(&ObjRef)) {
    if let Value::Object(o) = v {
        f(o);
    }
}

fn gc_visit_frame_raw(frame: &Frame, f: &mut impl FnMut(&ObjRef)) {
    for slot in &frame.locals {
        match slot {
            LocalSlot::Direct(v) => gc_visit_value(v, f),
            LocalSlot::Boxed(cell) => {
                if let Ok(v) = cell.try_borrow() {
                    gc_visit_value(&v, f);
                }
            }
        }
    }
    for cell in frame.upvalues.iter() {
        if let Ok(v) = cell.try_borrow() {
            gc_visit_value(&v, f);
        }
    }
    for v in &frame.stack {
        gc_visit_value(v, f);
    }
    for v in frame.raw_args.iter() {
        gc_visit_value(v, f);
    }
}

fn gc_visit_obj_outgoing(obj: &ObjRef, f: &mut impl FnMut(&ObjRef)) {
    let Ok(b) = obj.try_borrow() else {
        return;
    };
    for (_, slot) in b.props.iter() {
        match slot {
            PropSlot::Value(v) => gc_visit_value(v, f),
            PropSlot::Accessor { get, set } => {
                if let Some(v) = get {
                    gc_visit_value(v, f);
                }
                if let Some(v) = set {
                    gc_visit_value(v, f);
                }
            }
        }
    }
    if let Some(proto) = &b.proto {
        f(proto);
    }
    match &b.kind {
        ObjKind::Plain | ObjKind::Error | ObjKind::Regex(_) => {}
        ObjKind::Array(items) => {
            for v in items {
                gc_visit_value(v, f);
            }
        }
        ObjKind::Map(m) => {
            for (k, v) in &m.entries {
                gc_visit_value(k, f);
                gc_visit_value(v, f);
            }
        }
        ObjKind::Set(s) => {
            for v in &s.values {
                gc_visit_value(v, f);
            }
        }
        ObjKind::Function(fo) => {
            for cell in fo.upvalues.iter() {
                if let Ok(v) = cell.try_borrow() {
                    gc_visit_value(&v, f);
                }
            }
        }
        ObjKind::Native(n) => {
            for cap in &n.captures {
                f(cap);
            }
        }
        ObjKind::Class(cd) => {
            f(&cd.prototype);
            if let Some(ctor) = &cd.ctor {
                for cell in ctor.upvalues.iter() {
                    if let Ok(v) = cell.try_borrow() {
                        gc_visit_value(&v, f);
                    }
                }
            }
            for (_, init) in &cd.instance_fields {
                if let Some(fo) = init {
                    for cell in fo.upvalues.iter() {
                        if let Ok(v) = cell.try_borrow() {
                            gc_visit_value(&v, f);
                        }
                    }
                }
            }
        }
        ObjKind::Promise(pd) => {
            match &pd.state {
                PromiseState::Pending => {}
                PromiseState::Fulfilled(v) | PromiseState::Rejected(v) => gc_visit_value(v, f),
            }
            for r in &pd.reactions {
                match r {
                    PromiseReaction::Then {
                        on_fulfilled,
                        on_rejected,
                        result_promise,
                    } => {
                        if let Some(v) = on_fulfilled {
                            gc_visit_value(v, f);
                        }
                        if let Some(v) = on_rejected {
                            gc_visit_value(v, f);
                        }
                        f(result_promise);
                    }
                    PromiseReaction::Await(cont) => {
                        for frame in &cont.frames {
                            gc_visit_frame_raw(frame, f);
                        }
                        f(&cont.owner_promise);
                    }
                }
            }
        }
    }
}

/// 候補集合の内部参照カウント差し引き専用ビジタ。
/// `ObjData` に直接所有される参照はすべて 1 対 1 で訪問し、複数オブジェクト間で
/// 共有され得る中間 `Rc`（`FunctionObj`, アップバリューセル `Rc<RefCell<Value>>`,
/// `Frame::raw_args`）はポインタ単位で重複排除して exact な参照数を保つ。
fn gc_visit_obj_data_dedup(
    b: &ObjData,
    seen_fns: &mut HashSet<usize>,
    seen_cells: &mut HashSet<usize>,
    seen_raw_args: &mut HashSet<usize>,
    f: &mut impl FnMut(&ObjRef),
) {
    let visit_cell =
        |cell: &Rc<RefCell<Value>>, seen_cells: &mut HashSet<usize>, f: &mut dyn FnMut(&ObjRef)| {
            if seen_cells.insert(Rc::as_ptr(cell) as usize) {
                if let Ok(v) = cell.try_borrow() {
                    if let Value::Object(o) = &*v {
                        f(o);
                    }
                }
            }
        };
    let visit_fn = |fo: &Rc<FunctionObj>,
                    seen_fns: &mut HashSet<usize>,
                    seen_cells: &mut HashSet<usize>,
                    f: &mut dyn FnMut(&ObjRef)| {
        if seen_fns.insert(Rc::as_ptr(fo) as usize) {
            for cell in fo.upvalues.iter() {
                visit_cell(cell, seen_cells, f);
            }
        }
    };

    for (_, slot) in b.props.iter() {
        match slot {
            PropSlot::Value(v) => gc_visit_value(v, f),
            PropSlot::Accessor { get, set } => {
                if let Some(v) = get {
                    gc_visit_value(v, f);
                }
                if let Some(v) = set {
                    gc_visit_value(v, f);
                }
            }
        }
    }
    if let Some(proto) = &b.proto {
        f(proto);
    }
    match &b.kind {
        ObjKind::Plain | ObjKind::Error | ObjKind::Regex(_) => {}
        ObjKind::Array(items) => {
            for v in items {
                gc_visit_value(v, f);
            }
        }
        ObjKind::Map(m) => {
            for (k, v) in &m.entries {
                gc_visit_value(k, f);
                gc_visit_value(v, f);
            }
        }
        ObjKind::Set(s) => {
            for v in &s.values {
                gc_visit_value(v, f);
            }
        }
        ObjKind::Function(fo) => visit_fn(fo, seen_fns, seen_cells, f),
        ObjKind::Native(n) => {
            for cap in &n.captures {
                f(cap);
            }
        }
        ObjKind::Class(cd) => {
            f(&cd.prototype);
            if let Some(ctor) = &cd.ctor {
                visit_fn(ctor, seen_fns, seen_cells, f);
            }
            for (_, init) in &cd.instance_fields {
                if let Some(fo) = init {
                    visit_fn(fo, seen_fns, seen_cells, f);
                }
            }
        }
        ObjKind::Promise(pd) => {
            match &pd.state {
                PromiseState::Pending => {}
                PromiseState::Fulfilled(v) | PromiseState::Rejected(v) => gc_visit_value(v, f),
            }
            for r in &pd.reactions {
                match r {
                    PromiseReaction::Then {
                        on_fulfilled,
                        on_rejected,
                        result_promise,
                    } => {
                        if let Some(v) = on_fulfilled {
                            gc_visit_value(v, f);
                        }
                        if let Some(v) = on_rejected {
                            gc_visit_value(v, f);
                        }
                        f(result_promise);
                    }
                    PromiseReaction::Await(cont) => {
                        for frame in &cont.frames {
                            for slot in &frame.locals {
                                match slot {
                                    LocalSlot::Direct(v) => gc_visit_value(v, f),
                                    LocalSlot::Boxed(cell) => visit_cell(cell, seen_cells, f),
                                }
                            }
                            for cell in frame.upvalues.iter() {
                                visit_cell(cell, seen_cells, f);
                            }
                            for v in &frame.stack {
                                gc_visit_value(v, f);
                            }
                            if seen_raw_args.insert(Rc::as_ptr(&frame.raw_args) as usize) {
                                for v in frame.raw_args.iter() {
                                    gc_visit_value(v, f);
                                }
                            }
                        }
                        f(&cont.owner_promise);
                    }
                }
            }
        }
    }
}

enum CallOutcome {
    Immediate(Value),
    FramePushed,
}

/// 呼び出し可能（関数・ネイティブ関数）かどうか。クラスは `new` 経由でしか
/// 呼べないのでここには含めない。
pub fn is_callable(v: &Value) -> bool {
    matches!(
        v,
        Value::Object(o) if matches!(o.borrow().kind, ObjKind::Function(_) | ObjKind::Native(_))
    )
}

pub fn array_values(v: &Value) -> Vec<Value> {
    if let Value::Object(o) = v {
        if let ObjKind::Array(items) = &o.borrow().kind {
            return items.clone();
        }
    }
    Vec::new()
}

/// コンパイル済み正規表現から `RegExp` インスタンスを作る（正規表現リテラル・
/// `new RegExp(...)` の両方から使う共通ヘルパー）。`lastIndex` は own
/// プロパティとして持たせる（`source`/`flags`/`global` 等は読み取り専用の
/// `ObjKind::Regex` 側フォールバックで賄うため、own プロパティにはしない）。
pub fn new_regex_value(re: Rc<crate::regex::Regex>) -> Value {
    let mut o = ObjData::new(ObjKind::Regex(re));
    o.props.set_value(Rc::from("lastIndex"), Value::Number(0.0));
    Value::Object(Rc::new(RefCell::new(o)))
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
                ObjKind::Promise(pd) => match &pd.state {
                    PromiseState::Pending => "Promise { <pending> }".to_string(),
                    PromiseState::Fulfilled(v) => format!("Promise {{ {} }}", to_string_basic(v)),
                    PromiseState::Rejected(v) => {
                        format!("Promise {{ <rejected> {} }}", to_string_basic(v))
                    }
                },
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{compiler, parse_module, Interpreter};

    fn compile(src: &str) -> Rc<Chunk> {
        let m = parse_module(src, "<test>").unwrap();
        compiler::compile_module(&m).unwrap()
    }

    fn run_exports(interp: &mut Interpreter, src: &str) -> Vec<(Rc<str>, Value)> {
        interp.run_module(src, "<test>").unwrap()
    }

    fn export_val(exports: &[(Rc<str>, Value)], name: &str) -> Value {
        exports
            .iter()
            .find(|(k, _)| &**k == name)
            .map(|(_, v)| v.clone())
            .unwrap()
    }

    // =================================================================
    // Part 1: 非キャプチャローカル変数の非ボックス化テスト
    // =================================================================

    #[test]
    fn uncaptured_locals_stay_direct_and_only_captured_slots_are_boxed() {
        let src = r#"
            export function demo(a: number, b: number) {
                let uncaptured = a + 10;
                let captured = b + 20;
                let alsoUncaptured = uncaptured * 2;
                const fn1 = () => captured;
                captured += 5;
                return fn1() + alsoUncaptured;
            }
            export const result = demo(3, 7);
        "#;
        let chunk = compile(src);

        // `demo` の FunctionTemplate を定数表から取り出して captured_locals を検査する
        let demo_tpl = chunk
            .consts
            .iter()
            .find_map(|c| match c {
                Const::FunctionTemplate(t) if &*t.name == "demo" => Some(t.clone()),
                _ => None,
            })
            .expect("demo FunctionTemplate should exist");

        // demo のローカルスロットのうち captured されるのは `captured` 1 個だけ
        let boxed_count = demo_tpl
            .chunk
            .captured_locals
            .iter()
            .filter(|&&c| c)
            .count();
        let direct_count = demo_tpl
            .chunk
            .captured_locals
            .iter()
            .filter(|&&c| !c)
            .count();
        assert_eq!(
            boxed_count, 1,
            "captured_locals: {:?}",
            demo_tpl.chunk.captured_locals
        );
        assert!(direct_count >= 5, "non-captured locals should stay Direct");

        // Frame::new が実際に Direct / Boxed を正しく作り分けることも直接確認
        let frame = Frame::new(
            demo_tpl.chunk.clone(),
            Rc::new(Vec::new()),
            Rc::new(Vec::new()),
        );
        assert_eq!(frame.locals.iter().filter(|s| s.is_boxed()).count(), 1);
        assert_eq!(
            frame.locals.iter().filter(|s| s.is_direct()).count(),
            direct_count
        );

        // 実行結果が正しいことを確認: (7 + 20 + 5) + ((3 + 10) * 2) = 32 + 26 = 58
        let mut interp = Interpreter::new();
        let exports = run_exports(&mut interp, src);
        match export_val(&exports, "result") {
            Value::Number(n) => assert_eq!(n, 58.0),
            other => panic!("expected 58, got {other:?}"),
        }
    }

    #[test]
    fn uncaptured_for_let_loop_is_direct_and_captured_for_let_preserves_per_iter_binding() {
        let src = r#"
            function sumLoop(n: number) {
                let s = 0;
                for (let i = 0; i < n; i++) {
                    s += i;
                }
                return s;
            }
            function captureLoop() {
                const fns: Array<() => number> = [];
                for (let i = 0; i < 5; i++) {
                    fns.push(() => i);
                }
                return fns.map(f => f()).join(",");
            }
            export const sum = sumLoop(100);
            export const captured = captureLoop();
        "#;
        let chunk = compile(src);
        let sum_tpl = chunk
            .consts
            .iter()
            .find_map(|c| match c {
                Const::FunctionTemplate(t) if &*t.name == "sumLoop" => Some(t.clone()),
                _ => None,
            })
            .unwrap();
        assert!(
            sum_tpl.chunk.captured_locals.iter().all(|&c| !c),
            "sumLoop の全ローカルは非キャプチャ（Direct）であるべき: {:?}",
            sum_tpl.chunk.captured_locals
        );

        let cap_tpl = chunk
            .consts
            .iter()
            .find_map(|c| match c {
                Const::FunctionTemplate(t) if &*t.name == "captureLoop" => Some(t.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            cap_tpl.chunk.captured_locals.iter().filter(|&&c| c).count(),
            1,
            "captureLoop はループ変数 i のみ Boxed であるべき"
        );

        let mut interp = Interpreter::new();
        let exports = run_exports(&mut interp, src);
        match export_val(&exports, "sum") {
            Value::Number(n) => assert_eq!(n, 4950.0),
            other => panic!("expected 4950, got {other:?}"),
        }
        match export_val(&exports, "captured") {
            Value::Str(s) => assert_eq!(&*s, "0,1,2,3,4"),
            other => panic!("expected '0,1,2,3,4', got {other:?}"),
        }
    }

    #[test]
    fn nested_upvalues_and_arrow_captured_this_work_with_mixed_slots() {
        let src = r#"
            function makeCounter(start: number) {
                let uncap = 100;
                let count = start;
                function mid() {
                    let midUncap = 1;
                    return () => {
                        count += midUncap;
                        return count;
                    };
                }
                const inc = mid();
                return [inc(), inc(), uncap];
            }
            const obj = {
                x: 42,
                getX() {
                    let localOnly = 8;
                    const arrow = () => this.x;
                    return arrow() + localOnly;
                },
            };
            export const res = makeCounter(10).join(",");
            export const thisRes = obj.getX();
        "#;
        let mut interp = Interpreter::new();
        let exports = run_exports(&mut interp, src);
        match export_val(&exports, "res") {
            Value::Str(s) => assert_eq!(&*s, "11,12,100"),
            other => panic!("unexpected res: {other:?}"),
        }
        match export_val(&exports, "thisRes") {
            Value::Number(n) => assert_eq!(n, 50.0),
            other => panic!("unexpected thisRes: {other:?}"),
        }
    }

    // =================================================================
    // Part 2: 循環参照回収 (GC) テスト
    // =================================================================

    #[test]
    fn gc_reclaims_1000_circular_objects_and_pairs_back_to_baseline() {
        let mut interp = Interpreter::new();
        interp.collect_garbage();
        let baseline = interp.vm().live_tracked_objects();

        let src = r#"
            for (let i = 0; i < 1000; i++) {
                const selfRef: any = { id: i };
                selfRef.self = selfRef;

                const a: any = { name: "a" };
                const b: any = { name: "b" };
                a.b = b;
                b.a = a;
            }
            export const done = true;
        "#;
        // 閾値を大きくしておき、明示 collect_garbage による回収数を観測する
        interp.vm().set_gc_threshold(100_000);
        let _ = run_exports(&mut interp, src);

        let before = interp.vm().live_tracked_objects();
        assert!(
            before >= baseline + 3000,
            "GC 前は 3000 個以上の循環オブジェクトが残存しているはず: before={before}, baseline={baseline}"
        );

        let collected = interp.collect_garbage();
        let after = interp.vm().live_tracked_objects();
        assert!(
            collected >= 3000,
            "3000 個以上の循環オブジェクトが回収されるべき: collected={collected}"
        );
        assert_eq!(
            after, baseline,
            "GC 後はベースラインまで完全に回収されるべき: after={after}, baseline={baseline}"
        );
    }

    #[test]
    fn gc_reclaims_self_referencing_and_mutually_recursive_closures() {
        let mut interp = Interpreter::new();
        interp.collect_garbage();
        let baseline = interp.vm().live_tracked_objects();

        let src = r#"
            function makeSelf() {
                function f() {
                    return f;
                }
                return f() === f;
            }
            function makeMutual() {
                function a(n: number): number {
                    return n <= 0 ? 0 : b(n - 1);
                }
                function b(n: number): number {
                    return n <= 0 ? 1 : a(n - 1);
                }
                return a(4) + b(4);
            }
            let ok = 0;
            for (let i = 0; i < 200; i++) {
                if (makeSelf()) ok += 1;
                ok += makeMutual();
            }
            export const total = ok;
        "#;
        interp.vm().set_gc_threshold(100_000);
        let exports = run_exports(&mut interp, src);
        match export_val(&exports, "total") {
            Value::Number(n) => assert_eq!(n, 400.0),
            other => panic!("expected 400, got {other:?}"),
        }

        let before = interp.vm().live_tracked_objects();
        assert!(
            before >= baseline + 600,
            "before={before}, baseline={baseline}"
        );
        let collected = interp.collect_garbage();
        let after = interp.vm().live_tracked_objects();
        assert!(collected >= 600, "collected={collected}");
        assert_eq!(after, baseline, "after={after}, baseline={baseline}");
    }

    #[test]
    fn gc_reclaims_circular_arrays_classes_and_promises() {
        let mut interp = Interpreter::new();
        interp.collect_garbage();
        let baseline = interp.vm().live_tracked_objects();

        let src = r#"
            for (let i = 0; i < 100; i++) {
                // 循環配列
                const arr: any = [];
                arr.push(arr);

                // クラス自身と prototype.constructor の循環
                class LocalClass {
                    v = i;
                    get() { return this.v; }
                }
                const inst = new LocalClass();
                (inst as any).self = inst;

                // Promise + .then クロージャ循環
                const p1: any = new Promise(() => {});
                p1.then(() => p1);

                // Promise + resolver ネイティブ捕捉循環
                let res: any;
                const p2: any = new Promise((r: any) => { res = r; });
                p2.res = res;

                // 解決済み Promise とオブジェクトの循環
                const box: any = {};
                const p3 = Promise.resolve(box);
                box.p = p3;
            }
            export const done = true;
        "#;
        interp.vm().set_gc_threshold(100_000);
        let _ = run_exports(&mut interp, src);

        let before = interp.vm().live_tracked_objects();
        assert!(
            before >= baseline + 500,
            "before={before}, baseline={baseline}"
        );

        let collected = interp.collect_garbage();
        let after = interp.vm().live_tracked_objects();
        assert!(collected >= 500, "collected={collected}");
        assert_eq!(after, baseline, "after={after}, baseline={baseline}");
    }

    #[test]
    fn automatic_gc_threshold_reclaims_cycles_during_execution() {
        let mut interp = Interpreter::new();
        interp.collect_garbage();
        let baseline = interp.vm().live_tracked_objects();

        // 明示的に collect_garbage を呼ばず、自動 GC の閾値超過だけで回収されることを確認
        let src = r#"
            for (let i = 0; i < 6000; i++) {
                const a: any = {};
                a.self = a;
            }
            export const done = true;
        "#;
        let _ = run_exports(&mut interp, src);

        let live_after_run = interp.vm().live_tracked_objects();
        assert!(
            live_after_run < baseline + 4000,
            "自動 GC が発火して生存オブジェクト数が抑えられているべき: live={live_after_run}, baseline={baseline}"
        );
        interp.collect_garbage();
        assert_eq!(interp.vm().live_tracked_objects(), baseline);
    }

    #[test]
    fn gc_preserves_cyclic_objects_held_by_external_rust_value() {
        let mut interp = Interpreter::new();
        interp.collect_garbage();
        let baseline = interp.vm().live_tracked_objects();

        let src = r#"
            const a: any = { tag: "keep-a" };
            const b: any = { tag: "keep-b" };
            a.b = b;
            b.a = a;

            // こちらは到達不能な循環ゴミ
            {
                const trash: any = { tag: "trash" };
                trash.self = trash;
            }
            export const held = a;
        "#;
        let exports = run_exports(&mut interp, src);
        let held = export_val(&exports, "held");
        drop(exports);

        // VM ルートには `held` がもう存在しないが、Rust ローカル変数 `held` が強参照を持っている
        let collected = interp.collect_garbage();
        assert!(collected >= 1, "trash は回収されるべき");

        // 外部保持された `held` とその先の `b` は破壊されていないこと
        let tag_a = interp.vm().get_prop(&held, "tag").unwrap();
        let b_val = interp.vm().get_prop(&held, "b").unwrap();
        let tag_b = interp.vm().get_prop(&b_val, "tag").unwrap();
        let back_a = interp.vm().get_prop(&b_val, "a").unwrap();
        assert!(matches!(tag_a, Value::Str(ref s) if &**s == "keep-a"));
        assert!(matches!(tag_b, Value::Str(ref s) if &**s == "keep-b"));
        assert!(strict_eq(&back_a, &held));

        // 外部参照をすべて手放してから GC すると a・b ともに完全に回収される
        drop(back_a);
        drop(b_val);
        drop(held);
        let collected2 = interp.collect_garbage();
        assert_eq!(
            collected2, 2,
            "外部参照解放後は a と b の 2 個が回収されるべき"
        );
        assert_eq!(interp.vm().live_tracked_objects(), baseline);
    }

    #[test]
    fn gc_preserves_suspended_async_task_waiting_on_external_host_promise() {
        let mut interp = Interpreter::new();
        let saved_handle: Rc<RefCell<Option<HostPromise>>> = Rc::new(RefCell::new(None));
        let h_clone = saved_handle.clone();
        interp.register_native_async("waitHost", 0, move |_vm, _this, _args, handle| {
            *h_clone.borrow_mut() = Some(handle);
        });

        let src = r#"
            export let observed = 0;
            async function worker() {
                const localObj: any = { bonus: 7 };
                localObj.self = localObj;
                const v = await waitHost();
                observed = v + localObj.bonus;
            }
            worker();
            // ゴミ循環も同時に作る
            for (let i = 0; i < 50; i++) {
                const g: any = {};
                g.self = g;
            }
        "#;
        let _ = run_exports(&mut interp, src);
        assert!(saved_handle.borrow().is_some());

        // 中断中に GC を複数回走らせても、HostPromise と継続フレーム内の localObj は守られる
        let collected = interp.collect_garbage();
        assert!(collected >= 50);

        let handle = saved_handle.borrow_mut().take().unwrap();
        handle.resolve(interp.vm(), Value::Number(35.0));
        assert_eq!(interp.run_event_loop(), PumpState::Idle);

        // async 関数が正しく再開して observed = 35 + 7 = 42 に到達したこと
        // (トップレベル export は return 時点の値スナップショットなのでグローバル経由でも検証)
        let mut interp2 = Interpreter::new();
        let saved2: Rc<RefCell<Option<HostPromise>>> = Rc::new(RefCell::new(None));
        let h2 = saved2.clone();
        interp2.register_native_async("waitHost", 0, move |_vm, _this, _args, handle| {
            *h2.borrow_mut() = Some(handle);
        });
        let box_obj = Value::new_plain_object();
        interp2.define_global("outBox", box_obj.clone());
        let _ = run_exports(
            &mut interp2,
            r#"
                async function worker() {
                    const localObj: any = { bonus: 7 };
                    localObj.self = localObj;
                    const v = await waitHost();
                    outBox.result = v + localObj.bonus;
                }
                worker();
            "#,
        );
        interp2.collect_garbage();
        saved2
            .borrow_mut()
            .take()
            .unwrap()
            .resolve(interp2.vm(), Value::Number(35.0));
        assert_eq!(interp2.run_event_loop(), PumpState::Idle);
        let res = interp2.vm().get_prop(&box_obj, "result").unwrap();
        assert!(matches!(res, Value::Number(n) if n == 42.0), "got {res:?}");
    }
}
