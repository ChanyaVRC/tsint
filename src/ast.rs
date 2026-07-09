//! AST（抽象構文木）定義。
//!
//! 設計方針（T2 の「明示スタックのバイトコード VM」実装を見据えて）:
//! - すべての文/式ノードは `NodeId`（モジュール内で単調増加する一意な ID）と
//!   `Span`（開始/終了の行・桁・バイトオフセット）を持つ。エラー報告や、
//!   将来的にバイトコードの行番号テーブルを AST ノードに逆引きする際に使う。
//! - TS 固有の構文（型注釈・interface・type エイリアス・ジェネリクス等）は
//!   「パースはするが構造化した情報は保持しない」。存在した痕跡（名前や span）
//!   だけを AST に残し、詳細なトークン列は捨てる。型チェックをしないインタプリタ
//!   なので、この方針で十分。
//! - Box を多用した木構造だが、T2 での「明示スタック」コンパイルは
//!   コンパイル時に AST を辿ってバイトコードを線形に吐き出す形になる想定。
//!   AST 自体の走査はふつうの再帰下降（コンパイラは Rust の呼び出しスタックを
//!   使ってよい）で構わない。「明示スタック」が要求されるのは *実行時の VM* の方。

/// モジュール内で一意な AST ノード ID。
pub type NodeId = u32;

/// ソース上の一点（1 始まりの行・桁、0 始まりのバイトオフセット）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pos {
    pub line: u32,
    pub col: u32,
    pub byte: u32,
}

/// ソース範囲。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Span {
    pub start: Pos,
    pub end: Pos,
}

impl Span {
    pub fn new(start: Pos, end: Pos) -> Self {
        Span { start, end }
    }
}

/// パース済みモジュール。
#[derive(Debug, Clone)]
pub struct Module {
    /// モジュール名（診断メッセージ用。多くはファイルパス）。
    pub name: String,
    pub body: Vec<Stmt>,
    /// 発行された最大の NodeId + 1（統計・検証用）。
    pub node_count: u32,
}

impl Module {
    /// 文の総数（ネストしたブロック・関数本体・クラスメソッド本体もすべて
    /// 再帰的に数える）。
    pub fn count_statements(&self) -> usize {
        let mut stmts = 0usize;
        let mut funcs = 0usize;
        count_stmts_rec(&self.body, &mut stmts, &mut funcs);
        stmts
    }

    /// 関数（宣言・式・アロー・メソッド・アクセサ・オブジェクトリテラルの
    /// メソッド省略記法）の総数。
    pub fn count_functions(&self) -> usize {
        let mut stmts = 0usize;
        let mut funcs = 0usize;
        count_stmts_rec(&self.body, &mut stmts, &mut funcs);
        funcs
    }
}

fn count_stmts_rec(list: &[Stmt], stmts: &mut usize, funcs: &mut usize) {
    for s in list {
        count_stmt_rec(s, stmts, funcs);
    }
}

