//! ランタイム値の表現。
//!
//! 設計方針:
//! - プリミティブは `Value` に直接載せる（undefined/null/bool/f64/文字列）。
//!   文字列は不変（`Rc<str>`）。
//! - オブジェクト系（プレーンオブジェクト・配列・関数・クラス・Map/Set・
//!   エラー）はすべて `Rc<RefCell<ObjData>>` を共有する `Value::Object` に
//!   統一する。`ObjKind` で実体の種類を分岐する。
//! - ローカル変数スロットはすべて `Rc<RefCell<Value>>`（"boxed local"）で
//!   持つ（`vm.rs` 参照）。クロージャのアップバリュー捕捉はこのセルを
//!   共有するだけで済み、CLox 的な open/closed upvalue の作り分けが不要になる
//!   （多少のアロケーションコストと引き換えに実装を大幅に単純化する判断）。
//! - `Value::Uninit` は TDZ 判定専用の内部マーカー。JS コードから直接
//!   観測されることはない。

use std::cell::RefCell;
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

/// 挿入順を保持する単純なプロパティマップ（線形探索。コーパス規模なら十分）。
#[derive(Clone, Default)]
pub struct PropMap {
    entries: Vec<(Rc<str>, PropSlot)>,
}

impl PropMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, key: &str) -> Option<&PropSlot> {
        self.entries
            .iter()
            .find(|(k, _)| &**k == key)
            .map(|(_, v)| v)
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut PropSlot> {
        self.entries
            .iter_mut()
            .find(|(k, _)| &**k == key)
            .map(|(_, v)| v)
    }

    pub fn set_value(&mut self, key: Rc<str>, value: Value) {
        if let Some(slot) = self.get_mut(&key) {
            *slot = PropSlot::Value(value);
        } else {
            self.entries.push((key, PropSlot::Value(value)));
        }
    }

    pub fn set_slot(&mut self, key: Rc<str>, slot: PropSlot) {
        if let Some(existing) = self.entries.iter_mut().find(|(k, _)| *k == key) {
            existing.1 = slot;
        } else {
            self.entries.push((key, slot));
        }
    }

    pub fn define_accessor(&mut self, key: Rc<str>, is_get: bool, f: Value) {
        if let Some(PropSlot::Accessor { get, set }) = self.get_mut(&key) {
            if is_get {
                *get = Some(f);
            } else {
                *set = Some(f);
            }
            return;
        }
        let (get, set) = if is_get {
            (Some(f), None)
        } else {
            (None, Some(f))
        };
        self.entries.push((key, PropSlot::Accessor { get, set }));
    }

    pub fn delete(&mut self, key: &str) -> bool {
        if let Some(idx) = self.entries.iter().position(|(k, _)| &**k == key) {
            self.entries.remove(idx);
            true
        } else {
            false
        }
    }

    pub fn keys(&self) -> impl Iterator<Item = &Rc<str>> {
        self.entries.iter().map(|(k, _)| k)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Rc<str>, &PropSlot)> {
        self.entries.iter().map(|(k, v)| (k, v))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
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
