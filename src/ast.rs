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
    /// 文の総数（ネストしたブロック内も含めて再帰的に数える）。
    pub fn count_statements(&self) -> usize {
        fn walk_stmts(stmts: &[Stmt]) -> usize {
            let mut n = 0;
            for s in stmts {
                n += 1;
                n += walk_stmt_children(s);
            }
            n
        }
        fn walk_stmt_children(s: &Stmt) -> usize {
            match &s.kind {
                StmtKind::Block(b) => walk_stmts(b),
                StmtKind::If { cons, alt, .. } => {
                    let mut total = count_one(cons);
                    if let Some(a) = alt {
                        total += count_one(a);
                    }
                    total
                }
                StmtKind::While { body, .. } | StmtKind::DoWhile { body, .. } => count_one(body),
                StmtKind::For { body, .. } | StmtKind::ForIn { body, .. } => count_one(body),
                StmtKind::Switch { cases, .. } => {
                    cases.iter().map(|c| walk_stmts(&c.body)).sum()
                }
                StmtKind::Try {
                    block,
                    handler,
                    finalizer,
                } => {
                    let mut n = walk_stmts(block);
                    if let Some(h) = handler {
                        n += walk_stmts(&h.body);
                    }
                    if let Some(f) = finalizer {
                        n += walk_stmts(f);
                    }
                    n
                }
                StmtKind::Labeled { body, .. } => count_one(body),
                StmtKind::FunctionDecl(f) => f.body.as_ref().map(|b| walk_stmts(b)).unwrap_or(0),
                StmtKind::TsModule { body, .. } => walk_stmts(body),
                StmtKind::ExportDecl(inner) => count_one(inner),
                _ => 0,
            }
        }
        fn count_one(s: &Stmt) -> usize {
            1 + walk_stmt_children(s)
        }
        walk_stmts(&self.body)
    }

    /// 関数（宣言・式・アロー・メソッド）の総数。
    pub fn count_functions(&self) -> usize {
        let mut n = 0usize;
        fn visit_expr(e: &Expr, n: &mut usize) {
            match &e.kind {
                ExprKind::Function(_) | ExprKind::Arrow(_) => *n += 1,
                _ => {}
            }
            e.for_each_child(&mut |c| visit_expr(c, n));
        }
        fn visit_stmt(s: &Stmt, n: &mut usize) {
            if let StmtKind::FunctionDecl(_) = &s.kind {
                *n += 1;
            }
            s.for_each_child_stmt(&mut |c| visit_stmt(c, n));
            s.for_each_child_expr(&mut |e| visit_expr(e, n));
        }
        for s in &self.body {
            visit_stmt(s, &mut n);
        }
        n
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