fn count_stmt_rec(s: &Stmt, stmts: &mut usize, funcs: &mut usize) {
    *stmts += 1;
    match &s.kind {
        StmtKind::Block(b) => count_stmts_rec(b, stmts, funcs),
        StmtKind::VarDecl { decls, .. } => {
            for d in decls {
                if let Some(init) = &d.init {
                    count_expr_rec(init, stmts, funcs);
                }
            }
        }
        StmtKind::Expr(e) => count_expr_rec(e, stmts, funcs),
        StmtKind::FunctionDecl(f) => {
            *funcs += 1;
            if let Some(b) = &f.body {
                count_stmts_rec(b, stmts, funcs);
            }
        }
        StmtKind::ClassDecl(c) => count_class_rec(c, stmts, funcs),
        StmtKind::If { test, cons, alt } => {
            count_expr_rec(test, stmts, funcs);
            count_stmt_rec(cons, stmts, funcs);
            if let Some(a) = alt {
                count_stmt_rec(a, stmts, funcs);
            }
        }
        StmtKind::While { test, body } | StmtKind::DoWhile { body, test } => {
            count_expr_rec(test, stmts, funcs);
            count_stmt_rec(body, stmts, funcs);
        }
        StmtKind::For {
            init,
            test,
            update,
            body,
        } => {
            if let Some(init) = init {
                match init.as_ref() {
                    ForInit::VarDecl(_, decls) => {
                        for d in decls {
                            if let Some(e) = &d.init {
                                count_expr_rec(e, stmts, funcs);
                            }
                        }
                    }
                    ForInit::Expr(e) => count_expr_rec(e, stmts, funcs),
                }
            }
            if let Some(t) = test {
                count_expr_rec(t, stmts, funcs);
            }
            if let Some(u) = update {
                count_expr_rec(u, stmts, funcs);
            }
            count_stmt_rec(body, stmts, funcs);
        }
        StmtKind::ForIn { right, body, .. } => {
            count_expr_rec(right, stmts, funcs);
            count_stmt_rec(body, stmts, funcs);
        }
        StmtKind::Switch { disc, cases } => {
            count_expr_rec(disc, stmts, funcs);
            for c in cases {
                if let Some(t) = &c.test {
                    count_expr_rec(t, stmts, funcs);
                }
                count_stmts_rec(&c.body, stmts, funcs);
            }
        }
        StmtKind::Try {
            block,
            handler,
            finalizer,
        } => {
            count_stmts_rec(block, stmts, funcs);
            if let Some(h) = handler {
                count_stmts_rec(&h.body, stmts, funcs);
            }
            if let Some(f) = finalizer {
                count_stmts_rec(f, stmts, funcs);
            }
        }
        StmtKind::Throw(e) => count_expr_rec(e, stmts, funcs),
        StmtKind::Return(Some(e)) => count_expr_rec(e, stmts, funcs),
        StmtKind::Labeled { body, .. } => count_stmt_rec(body, stmts, funcs),
        StmtKind::ExportDefault(d) => match d.as_ref() {
            DefaultExportable::Expr(e) => count_expr_rec(e, stmts, funcs),
            DefaultExportable::Function(f) => {
                *funcs += 1;
                if let Some(b) = &f.body {
                    count_stmts_rec(b, stmts, funcs);
                }
            }
            DefaultExportable::Class(c) => count_class_rec(c, stmts, funcs),
        },
        StmtKind::ExportDecl(inner) => count_stmt_rec(inner, stmts, funcs),
        StmtKind::TsModule { body, .. } => count_stmts_rec(body, stmts, funcs),
        _ => {}
    }
}

fn count_class_rec(c: &Class, stmts: &mut usize, funcs: &mut usize) {
    for m in &c.members {
        if let Some(f) = &m.function {
            *funcs += 1;
            if let Some(b) = &f.body {
                count_stmts_rec(b, stmts, funcs);
            }
        }
        if let Some(v) = &m.value {
            count_expr_rec(v, stmts, funcs);
        }
    }
}

fn count_expr_rec(e: &Expr, stmts: &mut usize, funcs: &mut usize) {
    match &e.kind {
        ExprKind::Function(f) | ExprKind::Arrow(f) => {
            *funcs += 1;
            if let Some(b) = &f.body {
                count_stmts_rec(b, stmts, funcs);
            }
            if let Some(eb) = &f.expr_body {
                count_expr_rec(eb, stmts, funcs);
            }
            for p in &f.params {
                if let Some(d) = &p.default {
                    count_expr_rec(d, stmts, funcs);
                }
            }
        }
        ExprKind::Class(c) => count_class_rec(c, stmts, funcs),
        ExprKind::ObjectLit(props) => {
            for p in props {
                match p {
                    ObjectProp::KeyValue { value, .. } => count_expr_rec(value, stmts, funcs),
                    ObjectProp::Spread(e) => count_expr_rec(e, stmts, funcs),
                    ObjectProp::Method { function, .. } => {
                        *funcs += 1;
                        if let Some(b) = &function.body {
                            count_stmts_rec(b, stmts, funcs);
                        }
                    }
                }
            }
        }
        _ => {
            e.for_each_child(&mut |c| count_expr_rec(c, stmts, funcs));
        }
    }
}

#[derive(Debug, Clone)]
pub struct Stmt {
    pub id: NodeId,
    pub span: Span,
    pub kind: StmtKind,
}

