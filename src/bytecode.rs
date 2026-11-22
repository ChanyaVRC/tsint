//! バイトコード命令セットと関数単位の `Chunk`。
//!
//! 設計方針（T2 の核心）:
//! - 命令は「1 関数分の線形な列」（`Chunk`）にコンパイルされる。ネストした
//!   関数は別チャンクとして `chunk.functions` に保持し、`MakeClosure` 命令が
//!   実行時にクロージャオブジェクトを作る。
//! - VM（`vm.rs`）はこの命令列を Rust の再帰呼び出しを使わずに実行する。
//!   関数呼び出しは Rust スタックではなく `Vm.frames: Vec<Frame>`
//!   （ヒープ上の明示スタック）に積む。これにより将来（T3）
//!   「任意の命令境界で実行を中断し、後で再開する」ことが可能になる —
//!   中断は単に `Vm::run` のループを抜けて `Vm` 構造体（frames を含む）を
//!   保持しておけばよく、Rust のコールスタックには何も残らない。
//! - 例外（try/catch/finally）は「例外テーブル」方式。`finally` は
//!   コンパイラが各脱出経路（正常フォールスルー・catch 完了後・
//!   break/continue/return・例外での巻き戻し）ごとにバイトコードを
//!   複製して埋め込む（インライン展開方式）。これにより VM 側は
//!   「例外時に例外テーブルを引いて catch へジャンプする」以外の
//!   特別な制御構造を持たずに済む。

use std::rc::Rc;

use crate::value::Value;

pub type ConstIdx = u32;
pub type LocalIdx = u16;
pub type UpvalueIdx = u16;
pub type JumpAddr = u32;

#[derive(Debug, Clone, Copy)]
pub enum OpCode {
    // --- スタック操作 ---
    Pop,
    Dup,
    /// スタック上位 2 要素を入れ替える。
    Swap,

    // --- 定数・リテラル ---
    LoadConst(ConstIdx),
    LoadUndefined,
    LoadNull,
    LoadTrue,
    LoadFalse,

    // --- 変数 ---
    /// 読み出し。Uninit（TDZ）なら実行時エラー。
    GetLocal(LocalIdx),
    /// 代入式用。pop して書き込み、値を push し直す（`x = (y = 1)` の連鎖のため）。
    SetLocal(LocalIdx),
    /// 宣言（let/const/var/パラメータ/分割代入の束縛）用。pop して書き込み、push し直さない。
    BindLocal(LocalIdx),
    /// `for (let i ...)` の per-iteration 束縛用: このスロットの現在値を
    /// コピーした「新しいセル」に差し替える（既存のセルを指すクロージャは
    /// 古い値を保持したまま、以後の書き込みからは切り離される）。
    RebindLocal(LocalIdx),
    GetUpvalue(UpvalueIdx),
    SetUpvalue(UpvalueIdx),
    /// グローバル（モジュールトップレベル・ビルトイン）変数の参照/代入。
    /// 名前は定数表の文字列。未宣言の名前への Get/Set は実行時エラー。
    GetGlobal(ConstIdx),
    SetGlobal(ConstIdx),
    /// グローバルの宣言（初回登録。pop するだけで push し直さない）。
    BindGlobal(ConstIdx),

    // --- 引数（関数プロローグ専用） ---
    /// 呼び出し時の生引数配列から idx 番目を push（範囲外なら undefined）。
    GetArg(u16),
    /// 生引数配列の from_idx 以降をまとめた配列を push（rest パラメータ用）。
    GetRestArgs(u16),

    // --- プロパティ / 添字 ---
    /// 定数表の名前で obj.prop を取得。obj は pop。
    GetProp(ConstIdx),
    /// obj.prop = value。 stack: [obj, value] -> [value]
    SetProp(ConstIdx),
    /// stack: [obj, key] -> [value]
    GetIndex,
    /// stack: [obj, key, value] -> [value]
    SetIndex,
    DeleteProp(ConstIdx),
    DeleteIndex,
    /// オプショナルチェーン用: スタック最上位を覗いて null/undefined なら
    /// pop して undefined を push し addr へジャンプ。そうでなければ何もしない。
    JumpIfNullish(JumpAddr),
    /// 呼び出し直前の [this, callee] のうち callee(最上位) が null/undefined
    /// なら両方 pop して undefined を push し addr へジャンプ。
    JumpIfNullishCall(JumpAddr),

