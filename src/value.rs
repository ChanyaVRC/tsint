//! ランタイム値の表現。
//!
//! 設計方針:
//! - プリミティブは `Value` に直接載せる（undefined/null/bool/f64/文字列）。
//!   文字列は不変（`Rc<str>`）。
//! - オブジェクト系（プレーンオブジェクト・配列・関数・クラス・Map/Set・
//!   エラー）はすべて `Rc<RefCell<ObjData>>` を共有する `Value::Object` に
//!   統一する。`ObjKind` で実体の種類を分岐する。
//! - ローカル変数スロットは内側クロージャから捕捉されるものだけを
//!   `Rc<RefCell<Value>>`（`LocalSlot::Boxed`）で持ち、非キャプチャ変数は
//!   ヒープ確保なしの `LocalSlot::Direct(Value)` として保持する（`vm.rs` 参照）。
//! - `Value::Uninit` は TDZ 判定専用の内部マーカー。JS コードから直接
//!   観測されることはない。

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::rc::Rc;

use crate::bytecode::Chunk;

/// ネイティブ関数。`this` と引数を受け取り、戻り値または例外(throw)値を返す。
pub type NativeFn = Rc<dyn Fn(&mut crate::vm::Vm, Value, &[Value]) -> Result<Value, Value>>;

#[derive(Clone)]
pub enum Value {
    Undefined,
    Null,
    Bool(bool),
    Number(f64),
    Str(Rc<str>),
    Object(Rc<RefCell<ObjData>>),
    /// TDZ 用の内部マーカー（未初期化のローカル/アップバリュー）。
    Uninit,
}

impl Value {
    pub fn str(s: impl Into<Rc<str>>) -> Value {
        Value::Str(s.into())
    }

    pub fn is_nullish(&self) -> bool {
        matches!(self, Value::Undefined | Value::Null)
    }

    pub fn is_object(&self) -> bool {
        matches!(self, Value::Object(_))
    }

    pub fn as_object(&self) -> Option<&Rc<RefCell<ObjData>>> {
        match self {
            Value::Object(o) => Some(o),
            _ => None,
        }
    }

    /// ToBoolean（JS 準拠の主要規則）。
    pub fn to_bool(&self) -> bool {
        match self {
            Value::Undefined | Value::Null => false,
            Value::Bool(b) => *b,
            Value::Number(n) => *n != 0.0 && !n.is_nan(),
            Value::Str(s) => !s.is_empty(),
            Value::Object(_) => true,
            Value::Uninit => false,
        }
    }

    pub fn type_of(&self) -> &'static str {
        match self {
            Value::Undefined => "undefined",
            Value::Null => "object",
            Value::Bool(_) => "boolean",
            Value::Number(_) => "number",
            Value::Str(_) => "string",
            Value::Uninit => "undefined",
            Value::Object(o) => match &o.borrow().kind {
                ObjKind::Function(_) | ObjKind::Native(_) | ObjKind::Class(_) => "function",
                _ => "object",
            },
        }
    }

    pub fn new_array(items: Vec<Value>) -> Value {
        Value::Object(Rc::new(RefCell::new(ObjData::new(ObjKind::Array(items)))))
    }

    pub fn new_plain_object() -> Value {
        Value::Object(Rc::new(RefCell::new(ObjData::new(ObjKind::Plain))))
    }

    pub fn native_fn(name: &str, arity: usize, f: NativeFn) -> Value {
        Value::Object(Rc::new(RefCell::new(ObjData::new(ObjKind::Native(
            NativeObj {
                name: name.into(),
                arity,
                f,
                captures: Vec::new(),
            },
        )))))
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Undefined => write!(f, "undefined"),
            Value::Null => write!(f, "null"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Number(n) => write!(f, "{n}"),
            Value::Str(s) => write!(f, "{s:?}"),
            Value::Object(_) => write!(f, "[object]"),
            Value::Uninit => write!(f, "<uninit>"),
        }
    }
}

/// プロパティ値のスロット。アクセサ（getter/setter）と通常値を区別する。
#[derive(Clone)]
pub enum PropSlot {
    Value(Value),
    Accessor {
        get: Option<Value>,
        set: Option<Value>,
    },
}