#[derive(Debug, Clone)]
pub struct Expr {
    pub id: NodeId,
    pub span: Span,
    pub kind: ExprKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarKind {
    Var,
    Let,
    Const,
}

#[derive(Debug, Clone)]
pub struct VarDeclarator {
    pub name: Pattern,
    pub init: Option<Box<Expr>>,
}

#[derive(Debug, Clone)]
pub enum ForInit {
    VarDecl(VarKind, Vec<VarDeclarator>),
    Expr(Box<Expr>),
}

#[derive(Debug, Clone)]
pub struct SwitchCase {
    /// None は default 節。
    pub test: Option<Box<Expr>>,
    pub body: Vec<Stmt>,
}

#[derive(Debug, Clone)]
pub struct CatchClause {
    pub param: Option<Pattern>,
    pub body: Vec<Stmt>,
}

#[derive(Debug, Clone)]
pub enum ImportSpecifier {
    /// import Default from "..."
    Default(String),
    /// import * as ns from "..."
    Namespace(String),
    /// import { name [as local] } from "..."
    Named { imported: String, local: String },
}

#[derive(Debug, Clone)]
pub struct ImportDecl {
    pub specifiers: Vec<ImportSpecifier>,
    pub source: String,
    /// `import type { ... }` — 型のみ import（実行時には何も束縛しない）。
    pub type_only: bool,
}

#[derive(Debug, Clone)]
pub enum ExportSpecifier {
    Named { local: String, exported: String },
}

#[derive(Debug, Clone)]
pub enum DefaultExportable {
    Expr(Box<Expr>),
    Function(Box<Function>),
    Class(Box<Class>),
}

#[derive(Debug, Clone)]
pub enum StmtKind {
    Empty,
    Debugger,
    Expr(Box<Expr>),
    Block(Vec<Stmt>),
    VarDecl {
        kind: VarKind,
        decls: Vec<VarDeclarator>,
    },
    FunctionDecl(Box<Function>),
    ClassDecl(Box<Class>),
    If {
        test: Box<Expr>,
        cons: Box<Stmt>,
        alt: Option<Box<Stmt>>,
    },
    While {
        test: Box<Expr>,
        body: Box<Stmt>,
    },
    DoWhile {
        body: Box<Stmt>,
        test: Box<Expr>,
    },
    For {
        init: Option<Box<ForInit>>,
        test: Option<Box<Expr>>,
        update: Option<Box<Expr>>,
        body: Box<Stmt>,
    },
    ForIn {
        left: Box<ForInit>,
        right: Box<Expr>,
        body: Box<Stmt>,
        is_of: bool,
        is_await: bool,
    },
    Switch {
        disc: Box<Expr>,
        cases: Vec<SwitchCase>,
    },
    Try {
        block: Vec<Stmt>,
        handler: Option<CatchClause>,
        finalizer: Option<Vec<Stmt>>,
    },
    Throw(Box<Expr>),
    Return(Option<Box<Expr>>),
    Break(Option<String>),
    Continue(Option<String>),
    Labeled {
        label: String,
        body: Box<Stmt>,
    },
    Import(ImportDecl),
    ExportNamed {
        specifiers: Vec<ExportSpecifier>,
        /// `export { a } from "mod"` の再エクスポート元。
        source: Option<String>,
        type_only: bool,
    },
    ExportAll {
        /// `export * as ns from "mod"` の ns。
        as_name: Option<String>,
        source: String,
    },
    ExportDefault(Box<DefaultExportable>),
    /// `export const x = ...` / `export function f() {}` / `export class C {}`
    ExportDecl(Box<Stmt>),
    /// TS interface 宣言。中身は捨てて名前と span だけ残す。
    TsInterface { name: String },
    /// TS type エイリアス宣言。
    TsTypeAlias { name: String },
    /// `declare module "..." { ... }` / `declare namespace ... { ... }`
    TsModule { name: String, body: Vec<Stmt> },
    /// `declare` が付いた素のアンビエント宣言（module 以外）。中身は捨てる。
    TsAmbientDecl,
}

impl Stmt {
    /// 直接の子文（ネストしたブロックは含むが式の中の文は含まない）を訪問する。
    pub fn for_each_child_stmt<'a>(&'a self, f: &mut dyn FnMut(&'a Stmt)) {
        match &self.kind {
            StmtKind::Block(b) => b.iter().for_each(&mut *f),
            StmtKind::If { cons, alt, .. } => {
                f(cons);
                if let Some(a) = alt {
                    f(a);
                }
            }
            StmtKind::While { body, .. } | StmtKind::DoWhile { body, .. } => f(body),
            StmtKind::For { body, .. } | StmtKind::ForIn { body, .. } => f(body),
            StmtKind::Switch { cases, .. } => {
                for c in cases {
                    c.body.iter().for_each(&mut *f);
                }
            }
            StmtKind::Try {
                block,
                handler,
                finalizer,
            } => {
                block.iter().for_each(&mut *f);
                if let Some(h) = handler {
                    h.body.iter().for_each(&mut *f);
                }
                if let Some(fin) = finalizer {
                    fin.iter().for_each(&mut *f);
                }
            }
            StmtKind::Labeled { body, .. } => f(body),
            StmtKind::FunctionDecl(func) => {
                if let Some(b) = &func.body {
                    b.iter().for_each(&mut *f);
                }
            }
            StmtKind::ClassDecl(c) => {
                for m in &c.members {
                    if let Some(func) = &m.function {
                        if let Some(b) = &func.body {
                            b.iter().for_each(&mut *f);
                        }
                    }
                }
            }
            StmtKind::TsModule { body, .. } => body.iter().for_each(&mut *f),
            StmtKind::ExportDecl(inner) => f(inner),
            _ => {}
        }
    }