    // --- 算術 / 比較 / 論理 ---
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    Eq,
    NotEq,
    StrictEq,
    StrictNotEq,
    Lt,
    Gt,
    LtEq,
    GtEq,
    Shl,
    Shr,
    UShr,
    BitAnd,
    BitOr,
    BitXor,
    Instanceof,
    In,
    Neg,
    Pos,
    Not,
    BitNot,
    TypeOf,
    /// `typeof possiblyUndeclaredGlobal`: 未定義でも ReferenceError にせず
    /// "undefined" を返す（JS 仕様通り）。
    TypeOfGlobal(ConstIdx),
    /// ToString（テンプレートリテラルの連結に使用）。
    ToStringOp,
    /// void 演算子（オペランドを評価して捨て undefined を push）は
    /// コンパイラ側で Pop+LoadUndefined に展開するため専用命令は無い。
    // --- 論理短絡（値を保持したままジャンプ or pop して継続） ---
    JumpIfFalsyKeep(JumpAddr),
    JumpIfTruthyKeep(JumpAddr),
    JumpIfNotNullishKeep(JumpAddr),
    /// デフォルト引数/分割代入デフォルト値用: 値が undefined でなければ
    /// 保持してジャンプ、undefined なら pop して継続（デフォルト式を評価する）。
    JumpIfNotUndefinedKeep(JumpAddr),

    // --- 制御フロー ---
    Jump(JumpAddr),
    JumpIfFalse(JumpAddr),
    JumpIfTrue(JumpAddr),
    Return,
    /// 例外送出。スタック最上位を投げる（finally からの再送出も
    /// `GetLocal`(保持しておいた例外値) + `Throw` として表現され、
    /// 専用命令は不要）。
    Throw,

    // --- 関数 / クラス / オブジェクト生成 ---
    /// functions[idx] を現在フレームの upvalue 記述に従ってクロージャ化。
    MakeClosure(ConstIdx),
    /// stack: [callee(this の下), callee, arg0..argN-1] -> [result]
    Call(u16),
    /// 引数を単一の配列値（スプレッド展開込み）として渡す呼び出し。
    /// stack: [this, callee, argsArray] -> [result]
    CallSpread,
    New(u16),
    NewSpread,
    NewObject,
    NewArray,
    /// stack: [array, value] -> [array]（value を push、array はそのまま残す）
    ArrayPush,
    /// stack: [array, holeCount] は使わず、代わりに elision は LoadUndefined で表現する。
    /// stack: [array, srcIterable] -> [array]（スプレッド展開して array に追加）
    SpreadAppendArray,
    /// stack: [obj, value] -> [obj]（定数名プロパティを設定）
    ObjectSetProp(ConstIdx),
    /// stack: [obj, key, value] -> [obj]
    ObjectSetComputed,
    /// stack: [obj, funcValue] -> [obj]（メソッド定義。ConstIdx は名前）
    ObjectSetMethod(ConstIdx),
    /// stack: [obj, funcValue] -> [obj]（getter/setter 定義）
    ObjectDefineGetter(ConstIdx),
    ObjectDefineSetter(ConstIdx),
    /// stack: [obj, src] -> [obj]（オブジェクトスプレッド）
    ObjectSpread,
    /// 値を反復可能な配列へ実体化する（Array はそのまま、String は文字配列、
    /// Map は [k,v] 配列、Set は値配列）。
    MakeIterArray,
    /// for-in 用: オブジェクトの列挙可能な own キー一覧（配列は添字の文字列表現）を配列にして push。
    ObjectKeysArray,
    /// オブジェクト分割代入の rest 用: stack [obj, key1..keyN] -> [restObj]
    /// （obj の own プロパティのうち key1..keyN を除いたコピーを作る）。
    ObjectRestExclude(u16),
    /// 配列分割代入の rest 用: stack [arr, startIndex] -> [restArr]
    ArraySliceFrom,

    // --- this / 式文脈 ---
    LoadThis,

    /// クラス定義。定数表の ClassTemplate から実行時にクラス値を組み立てる。
    MakeClass(ConstIdx),

    /// 正規表現リテラル（定数表の Const::Regex から都度オブジェクト化）。
    LoadRegex(ConstIdx),
    /// await（T2 では同期的パススルー。Promise らしき値ならエラー。
    /// T3 でここに中断ポイントを追加する）。
    Await,
}

/// 定数表に置けるコンパイル時定数。
pub enum Const {
    Number(f64),
    Str(Rc<str>),
    /// ネストした関数のバイトコード + アップバリュー捕捉記述。
    FunctionTemplate(Rc<FunctionTemplate>),
    ClassTemplate(Rc<ClassTemplate>),
    Regex(Rc<crate::regex::Regex>),
}

impl Clone for Const {
    fn clone(&self) -> Self {
        match self {
            Const::Number(n) => Const::Number(*n),
            Const::Str(s) => Const::Str(s.clone()),
            Const::FunctionTemplate(f) => Const::FunctionTemplate(f.clone()),
            Const::ClassTemplate(c) => Const::ClassTemplate(c.clone()),
            Const::Regex(r) => Const::Regex(r.clone()),
        }
    }
}

/// クロージャがどこから値を捕捉するかの記述。
#[derive(Debug, Clone, Copy)]
pub enum UpvalueDesc {
    /// 直接の親フレームのローカルスロットから捕捉。
    ParentLocal(LocalIdx),
    /// 直接の親フレーム自身のアップバリューから捕捉（さらに外側）。
    ParentUpvalue(UpvalueIdx),
}

