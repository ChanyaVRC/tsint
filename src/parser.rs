//! 再帰下降 + Pratt パーサ。
//!
//! TS 固有構文（型注釈・interface・generics 等）は「読み飛ばす」ことに
//! 特化したヘルパー（`skip_type` 系）で処理し、AST には痕跡（名前・span）
//! だけを残す。実行時に必要な情報（値・制御構造）はふつうに AST 化する。

use crate::ast::*;
use crate::lexer::{LexError, Lexer, Punct, Token, TokenKind};

#[derive(Debug, Clone)]
pub struct ParseError {
    pub message: String,
    pub span: Span,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "構文エラー {}:{}: {}",
            self.span.start.line, self.span.start.col, self.message
        )
    }
}

impl std::error::Error for ParseError {}

impl From<LexError> for ParseError {
    fn from(e: LexError) -> Self {
        ParseError {
            message: e.message,
            span: Span::new(e.pos, e.pos),
        }
    }
}

pub(crate) type PResult<T> = Result<T, ParseError>;

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    next_id: NodeId,
    /// `>>` / `>>>` トークンを 1 個の `>` としてだけ消費した際に、残りの
    /// 「閉じ borrowed」を後続の型引数スキップ処理に持ち越すためのカウンタ。
    /// ネストしたジェネリクス (`Foo<Bar<Baz>>`) をレキサ側でトークン分割
    /// せずに扱うための簡易な帳尻合わせ。
    pending_angle_closes: i32,
}

impl Parser {
    pub fn new(source: &str) -> Result<Self, ParseError> {
        let tokens = Lexer::tokenize(source)?;
        Ok(Parser {
            tokens,
            pos: 0,
            next_id: 0,
            pending_angle_closes: 0,
        })
    }

    fn fresh_id(&mut self) -> NodeId {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    // ---------- トークン操作 ----------

    #[inline]
    fn cur(&self) -> &Token {
        &self.tokens[self.pos.min(self.tokens.len() - 1)]
    }

    #[inline]
    fn cur_span(&self) -> Span {
        self.cur().span
    }

    #[inline]
    fn is_eof(&self) -> bool {
        matches!(self.cur().kind, TokenKind::Eof)
    }

    /// 現在トークンを 1 つ進める（トークンのクローンを一切行わない）。
    #[inline]
    fn bump(&mut self) {
        if self.pos < self.tokens.len() - 1 {
            self.pos += 1;
        }
    }

    #[inline]
    fn save(&self) -> usize {
        self.pos
    }

    #[inline]
    fn restore(&mut self, mark: usize) {
        self.pos = mark;
    }

    fn err(&self, message: impl Into<String>) -> ParseError {
        ParseError {
            message: message.into(),
            span: self.cur_span(),
        }
    }

    #[inline]
    fn peek_kind(&self, offset: usize) -> &TokenKind {
        let idx = (self.pos + offset).min(self.tokens.len() - 1);
        &self.tokens[idx].kind
    }

    #[inline]
    fn peek_is_punct(&self, offset: usize, p: Punct) -> bool {
        matches!(self.peek_kind(offset), TokenKind::Punct(q) if *q == p)
    }

    #[inline]
    fn is_punct(&self, p: Punct) -> bool {
        matches!(&self.cur().kind, TokenKind::Punct(q) if *q == p)
    }

    #[inline]
    fn eat_punct(&mut self, p: Punct) -> bool {
        if self.is_punct(p) {
            self.bump();
            true
        } else {
            false
        }
    }

    #[inline]
    fn expect_punct(&mut self, p: Punct) -> PResult<Span> {
        if self.is_punct(p) {
            let span = self.cur_span();
            self.bump();
            Ok(span)
        } else {
            Err(self.err(format!(
                "{:?} を期待しましたが {:?} が見つかりました",
                p,
                self.cur().kind
            )))
        }
    }

    /// 現在位置が識別子（キーワードを含む「単語」トークン）で、かつ指定した
    /// 文字列と一致するかどうか。TS の文脈依存キーワード（interface, type,
    /// declare, as, of, get, set, async, from ...）はすべて Ident として
    /// レックスされているため、この関数で判定する。
    #[inline]
    fn is_word(&self, word: &str) -> bool {
        matches!(&self.cur().kind, TokenKind::Ident(s) if s == word)
    }

    #[inline]
    fn eat_word(&mut self, word: &str) -> bool {
        if self.is_word(word) {
            self.bump();
            true
        } else {
            false
        }
    }

    #[inline]
    fn expect_word(&mut self, word: &str) -> PResult<()> {
        if self.eat_word(word) {
            Ok(())
        } else {
            Err(self.err(format!("キーワード '{word}' を期待しました")))
        }
    }

    #[inline]
    fn cur_ident(&self) -> Option<&str> {
        match &self.cur().kind {
            TokenKind::Ident(s) => Some(s.as_str()),
            _ => None,
        }
    }

    fn ident_name(&self) -> Option<String> {
        match &self.cur().kind {
            TokenKind::Ident(s) => Some(s.clone()),
            _ => None,
        }
    }

    /// 識別子（束縛名として使うためのもの。ほとんどの TS 文脈キーワードは
    /// 予約語ではないのでこれで受理してよい）を読む。
    fn parse_binding_ident(&mut self) -> PResult<String> {
        match self.ident_name() {
            Some(s) => {
                self.bump();
                Ok(s)
            }
            None => Err(self.err("識別子を期待しました")),
        }
    }

    /// ASI: 文の区切りとして `;` を読む。無ければ、直前に改行があるか
    /// `}` / EOF の直前であれば省略を許す（主要な ASI 規則）。
    fn consume_semi(&mut self) -> PResult<()> {
        if self.eat_punct(Punct::Semi) {
            return Ok(());
        }
        if self.is_punct(Punct::RBrace) || self.is_eof() || self.cur().newline_before {
            return Ok(());
        }
        Err(self.err("';' が必要です"))
    }

    /// 直前に消費したトークンの終端位置（span 計算用）。
    fn prev_end(&self) -> Pos {
        if self.pos == 0 {
            return self.tokens[0].span.start;
        }
        self.tokens[self.pos - 1].span.end
    }

    fn mk_expr(&mut self, start: Span, kind: ExprKind) -> Expr {
        let id = self.fresh_id();
        let span = Span::new(start.start, self.prev_end());
        Expr { id, span, kind }
    }

    fn mk_stmt(&mut self, start: Span, kind: StmtKind) -> Stmt {
        let id = self.fresh_id();
        let span = Span::new(start.start, self.prev_end());
        Stmt { id, span, kind }
    }

    // ---------- エントリポイント ----------

    pub fn parse_module(mut self, name: &str) -> Result<Module, ParseError> {
        let mut body = Vec::new();
        while !self.is_eof() {
            body.push(self.parse_stmt()?);
        }
        Ok(Module {
            name: name.to_string(),
            body,
            node_count: self.next_id,
        })
    }
}

mod expr;
mod pattern;
mod stmt;
mod types;