    /// 直接含む式を訪問する（子文の中の式は含まない）。
    pub fn for_each_child_expr<'a>(&'a self, f: &mut dyn FnMut(&'a Expr)) {
        match &self.kind {
            StmtKind::Expr(e) => f(e),
            StmtKind::VarDecl { decls, .. } => {
                for d in decls {
                    if let Some(i) = &d.init {
                        f(i);
                    }
                }
            }
            StmtKind::If { test, .. } => f(test),
            StmtKind::While { test, .. } => f(test),
            StmtKind::DoWhile { test, .. } => f(test),
            StmtKind::For { test, update, .. } => {
                if let Some(t) = test {
                    f(t);
                }
                if let Some(u) = update {
                    f(u);
                }
            }
            StmtKind::ForIn { right, .. } => f(right),
            StmtKind::Switch { disc, cases, .. } => {
                f(disc);
                for c in cases {
                    if let Some(t) = &c.test {
                        f(t);
                    }
                }
            }
            StmtKind::Throw(e) => f(e),
            StmtKind::Return(Some(e)) => f(e),
            _ => {}
        }
    }
}

/// 関数（宣言・式・アロー・メソッド共通）。
#[derive(Debug, Clone)]
pub struct Function {
    pub id: NodeId,
    pub span: Span,
    pub name: Option<String>,
    pub params: Vec<Param>,
    /// None は本体を持たないアンビエント宣言（.d.ts 由来のオーバーロード等）。
    pub body: Option<Vec<Stmt>>,
    /// アロー関数の式本体（`x => x + 1`）。body と排他。
    pub expr_body: Option<Box<Expr>>,
    pub is_async: bool,
    pub is_arrow: bool,
}

#[derive(Debug, Clone)]
pub struct Param {
    pub pattern: Pattern,
    pub default: Option<Box<Expr>>,
    pub rest: bool,
}

#[derive(Debug, Clone)]
pub enum Pattern {
    Ident(String),
    Object {
        props: Vec<ObjectPatternProp>,
        rest: Option<Box<Pattern>>,
    },
    Array {
        elements: Vec<Option<Pattern>>,
        rest: Option<Box<Pattern>>,
    },
    /// 分割代入内のデフォルト値付き要素（例: `{ a = 1 }`）。
    Assign {
        left: Box<Pattern>,
        right: Box<Expr>,
    },
}

#[derive(Debug, Clone)]
pub struct ObjectPatternProp {
    pub key: PropertyKey,
    pub value: Pattern,
    pub shorthand: bool,
}

#[derive(Debug, Clone)]
pub enum PropertyKey {
    Ident(String),
    Str(String),
    Num(f64),
    Computed(Box<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassMemberKind {
    Method,
    Getter,
    Setter,
    Field,
    Constructor,
}

#[derive(Debug, Clone)]
pub struct ClassMember {
    pub kind: ClassMemberKind,
    pub is_static: bool,
    pub name: PropertyKey,
    pub function: Option<Function>,
    pub value: Option<Box<Expr>>,
}

#[derive(Debug, Clone)]
pub struct Class {
    pub id: NodeId,
    pub span: Span,
    pub name: Option<String>,
    pub super_class: Option<Box<Expr>>,
    pub members: Vec<ClassMember>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Minus,
    Plus,
    Not,
    BitNot,
    Typeof,
    Void,
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateOp {
    Inc,
    Dec,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogicalOp {
    And,
    Or,
    Nullish,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignOp {
    Assign,
    AddAssign,
    SubAssign,
    MulAssign,
    DivAssign,
    ModAssign,
    PowAssign,
    ShlAssign,
    ShrAssign,
    UShrAssign,
    BitAndAssign,
    BitOrAssign,
    BitXorAssign,
    AndAssign,
    OrAssign,
    NullishAssign,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodKind {
    Normal,
    Get,
    Set,
}

#[derive(Debug, Clone)]
pub enum ObjectProp {
    KeyValue {
        key: PropertyKey,
        value: Box<Expr>,
        shorthand: bool,
    },
    Method {
        key: PropertyKey,
        function: Box<Function>,
        kind: MethodKind,
    },
    Spread(Box<Expr>),
}

#[derive(Debug, Clone)]
pub enum ExprKind {
    NumberLit(f64),
    StringLit(String),
    BoolLit(bool),
    NullLit,
    UndefinedLit,
    Ident(String),
    This,
    Super,
    TemplateLit {
        /// テンプレートの文字列断片。exprs より 1 つ多い。
        quasis: Vec<String>,
        exprs: Vec<Expr>,
    },
    RegexLit {
        pattern: String,
        flags: String,
    },
    ArrayLit(Vec<Option<Expr>>),
    ObjectLit(Vec<ObjectProp>),
    Function(Box<Function>),
    Arrow(Box<Function>),
    Class(Box<Class>),
    Unary {
        op: UnaryOp,
        arg: Box<Expr>,
    },
    Update {
        op: UpdateOp,
        arg: Box<Expr>,
        prefix: bool,
    },
    Binary {
        op: BinaryOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Logical {
        op: LogicalOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Assign {
        op: AssignOp,
        target: Box<Expr>,
        value: Box<Expr>,
    },
    Conditional {
        test: Box<Expr>,
        cons: Box<Expr>,
        alt: Box<Expr>,
    },
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
        optional: bool,
    },
    New {
        callee: Box<Expr>,
        args: Vec<Expr>,
    },
    Member {
        obj: Box<Expr>,
        prop: PropertyKey,
        optional: bool,
    },
    Sequence(Vec<Expr>),
    Spread(Box<Expr>),
    Await(Box<Expr>),
    /// `expr as Type` / `expr as const` — 型は捨てる。
    TsAs(Box<Expr>),
    /// `expr!` 非 null アサーション。
    TsNonNull(Box<Expr>),
}

impl Expr {
    /// 直接の子式を訪問する（浅い走査。統計用）。
    pub fn for_each_child<'a>(&'a self, f: &mut dyn FnMut(&'a Expr)) {
        match &self.kind {
            ExprKind::TemplateLit { exprs, .. } => exprs.iter().for_each(&mut *f),
            ExprKind::ArrayLit(els) => els.iter().flatten().for_each(&mut *f),
            ExprKind::ObjectLit(props) => {
                for p in props {
                    match p {
                        ObjectProp::KeyValue { value, .. } => f(value),
                        ObjectProp::Spread(e) => f(e),
                        ObjectProp::Method { .. } => {}
                    }
                }
            }
            ExprKind::Unary { arg, .. } => f(arg),
            ExprKind::Update { arg, .. } => f(arg),
            ExprKind::Binary { left, right, .. } => {
                f(left);
                f(right);
            }
            ExprKind::Logical { left, right, .. } => {
                f(left);
                f(right);
            }
            ExprKind::Assign { target, value, .. } => {
                f(target);
                f(value);
            }
            ExprKind::Conditional { test, cons, alt } => {
                f(test);
                f(cons);
                f(alt);
            }
            ExprKind::Call { callee, args, .. } => {
                f(callee);
                args.iter().for_each(&mut *f);
            }
            ExprKind::New { callee, args } => {
                f(callee);
                args.iter().for_each(&mut *f);
            }
            ExprKind::Member { obj, prop, .. } => {
                f(obj);
                if let PropertyKey::Computed(e) = prop {
                    f(e);
                }
            }
            ExprKind::Sequence(es) => es.iter().for_each(&mut *f),
            ExprKind::Spread(e) => f(e),
            ExprKind::Await(e) => f(e),
            ExprKind::TsAs(e) => f(e),
            ExprKind::TsNonNull(e) => f(e),
            _ => {}
        }
    }
}