/// `PropMap` が線形探索からハッシュ索引へ切り替えるプロパティ数の閾値。
/// 小さなオブジェクトは `HashMap` を確保せず（`index == None`）、`Vec` の
/// 線形探索のみでゼロ追加アロケーション・キャッシュフレンドリーに処理する。
const PROP_MAP_LINEAR_THRESHOLD: usize = 16;

/// 挿入順を保持するプロパティマップ。
///
/// - プロパティ数が `PROP_MAP_LINEAR_THRESHOLD` 以下の間は `entries` の線形探索のみ行い、
///   `index` は `None`（`Option<Box<HashMap<...>>>` のため追加コストは 1 ポインタ幅のみ、
///   ヒープ確保ゼロ）。
/// - 閾値を超えると遅延で `HashMap<Rc<str>, usize>` を構築し、探索・更新・削除を O(1) 化する。
/// - 索引ありの状態で削除されたスロットは `None`（トゥームストーン）として扱い、
///   トゥームストーンが半分以上に達したタイミングで圧縮・再構築することで
///   JS の挿入順と削除→再挿入（末尾移動）セマンティクスを厳密に保ちつつ償却 O(1) を達成する。
#[derive(Clone, Default)]
pub struct PropMap {
    entries: Vec<Option<(Rc<str>, PropSlot)>>,
    live_len: usize,
    // Keep `PropMap` compact (1 pointer width when `None`) for small objects below
    // `PROP_MAP_LINEAR_THRESHOLD`, which make up the vast majority of JS objects.
    #[allow(clippy::box_collection)]
    index: Option<Box<HashMap<Rc<str>, usize>>>,
}

impl PropMap {
    pub fn new() -> Self {
        Self::default()
    }

    fn find_index(&self, key: &str) -> Option<usize> {
        if let Some(ref idx_map) = self.index {
            idx_map.get(key).copied()
        } else {
            self.entries
                .iter()
                .position(|e| matches!(e, Some((k, _)) if &**k == key))
        }
    }

    fn build_index(&mut self) {
        let mut map = HashMap::with_capacity(self.live_len * 2);
        for (i, entry) in self.entries.iter().enumerate() {
            if let Some((k, _)) = entry {
                map.insert(k.clone(), i);
            }
        }
        self.index = Some(Box::new(map));
    }

    fn push_new(&mut self, key: Rc<str>, slot: PropSlot) {
        let new_idx = self.entries.len();
        if let Some(ref mut idx_map) = self.index {
            idx_map.insert(key.clone(), new_idx);
            self.entries.push(Some((key, slot)));
            self.live_len += 1;
        } else {
            self.entries.push(Some((key, slot)));
            self.live_len += 1;
            if self.live_len > PROP_MAP_LINEAR_THRESHOLD {
                self.build_index();
            }
        }
    }

    fn maybe_compact(&mut self) {
        if self.live_len == 0 {
            self.entries.clear();
            self.index = None;
            return;
        }
        let drop_index = self.live_len <= PROP_MAP_LINEAR_THRESHOLD / 2;
        if drop_index
            || (self.entries.len() > PROP_MAP_LINEAR_THRESHOLD
                && self.live_len * 2 <= self.entries.len())
        {
            self.entries.retain(|e| e.is_some());
            if drop_index {
                self.index = None;
            } else if let Some(ref mut idx_map) = self.index {
                idx_map.clear();
                for (i, entry) in self.entries.iter().enumerate() {
                    if let Some((k, _)) = entry {
                        idx_map.insert(k.clone(), i);
                    }
                }
            }
        }
    }