pub struct FunctionTemplate {
    pub name: Rc<str>,
    pub chunk: Rc<Chunk>,
    pub upvalues: Vec<UpvalueDesc>,
    pub is_arrow: bool,
    pub is_async: bool,
    pub param_count: usize,
    pub has_rest: bool,
}

pub struct ClassMethodTemplate {
    pub name: Rc<str>,
    pub is_static: bool,
    pub func: Rc<FunctionTemplate>,
}

pub struct ClassAccessorTemplate {
    pub name: Rc<str>,
    pub is_static: bool,
    pub is_get: bool,
    pub func: Rc<FunctionTemplate>,
}

pub struct ClassTemplate {
    pub name: Rc<str>,
    pub ctor: Option<Rc<FunctionTemplate>>,
    pub methods: Vec<ClassMethodTemplate>,
    pub accessors: Vec<ClassAccessorTemplate>,
    /// static フィールド初期化子（クラス定義時に一度だけ評価）。
    /// 初期化子は「引数なしで `this`（＝クラス自身 or インスタンス）を
    /// 受け取り値を返す小さな関数」として表現する（クロージャ捕捉の仕組みを
    /// メソッドとまったく同じ経路で再利用するため）。
    pub static_fields: Vec<(Rc<str>, Option<Rc<FunctionTemplate>>)>,
    /// インスタンスフィールド初期化子（`new` のたびに `this` = 新インスタンスで呼ぶ）。
    pub instance_fields: Vec<(Rc<str>, Option<Rc<FunctionTemplate>>)>,
}

/// 例外テーブルの 1 エントリ。`try_start..try_end`（命令アドレス範囲）で
/// 例外が発生したら、フレームのオペランドスタックを空に巻き戻したうえで
/// 例外値を push し、`handler_addr` へジャンプする。
///
/// スタックを常に「空」へ巻き戻すだけで正しい理由: このコンパイラは文
/// （statement）の境界では常にオペランドスタックの深さを 0
/// （フレーム開始時点と同じ）に保つように式をコンパイルする
/// （すべての式文は評価後に必ず Pop する等）。`try` は文としてしか
/// 現れない（式の中には現れない）ので、try 節の開始時点のスタック深さは
/// 必ず 0 であり、例外発生時に残っている値は評価途中の一時値のみ
/// （例: `1 + f()` で `f()` が投げた場合の `1`）なので安全に捨ててよい。
#[derive(Debug, Clone, Copy)]
pub struct ExceptionEntry {
    pub try_start: u32,
    pub try_end: u32,
    pub handler_addr: u32,
}

pub struct Chunk {
    pub name: Rc<str>,
    pub code: Vec<OpCode>,
    /// `code[i]` に対応するソース行番号（デバッグ/エラー表示用）。
    pub lines: Vec<u32>,
    pub consts: Vec<Const>,
    pub exception_table: Vec<ExceptionEntry>,
    /// このフレームで確保するローカルスロット数（`this` やパラメータ含む）。
    pub num_locals: usize,
    /// 各ローカルスロットが内側クロージャにキャプチャされるかどうか（長さ == `num_locals`）。
    /// `true` のスロットのみ実行時に `Rc<RefCell<Value>>` でボックス化され、
    /// `false` のスロットは非ボックス（`LocalSlot::Direct`）のまま保持される。
    pub captured_locals: Vec<bool>,
    /// モジュールのトップレベルチャンクにのみ意味を持つ:
    /// export 名 -> ローカルスロット の対応表。
    pub exports: Vec<(Rc<str>, LocalIdx)>,
    /// これがモジュールのトップレベルチャンクなら true（T3: `Return` 実行時に
    /// 「このタスクはモジュール本体である」を `frames.len() == 1` という
    /// 深さだけに頼らず判定するためのマーカー。async 関数呼び出しも
    /// 専用のフレーム列を新規に持つため、深さだけでは区別できない）。
    pub is_module: bool,
}

impl Chunk {
    pub fn new(name: impl Into<Rc<str>>) -> Self {
        Chunk {
            name: name.into(),
            code: Vec::new(),
            lines: Vec::new(),
            consts: Vec::new(),
            exception_table: Vec::new(),
            num_locals: 0,
            captured_locals: Vec::new(),
            exports: Vec::new(),
            is_module: false,
        }
    }
}

/// 定数表からの読み出しヘルパー。
impl Chunk {
    pub fn const_str(&self, idx: ConstIdx) -> Rc<str> {
        match &self.consts[idx as usize] {
            Const::Str(s) => s.clone(),
            _ => panic!("定数 {idx} は文字列ではありません"),
        }
    }

    pub fn const_value(&self, idx: ConstIdx) -> Value {
        match &self.consts[idx as usize] {
            Const::Number(n) => Value::Number(*n),
            Const::Str(s) => Value::Str(s.clone()),
            _ => panic!("定数 {idx} は値化できません"),
        }
    }
}