    pub fn get(&self, key: &str) -> Option<&PropSlot> {
        let idx = self.find_index(key)?;
        self.entries[idx].as_ref().map(|(_, v)| v)
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut PropSlot> {
        let idx = self.find_index(key)?;
        self.entries[idx].as_mut().map(|(_, v)| v)
    }

    pub fn set_value(&mut self, key: Rc<str>, value: Value) {
        if let Some(slot) = self.get_mut(&key) {
            *slot = PropSlot::Value(value);
        } else {
            self.push_new(key, PropSlot::Value(value));
        }
    }

    pub fn set_slot(&mut self, key: Rc<str>, slot: PropSlot) {
        if let Some(existing) = self.get_mut(&key) {
            *existing = slot;
        } else {
            self.push_new(key, slot);
        }
    }

    pub fn define_accessor(&mut self, key: Rc<str>, is_get: bool, f: Value) {
        if let Some(slot) = self.get_mut(&key) {
            match slot {
                PropSlot::Accessor { get, set } => {
                    if is_get {
                        *get = Some(f);
                    } else {
                        *set = Some(f);
                    }
                }
                _ => {
                    let (get, set) = if is_get {
                        (Some(f), None)
                    } else {
                        (None, Some(f))
                    };
                    *slot = PropSlot::Accessor { get, set };
                }
            }
            return;
        }
        let (get, set) = if is_get {
            (Some(f), None)
        } else {
            (None, Some(f))
        };
        self.push_new(key, PropSlot::Accessor { get, set });
    }

    pub fn delete(&mut self, key: &str) -> bool {
        if let Some(ref mut idx_map) = self.index {
            if let Some(idx) = idx_map.remove(key) {
                self.entries[idx] = None;
                self.live_len -= 1;
                self.maybe_compact();
                true
            } else {
                false
            }
        } else if let Some(idx) = self
            .entries
            .iter()
            .position(|e| matches!(e, Some((k, _)) if &**k == key))
        {
            self.entries.remove(idx);
            self.live_len -= 1;
            true
        } else {
            false
        }
    }

    pub fn keys(&self) -> impl Iterator<Item = &Rc<str>> {
        self.entries
            .iter()
            .filter_map(|e| e.as_ref().map(|(k, _)| k))
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Rc<str>, &PropSlot)> {
        self.entries
            .iter()
            .filter_map(|e| e.as_ref().map(|(k, v)| (k, v)))
    }

    pub fn len(&self) -> usize {
        self.live_len
    }

    pub fn is_empty(&self) -> bool {
        self.live_len == 0
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.live_len = 0;
        self.index = None;
    }
}

/// クラス／プロトタイプ相当。`ObjData::proto` で連鎖する。
pub type ObjRef = Rc<RefCell<ObjData>>;

pub struct ObjData {
    pub kind: ObjKind,
    pub props: PropMap,
    pub proto: Option<ObjRef>,
}

impl ObjData {
    pub fn new(kind: ObjKind) -> Self {
        ObjData {
            kind,
            props: PropMap::new(),
            proto: None,
        }
    }
}

#[derive(Clone)]
pub struct NativeObj {
    pub name: Rc<str>,
    pub arity: usize,
    pub f: NativeFn,
    /// ビルトイン（Promise の resolve/reject/finally 等）が Rust クロージャ内に
    /// Weak で参照している JS オブジェクトの強い参照を GC から可視化するためのリスト。
    pub(crate) captures: Vec<ObjRef>,
}

/// バイトコード関数（クロージャ）。
pub struct FunctionObj {
    pub name: Rc<str>,
    pub chunk: Rc<Chunk>,
    pub upvalues: Rc<Vec<Rc<RefCell<Value>>>>,
    pub is_arrow: bool,
    /// `async function`/`async () => {}`/`async` メソッド。呼び出すと本体を
    /// 即座には実行せず、専用のフレーム列（タスク）で駆動して Promise を返す
    /// （`vm.rs` の `spawn_async_task_from_function` 参照）。
    pub is_async: bool,
}

/// Map/Set はコーパスで使う分の操作を素朴な Vec で実装する（挿入順保持）。
#[derive(Default)]
pub struct MapData {
    pub entries: Vec<(Value, Value)>,
}

impl MapData {
    pub fn find(&self, key: &Value) -> Option<usize> {
        self.entries
            .iter()
            .position(|(k, _)| same_value_zero(k, key))
    }
}

#[derive(Default)]
pub struct SetData {
    pub values: Vec<Value>,
}

impl SetData {
    pub fn find(&self, v: &Value) -> Option<usize> {
        self.values.iter().position(|x| same_value_zero(x, v))
    }
}

pub enum ObjKind {
    Plain,
    Array(Vec<Value>),
    Function(Rc<FunctionObj>),
    Native(NativeObj),
    /// クラス本体。`Value::Object` 自身が呼び出し可能なコンストラクタを兼ねる。
    Class(ClassData),
    Map(MapData),
    Set(SetData),
    /// Error 系オブジェクト（`message`/`name` は props に格納。マーカーのみ）。
    Error,
    /// 正規表現（T4 本実装。`src/regex.rs` 参照）。
    Regex(Rc<crate::regex::Regex>),
    /// Promise（T3）。状態と pending 中の反応（reaction）を保持する。
    Promise(PromiseData),
}

/// Promise の 3 状態。
pub enum PromiseState {
    Pending,
    Fulfilled(Value),
    Rejected(Value),
}

/// pending 中に登録された「解決/棄却時にやること」。
/// `.then`/`.catch`/`.finally` はハンドラ呼び出し＋新しい Promise の解決、
/// `await` は中断していたフレーム列の再開として扱う。
pub enum PromiseReaction {
    Then {
        on_fulfilled: Option<Value>,
        on_rejected: Option<Value>,
        result_promise: ObjRef,
    },
    Await(AwaitContinuation),
}

/// `await` で中断した実行単位（1 回の async 関数呼び出し、またはモジュール
/// トップレベル）の継続。`frames` は中断時点のフレーム列そのもの
/// （`Vm.frames` を丸ごと `std::mem::take` したもの）。
pub struct AwaitContinuation {
    pub frames: Vec<crate::vm::Frame>,
    /// この継続が属するタスク全体が完了した時に確定させる Promise
    /// （async 関数自身の戻り値 Promise、またはモジュールトップレベル用の
    /// 内部 Promise）。
    pub owner_promise: ObjRef,
}

pub struct PromiseData {
    pub state: PromiseState,
    pub reactions: Vec<PromiseReaction>,
    /// 一度でも `.then`/`.catch`/`.finally`/`await` の対象になったかどうか。
    /// reject 時にこれが false のままだと「未捕捉の reject」としてホストへ
    /// 報告する（`Vm::run_event_loop` 参照）。
    pub handled: bool,
}

impl PromiseData {
    pub fn pending() -> Self {
        PromiseData {
            state: PromiseState::Pending,
            reactions: Vec::new(),
            handled: false,
        }
    }
}

pub struct ClassData {
    pub name: Rc<str>,
    pub ctor: Option<Rc<FunctionObj>>,
    /// インスタンス用プロトタイプオブジェクト（メソッド/getter/setter を保持）。
    pub prototype: ObjRef,
    /// インスタンスフィールド初期化子（`new` のたびに `this` = 新インスタンスで呼ぶ）。
    pub instance_fields: Vec<(Rc<str>, Option<Rc<FunctionObj>>)>,
}

/// SameValueZero（Map/Set のキー比較・Array.includes に使用）。
pub fn same_value_zero(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Undefined, Value::Undefined) => true,
        (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Number(x), Value::Number(y)) => {
            if x.is_nan() && y.is_nan() {
                true
            } else {
                x == y
            }
        }
        (Value::Str(x), Value::Str(y)) => x == y,
        (Value::Object(x), Value::Object(y)) => Rc::ptr_eq(x, y),
        (Value::Uninit, Value::Uninit) => true,
        _ => false,
    }
}

/// === strict equality（Uninit は現れない前提）。
pub fn strict_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Undefined, Value::Undefined) => true,
        (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Number(x), Value::Number(y)) => x == y, // NaN !== NaN は f64 の PartialEq が自然に満たす
        (Value::Str(x), Value::Str(y)) => x == y,
        (Value::Object(x), Value::Object(y)) => Rc::ptr_eq(x, y),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn num_of(slot: Option<&PropSlot>) -> Option<f64> {
        match slot {
            Some(PropSlot::Value(Value::Number(n))) => Some(*n),
            _ => None,
        }
    }

    fn key_vec(m: &PropMap) -> Vec<String> {
        m.keys().map(|k| k.to_string()).collect()
    }

    #[test]
    fn propmap_small_object_fast_path_preserves_js_order_and_zero_index() {
        let mut m = PropMap::new();
        assert!(m.is_empty());
        assert!(m.index.is_none());

        for i in 0..PROP_MAP_LINEAR_THRESHOLD {
            m.set_value(Rc::from(format!("k{i}")), Value::Number(i as f64));
        }
        assert_eq!(m.len(), PROP_MAP_LINEAR_THRESHOLD);
        assert!(
            m.index.is_none(),
            "閾値以下のオブジェクトは HashMap を確保しないべき"
        );

        // 既存キーの更新は挿入順を変えない
        m.set_value(Rc::from("k2"), Value::Number(200.0));
        assert_eq!(num_of(m.get("k2")), Some(200.0));
        assert_eq!(m.len(), PROP_MAP_LINEAR_THRESHOLD);

        // 削除して再挿入すると末尾に移動する（JS 仕様）
        assert!(m.delete("k2"));
        assert!(!m.delete("k2"));
        assert!(m.get("k2").is_none());
        assert_eq!(m.len(), PROP_MAP_LINEAR_THRESHOLD - 1);

        m.set_value(Rc::from("k2"), Value::Number(201.0));
        assert_eq!(m.len(), PROP_MAP_LINEAR_THRESHOLD);
        assert!(m.index.is_none());

        let mut expected: Vec<String> = (0..PROP_MAP_LINEAR_THRESHOLD)
            .filter(|&i| i != 2)
            .map(|i| format!("k{i}"))
            .collect();
        expected.push("k2".to_string());
        assert_eq!(key_vec(&m), expected);
    }

    #[test]
    fn propmap_large_object_transitions_to_index_and_preserves_js_semantics() {
        let mut m = PropMap::new();
        let n = 64;
        for i in 0..n {
            m.set_value(Rc::from(format!("p{i}")), Value::Number(i as f64));
        }
        assert!(m.index.is_some());
        assert_eq!(m.len(), n);

        // 更新は順序を変えない
        m.set_value(Rc::from("p10"), Value::Number(999.0));
        assert_eq!(num_of(m.get("p10")), Some(999.0));
        assert_eq!(m.len(), n);

        // get_mut 経由の更新
        if let Some(PropSlot::Value(v)) = m.get_mut("p20") {
            *v = Value::Number(888.0);
        } else {
            panic!("p20 should exist");
        }
        assert_eq!(num_of(m.get("p20")), Some(888.0));

        // 削除 → 再挿入で末尾に移動
        assert!(m.delete("p0"));
        assert!(m.delete("p15"));
        assert!(m.delete("p63"));
        assert!(!m.delete("nonexistent"));
        assert_eq!(m.len(), n - 3);
        assert!(m.get("p0").is_none());
        assert!(m.get("p15").is_none());
        assert!(m.get("p63").is_none());

        m.set_value(Rc::from("p15"), Value::Number(1500.0));
        m.set_slot(Rc::from("p0"), PropSlot::Value(Value::Number(1000.0)));
        assert_eq!(m.len(), n - 1);

        let mut expected: Vec<String> = (0..n)
            .filter(|&i| i != 0 && i != 15 && i != 63)
            .map(|i| format!("p{i}"))
            .collect();
        expected.push("p15".to_string());
        expected.push("p0".to_string());
        assert_eq!(key_vec(&m), expected);

        // iter() も同じ順序と値を持つ
        let iter_keys: Vec<String> = m.iter().map(|(k, _)| k.to_string()).collect();
        assert_eq!(iter_keys, expected);
    }

    #[test]
    fn propmap_compacts_after_heavy_deletions_and_clone_is_independent() {
        let mut m = PropMap::new();
        for i in 0..200 {
            m.set_value(Rc::from(format!("k{i}")), Value::Number(i as f64));
        }
        assert!(m.index.is_some());

        // 150 個削除して圧縮を発火させる（残り 50 個、まだ indexed）
        for i in 0..150 {
            assert!(m.delete(&format!("k{i}")));
        }
        assert_eq!(m.len(), 50);
        assert!(m.index.is_some());
        assert!(
            m.entries.len() < 120,
            "圧縮されているべき: {}",
            m.entries.len()
        );

        let expected_50: Vec<String> = (150..200).map(|i| format!("k{i}")).collect();
        assert_eq!(key_vec(&m), expected_50);

        // Clone が独立しており元のマップと干渉しない
        let mut cloned = m.clone();
        cloned.set_value(Rc::from("k150"), Value::Number(-1.0));
        assert!(cloned.delete("k151"));
        assert_eq!(num_of(m.get("k150")), Some(150.0));
        assert_eq!(num_of(m.get("k151")), Some(151.0));
        assert_eq!(num_of(cloned.get("k150")), Some(-1.0));
        assert!(cloned.get("k151").is_none());

        // さらに削除してヒステリシス閾値（PROP_MAP_LINEAR_THRESHOLD / 2 = 8）以下まで減らすと index が解放される
        for i in 150..192 {
            assert!(m.delete(&format!("k{i}")));
        }
        assert_eq!(m.len(), 8);
        assert!(m.index.is_none());
        let expected_8: Vec<String> = (192..200).map(|i| format!("k{i}")).collect();
        assert_eq!(key_vec(&m), expected_8);

        // 全削除
        for i in 192..200 {
            assert!(m.delete(&format!("k{i}")));
        }
        assert!(m.is_empty());
        assert!(m.index.is_none());
    }

    #[test]
    fn propmap_define_accessor_updates_in_place_without_duplicating_key() {
        let mut m = PropMap::new();
        for i in 0..24 {
            m.set_value(Rc::from(format!("k{i}")), Value::Number(i as f64));
        }
        m.define_accessor(Rc::from("k5"), true, Value::Bool(true));
        m.define_accessor(Rc::from("k5"), false, Value::Bool(false));
        assert_eq!(m.len(), 24);
        assert_eq!(key_vec(&m)[5], "k5");
        match m.get("k5") {
            Some(PropSlot::Accessor {
                get: Some(Value::Bool(true)),
                set: Some(Value::Bool(false)),
            }) => {}
            _ => panic!("expected getter+setter accessor at k5"),
        }
    }

    #[test]
    fn propmap_2000_properties_correctness_and_order() {
        let mut m = PropMap::new();
        let count = 2000usize;
        for i in 0..count {
            m.set_value(Rc::from(format!("prop_{i}")), Value::Number(i as f64));
        }
        assert_eq!(m.len(), count);
        assert!(m.index.is_some());

        // 全件 O(1) 探索確認
        for i in 0..count {
            assert_eq!(num_of(m.get(&format!("prop_{i}"))), Some(i as f64));
        }

        // 偶数キーを更新し、3 の倍数のキーを削除して再挿入
        for i in (0..count).step_by(2) {
            m.set_value(
                Rc::from(format!("prop_{i}")),
                Value::Number((i * 10) as f64),
            );
        }
        for i in (0..count).step_by(3) {
            assert!(m.delete(&format!("prop_{i}")));
        }
        for i in (0..count).step_by(3) {
            m.set_value(
                Rc::from(format!("prop_{i}")),
                Value::Number((i + 100_000) as f64),
            );
        }

        assert_eq!(m.len(), count);
        let mut expected = Vec::with_capacity(count);
        for i in 0..count {
            if i % 3 != 0 {
                expected.push(format!("prop_{i}"));
            }
        }
        for i in (0..count).step_by(3) {
            expected.push(format!("prop_{i}"));
        }
        assert_eq!(key_vec(&m), expected);
        for i in 0..count {
            let want = if i % 3 == 0 {
                (i + 100_000) as f64
            } else if i % 2 == 0 {
                (i * 10) as f64
            } else {
                i as f64
            };
            assert_eq!(num_of(m.get(&format!("prop_{i}"))), Some(want));
        }
    }
}
