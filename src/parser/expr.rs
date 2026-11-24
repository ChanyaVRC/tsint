//! 式のパース（Pratt / 優先順位再帰下降）。
//!
//! 優先順位（低い順、`parse_expr` が最も低い）:
//! sequence(,) > assign > conditional(?:) > nullish/or > and > bitor > bitxor
//! > bitand > equality > relational > shift > additive > multiplicative
//! > exponent(**) > unary > postfix(++/-- , as, !) > call/member > primary

use super::{PResult, Parser};
use crate::ast::*;
use crate::lexer::{Punct, TokenKind};

/// アロー関数本体: ブロック文本体か、単一の式本体か。
type ArrowBody = (Option<Vec<Stmt>>, Option<Box<Expr>>);

impl Parser {
    /// カンマ演算子を含む最上位の式。
    pub(super) fn parse_expr(&mut self) -> PResult<Expr> {
        let start = self.cur_span();
        let first = self.parse_assign_expr()?;
        if self.is_punct(Punct::Comma) {
            let mut seq = vec![first];
            while self.eat_punct(Punct::Comma) {
                seq.push(self.parse_assign_expr()?);
            }
            return Ok(self.mk_expr(start, ExprKind::Sequence(seq)));
        }
        Ok(first)
    }

    /// 代入式（アロー関数の検出もここから始まる）。
    pub(super) fn parse_assign_expr(&mut self) -> PResult<Expr> {
        if let Some(arrow) = self.try_parse_arrow()? {
            return Ok(arrow);
        }
        let start = self.cur_span();
        let left = self.parse_conditional()?;
        let op = match &self.cur().kind {
            TokenKind::Punct(p) => match p {
                Punct::Eq => Some(AssignOp::Assign),
                Punct::PlusEq => Some(AssignOp::AddAssign),
                Punct::MinusEq => Some(AssignOp::SubAssign),
                Punct::StarEq => Some(AssignOp::MulAssign),
                Punct::SlashEq => Some(AssignOp::DivAssign),
                Punct::PercentEq => Some(AssignOp::ModAssign),
                Punct::StarStarEq => Some(AssignOp::PowAssign),
                Punct::LtLtEq => Some(AssignOp::ShlAssign),
                Punct::GtGtEq => Some(AssignOp::ShrAssign),
                Punct::GtGtGtEq => Some(AssignOp::UShrAssign),
                Punct::AmpEq => Some(AssignOp::BitAndAssign),
                Punct::PipeEq => Some(AssignOp::BitOrAssign),
                Punct::CaretEq => Some(AssignOp::BitXorAssign),
                Punct::AmpAmpEq => Some(AssignOp::AndAssign),
                Punct::PipePipeEq => Some(AssignOp::OrAssign),
                Punct::QuestionQuestionEq => Some(AssignOp::NullishAssign),
                _ => None,
            },
            _ => None,
        };
        if let Some(op) = op {
            self.bump();
            let value = self.parse_assign_expr()?;
            return Ok(self.mk_expr(
                start,
                ExprKind::Assign {
                    op,
                    target: Box::new(left),
                    value: Box::new(value),
                },
            ));
        }
        Ok(left)
    }

    fn parse_conditional(&mut self) -> PResult<Expr> {
        let start = self.cur_span();
        let test = self.parse_nullish_or()?;
        if self.eat_punct(Punct::Question) {
            let cons = self.parse_assign_expr()?;
            self.expect_punct(Punct::Colon)?;
            let alt = self.parse_assign_expr()?;
            return Ok(self.mk_expr(
                start,
                ExprKind::Conditional {
                    test: Box::new(test),
                    cons: Box::new(cons),
                    alt: Box::new(alt),
                },
            ));
        }
        Ok(test)
    }

    /// `||` と `??` は同じ優先順位段で扱う（本来は直接混在不可だが寛容にする）。
    fn parse_nullish_or(&mut self) -> PResult<Expr> {
        let start = self.cur_span();
        let mut left = self.parse_logical_and()?;
        loop {
            let op = if self.is_punct(Punct::PipePipe) {
                LogicalOp::Or
            } else if self.is_punct(Punct::QuestionQuestion) {
                LogicalOp::Nullish
            } else {
                break;
            };
            self.bump();
            let right = self.parse_logical_and()?;
            left = self.mk_expr(
                start,
                ExprKind::Logical {
                    op,
                    left: Box::new(left),
                    right: Box::new(right),
                },
            );
        }
        Ok(left)
    }

    fn parse_logical_and(&mut self) -> PResult<Expr> {
        let start = self.cur_span();
        let mut left = self.parse_bit_or()?;
        while self.eat_punct(Punct::AmpAmp) {
            let right = self.parse_bit_or()?;
            left = self.mk_expr(
                start,
                ExprKind::Logical {
                    op: LogicalOp::And,
                    left: Box::new(left),
                    right: Box::new(right),
                },
            );
        }
        Ok(left)
    }

    fn parse_bit_or(&mut self) -> PResult<Expr> {
        let start = self.cur_span();
        let mut left = self.parse_bit_xor()?;
        while self.is_punct(Punct::Pipe) {
            self.bump();
            let right = self.parse_bit_xor()?;
            left = self.mk_bin(start, BinaryOp::BitOr, left, right);
        }
        Ok(left)
    }

    fn parse_bit_xor(&mut self) -> PResult<Expr> {
        let start = self.cur_span();
        let mut left = self.parse_bit_and()?;
        while self.is_punct(Punct::Caret) {
            self.bump();
            let right = self.parse_bit_and()?;
            left = self.mk_bin(start, BinaryOp::BitXor, left, right);
        }
        Ok(left)
    }

    fn parse_bit_and(&mut self) -> PResult<Expr> {
        let start = self.cur_span();
        let mut left = self.parse_equality()?;
        while self.is_punct(Punct::Amp) {
            self.bump();
            let right = self.parse_equality()?;
            left = self.mk_bin(start, BinaryOp::BitAnd, left, right);
        }
        Ok(left)
    }

    fn parse_equality(&mut self) -> PResult<Expr> {
        let start = self.cur_span();
        let mut left = self.parse_relational()?;
        loop {
            let op = match &self.cur().kind {
                TokenKind::Punct(Punct::EqEq) => BinaryOp::Eq,
                TokenKind::Punct(Punct::NotEq) => BinaryOp::NotEq,
                TokenKind::Punct(Punct::EqEqEq) => BinaryOp::StrictEq,
                TokenKind::Punct(Punct::NotEqEq) => BinaryOp::StrictNotEq,
                _ => break,
            };
            self.bump();
            let right = self.parse_relational()?;
            left = self.mk_bin(start, op, left, right);
        }
        Ok(left)
    }

    fn parse_relational(&mut self) -> PResult<Expr> {
        let start = self.cur_span();
        let mut left = self.parse_shift()?;
        loop {
            let op = match &self.cur().kind {
                TokenKind::Punct(Punct::Lt) => BinaryOp::Lt,
                TokenKind::Punct(Punct::Gt) => BinaryOp::Gt,
                TokenKind::Punct(Punct::LtEq) => BinaryOp::LtEq,
                TokenKind::Punct(Punct::GtEq) => BinaryOp::GtEq,
                TokenKind::Ident(w) if w == "instanceof" => BinaryOp::Instanceof,
                TokenKind::Ident(w) if w == "in" => BinaryOp::In,
                _ => break,
            };
            self.bump();
            let right = self.parse_shift()?;
            left = self.mk_bin(start, op, left, right);
        }
        Ok(left)
    }

    fn parse_shift(&mut self) -> PResult<Expr> {
        let start = self.cur_span();
        let mut left = self.parse_additive()?;
        loop {
            let op = match &self.cur().kind {
                TokenKind::Punct(Punct::LtLt) => BinaryOp::Shl,
                TokenKind::Punct(Punct::GtGt) => BinaryOp::Shr,
                TokenKind::Punct(Punct::GtGtGt) => BinaryOp::UShr,
                _ => break,
            };
            self.bump();
            let right = self.parse_additive()?;
            left = self.mk_bin(start, op, left, right);
        }
        Ok(left)
    }

    fn parse_additive(&mut self) -> PResult<Expr> {
        let start = self.cur_span();
        let mut left = self.parse_multiplicative()?;
        loop {
            let op = match &self.cur().kind {
                TokenKind::Punct(Punct::Plus) => BinaryOp::Add,
                TokenKind::Punct(Punct::Minus) => BinaryOp::Sub,
                _ => break,
            };
            self.bump();
            let right = self.parse_multiplicative()?;
            left = self.mk_bin(start, op, left, right);
        }
        Ok(left)
    }

    fn parse_multiplicative(&mut self) -> PResult<Expr> {
        let start = self.cur_span();
        let mut left = self.parse_exponent()?;
        loop {
            let op = match &self.cur().kind {
                TokenKind::Punct(Punct::Star) => BinaryOp::Mul,
                TokenKind::Punct(Punct::Slash) => BinaryOp::Div,
                TokenKind::Punct(Punct::Percent) => BinaryOp::Mod,
                _ => break,
            };
            self.bump();
            let right = self.parse_exponent()?;
            left = self.mk_bin(start, op, left, right);
        }
        Ok(left)
    }

    fn parse_exponent(&mut self) -> PResult<Expr> {
        let start = self.cur_span();
        let base = self.parse_unary()?;
        if self.eat_punct(Punct::StarStar) {
            let right = self.parse_exponent()?; // 右結合
            return Ok(self.mk_bin(start, BinaryOp::Pow, base, right));
        }
        Ok(base)
    }

    fn mk_bin(&mut self, start: crate::ast::Span, op: BinaryOp, left: Expr, right: Expr) -> Expr {
        self.mk_expr(
            start,
            ExprKind::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
            },
        )
    }

    fn parse_unary(&mut self) -> PResult<Expr> {
        let start = self.cur_span();
        if let TokenKind::Punct(p) = &self.cur().kind {
            let op = match p {
                Punct::Bang => Some(UnaryOp::Not),
                Punct::Tilde => Some(UnaryOp::BitNot),
                Punct::Plus => Some(UnaryOp::Plus),
                Punct::Minus => Some(UnaryOp::Minus),
                _ => None,
            };
            if let Some(op) = op {
                self.bump();
                let arg = self.parse_unary()?;
                return Ok(self.mk_expr(
                    start,
                    ExprKind::Unary {
                        op,
                        arg: Box::new(arg),
                    },
                ));
            }
            if matches!(p, Punct::PlusPlus | Punct::MinusMinus) {
                let op = if *p == Punct::PlusPlus {
                    UpdateOp::Inc
                } else {
                    UpdateOp::Dec
                };
                self.bump();
                let arg = self.parse_unary()?;
                return Ok(self.mk_expr(
                    start,
                    ExprKind::Update {
                        op,
                        arg: Box::new(arg),
                        prefix: true,
                    },
                ));
            }
        }
        if let Some(word) = self.cur_ident() {
            let op = match word {
                "typeof" => Some(UnaryOp::Typeof),
                "void" => Some(UnaryOp::Void),
                "delete" => Some(UnaryOp::Delete),
                _ => None,
            };
            if let Some(op) = op {
                self.bump();
                let arg = self.parse_unary()?;
                return Ok(self.mk_expr(
                    start,
                    ExprKind::Unary {
                        op,
                        arg: Box::new(arg),
                    },
                ));
            }
            if word == "await" {
                self.bump();
                let arg = self.parse_unary()?;
                return Ok(self.mk_expr(start, ExprKind::Await(Box::new(arg))));
            }
        }
        self.parse_postfix()
    }

    /// 後置 `++`/`--`、TS の `as`/`satisfies` の連鎖（`!` 非 null アサーションは
    /// `parse_call_member` 側で処理し、その後に続くメンバアクセスも読み進める）。
    fn parse_postfix(&mut self) -> PResult<Expr> {
        let start = self.cur_span();
        let mut expr = self.parse_call_member()?;
        loop {
            if !self.cur().newline_before
                && (self.is_punct(Punct::PlusPlus) || self.is_punct(Punct::MinusMinus))
            {
                let op = if self.is_punct(Punct::PlusPlus) {
                    UpdateOp::Inc
                } else {
                    UpdateOp::Dec
                };
                self.bump();
                expr = self.mk_expr(
                    start,
                    ExprKind::Update {
                        op,
                        arg: Box::new(expr),
                        prefix: false,
                    },
                );
                continue;
            }
            if self.is_word("as") {
                self.bump();
                if !self.eat_word("const") {
                    self.skip_type()?;
                }
                expr = self.mk_expr(start, ExprKind::TsAs(Box::new(expr)));
                continue;
            }
            if self.is_word("satisfies") {
                self.bump();
                self.skip_type()?;
                expr = self.mk_expr(start, ExprKind::TsAs(Box::new(expr)));
                continue;
            }
            break;
        }
        Ok(expr)
    }

    /// `new` 式・メンバアクセス・呼び出し・オプショナルチェーン。
    fn parse_call_member(&mut self) -> PResult<Expr> {
        let start = self.cur_span();
        let mut expr = if self.is_word("new") {
            self.parse_new_expr()?
        } else {
            self.parse_primary()?
        };
        loop {
            if self.eat_punct(Punct::Dot) {
                let name = self.parse_member_name()?;
                expr = self.mk_expr(
                    start,
                    ExprKind::Member {
                        obj: Box::new(expr),
                        prop: PropertyKey::Ident(name),
                        optional: false,
                    },
                );
            } else if self.eat_punct(Punct::QuestionDot) {
                if self.is_punct(Punct::LParen) {
                    let args = self.parse_args()?;
                    expr = self.mk_expr(
                        start,
                        ExprKind::Call {
                            callee: Box::new(expr),
                            args,
                            optional: true,
                        },
                    );
                } else if self.eat_punct(Punct::LBracket) {
                    let idx = self.parse_expr()?;
                    self.expect_punct(Punct::RBracket)?;
                    expr = self.mk_expr(
                        start,
                        ExprKind::Member {
                            obj: Box::new(expr),
                            prop: PropertyKey::Computed(Box::new(idx)),
                            optional: true,
                        },
                    );
                } else {
                    let name = self.parse_member_name()?;
                    expr = self.mk_expr(
                        start,
                        ExprKind::Member {
                            obj: Box::new(expr),
                            prop: PropertyKey::Ident(name),
                            optional: true,
                        },
                    );
                }
            } else if self.eat_punct(Punct::LBracket) {
                let idx = self.parse_expr()?;
                self.expect_punct(Punct::RBracket)?;
                expr = self.mk_expr(
                    start,
                    ExprKind::Member {
                        obj: Box::new(expr),
                        prop: PropertyKey::Computed(Box::new(idx)),
                        optional: false,
                    },
                );
            } else if self.is_punct(Punct::LParen) {
                let args = self.parse_args()?;
                expr = self.mk_expr(
                    start,
                    ExprKind::Call {
                        callee: Box::new(expr),
                        args,
                        optional: false,
                    },
                );
            } else if self.is_punct(Punct::Lt) {
                // `foo<T>(...)` 投機的パース。'<' が比較演算子の可能性もあるため、
                // 型引数として読めて、かつ直後に '(' が続く場合のみ確定させる。
                let mark = self.save();
                let looks_like_call = self.skip_type_args().is_ok() && self.is_punct(Punct::LParen);
                if looks_like_call {
                    let args = self.parse_args()?;
                    expr = self.mk_expr(
                        start,
                        ExprKind::Call {
                            callee: Box::new(expr),
                            args,
                            optional: false,
                        },
                    );
                } else {
                    self.restore(mark);
                    break;
                }
            } else if self.is_punct(Punct::Bang) {
                // 非 null アサーション `expr!`。後続の `.`/`[]`/`()` 連鎖も
                // このループでそのまま読み進められるようにここで処理する。
                self.bump();
                expr = self.mk_expr(start, ExprKind::TsNonNull(Box::new(expr)));
            } else {
                break;
            }
        }
        Ok(expr)
    }

    fn parse_member_name(&mut self) -> PResult<String> {
        if self.eat_punct(Punct::Hash) {
            // プライベートフィールドアクセス `this.#x`。
            let name = self.parse_binding_ident()?;
            return Ok(format!("#{name}"));
        }
        match self.ident_name() {
            Some(s) => {
                self.bump();
                Ok(s)
            }
            None => Err(self.err("プロパティ名が必要です")),
        }
    }

    fn parse_new_expr(&mut self) -> PResult<Expr> {
        let start = self.cur_span();
        self.expect_word("new")?;
        if self.eat_punct(Punct::Dot) {
            // new.target
            let _ = self.parse_binding_ident()?;
            return Ok(self.mk_expr(start, ExprKind::Ident("new.target".to_string())));
        }
        let callee = if self.is_word("new") {
            self.parse_new_expr()?
        } else {
            self.parse_member_no_call()?
        };
        if self.is_punct(Punct::Lt) {
            let mark = self.save();
            if !(self.skip_type_args().is_ok() && self.is_punct(Punct::LParen)) {
                self.restore(mark);
            }
        }
        let args = if self.is_punct(Punct::LParen) {
            self.parse_args()?
        } else {
            Vec::new()
        };
        Ok(self.mk_expr(
            start,
            ExprKind::New {
                callee: Box::new(callee),
                args,
            },
        ))
    }

    /// `new` の callee 用: 呼び出しを含まないメンバアクセスの連鎖。
    fn parse_member_no_call(&mut self) -> PResult<Expr> {
        let start = self.cur_span();
        let mut expr = self.parse_primary()?;
        loop {
            if self.eat_punct(Punct::Dot) {
                let name = self.parse_member_name()?;
                expr = self.mk_expr(
                    start,
                    ExprKind::Member {
                        obj: Box::new(expr),
                        prop: PropertyKey::Ident(name),
                        optional: false,
                    },
                );
            } else if self.eat_punct(Punct::LBracket) {
                let idx = self.parse_expr()?;
                self.expect_punct(Punct::RBracket)?;
                expr = self.mk_expr(
                    start,
                    ExprKind::Member {
                        obj: Box::new(expr),
                        prop: PropertyKey::Computed(Box::new(idx)),
                        optional: false,
                    },
                );
            } else {
                break;
            }
        }
        Ok(expr)
    }

    pub(super) fn parse_args(&mut self) -> PResult<Vec<Expr>> {
        self.expect_punct(Punct::LParen)?;
        let mut args = Vec::new();
        while !self.is_punct(Punct::RParen) {
            let start = self.cur_span();
            if self.eat_punct(Punct::DotDotDot) {
                let e = self.parse_assign_expr()?;
                args.push(self.mk_expr(start, ExprKind::Spread(Box::new(e))));
            } else {
                args.push(self.parse_assign_expr()?);
            }
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        self.expect_punct(Punct::RParen)?;
        Ok(args)
    }

    fn parse_primary(&mut self) -> PResult<Expr> {
        let start = self.cur_span();
        match &self.cur().kind {
            TokenKind::Number(n) => {
                let n = *n;
                self.bump();
                Ok(self.mk_expr(start, ExprKind::NumberLit(n)))
            }
            TokenKind::Str(s) => {
                let s = s.clone();
                self.bump();
                Ok(self.mk_expr(start, ExprKind::StringLit(s)))
            }
            TokenKind::NoSubTemplate(s) => {
                let s = s.clone();
                self.bump();
                Ok(self.mk_expr(
                    start,
                    ExprKind::TemplateLit {
                        quasis: vec![s],
                        exprs: vec![],
                    },
                ))
            }
            TokenKind::TemplateHead(s) => {
                let mut quasis = vec![s.clone()];
                self.bump();
                let mut exprs = Vec::new();
                loop {
                    let e = self.parse_expr()?;
                    exprs.push(e);
                    match &self.cur().kind {
                        TokenKind::TemplateMiddle(s2) => {
                            quasis.push(s2.clone());
                            self.bump();
                        }
                        TokenKind::TemplateTail(s2) => {
                            quasis.push(s2.clone());
                            self.bump();
                            break;
                        }
                        _ => return Err(self.err("テンプレートリテラルが不正です")),
                    }
                }
                Ok(self.mk_expr(start, ExprKind::TemplateLit { quasis, exprs }))
            }
            TokenKind::Regex { pattern, flags } => {
                let pattern = pattern.clone();
                let flags = flags.clone();
                self.bump();
                Ok(self.mk_expr(start, ExprKind::RegexLit { pattern, flags }))
            }
            TokenKind::Punct(Punct::LParen) => {
                self.bump();
                let e = self.parse_expr()?;
                self.expect_punct(Punct::RParen)?;
                Ok(e)
            }
            TokenKind::Punct(Punct::LBracket) => self.parse_array_lit(),
            TokenKind::Punct(Punct::LBrace) => self.parse_object_lit(),
            TokenKind::Ident(_) => self.parse_ident_primary(),
            other => {
                let msg = format!("式が必要ですが {other:?} が見つかりました");
                Err(self.err(msg))
            }
        }
    }

    fn parse_ident_primary(&mut self) -> PResult<Expr> {
        let start = self.cur_span();
        let word = self.cur_ident().unwrap();
        match word {
            "function" => {
                self.bump();
                self.parse_function_expr(false)
            }
            "async" if self.peek_word_is(1, "function") && !self.peek_kind_newline(1) => {
                self.bump();
                self.bump();
                self.parse_function_expr(true)
            }
            "class" => self.parse_class_expr(),
            "this" => {
                self.bump();
                Ok(self.mk_expr(start, ExprKind::This))
            }
            "super" => {
                self.bump();
                Ok(self.mk_expr(start, ExprKind::Super))
            }
            "true" => {
                self.bump();
                Ok(self.mk_expr(start, ExprKind::BoolLit(true)))
            }
            "false" => {
                self.bump();
                Ok(self.mk_expr(start, ExprKind::BoolLit(false)))
            }
            "null" => {
                self.bump();
                Ok(self.mk_expr(start, ExprKind::NullLit))
            }
            "undefined" => {
                self.bump();
                Ok(self.mk_expr(start, ExprKind::UndefinedLit))
            }
            _ => {
                let name = word.to_owned();
                self.bump();
                Ok(self.mk_expr(start, ExprKind::Ident(name)))
            }
        }
    }

    pub(super) fn peek_word_is(&self, offset: usize, word: &str) -> bool {
        matches!(self.peek_kind(offset), TokenKind::Ident(s) if s == word)
    }

    /// offset 番目のトークンの直前に改行があるか（`async` の直後に改行があると
    /// アロー/関数式とはみなされないという ASI 由来の規則の簡易対応）。
    pub(super) fn peek_kind_newline(&self, offset: usize) -> bool {
        let idx = (self.pos + offset).min(self.tokens.len() - 1);
        self.tokens[idx].newline_before
    }

    fn parse_array_lit(&mut self) -> PResult<Expr> {
        let start = self.cur_span();
        self.expect_punct(Punct::LBracket)?;
        let mut elements = Vec::new();
        while !self.is_punct(Punct::RBracket) {
            if self.is_punct(Punct::Comma) {
                elements.push(None);
                self.bump();
                continue;
            }
            let el_start = self.cur_span();
            if self.eat_punct(Punct::DotDotDot) {
                let e = self.parse_assign_expr()?;
                elements.push(Some(self.mk_expr(el_start, ExprKind::Spread(Box::new(e)))));
            } else {
                elements.push(Some(self.parse_assign_expr()?));
            }
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        self.expect_punct(Punct::RBracket)?;
        Ok(self.mk_expr(start, ExprKind::ArrayLit(elements)))
    }

    fn parse_object_lit(&mut self) -> PResult<Expr> {
        let start = self.cur_span();
        self.expect_punct(Punct::LBrace)?;
        let mut props = Vec::new();
        while !self.is_punct(Punct::RBrace) {
            props.push(self.parse_object_prop()?);
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        self.expect_punct(Punct::RBrace)?;
        Ok(self.mk_expr(start, ExprKind::ObjectLit(props)))
    }

    fn parse_object_prop(&mut self) -> PResult<ObjectProp> {
        if self.eat_punct(Punct::DotDotDot) {
            let e = self.parse_assign_expr()?;
            return Ok(ObjectProp::Spread(Box::new(e)));
        }

        // get/set アクセサ shorthand の投機的判定。
        if self.is_word("get") || self.is_word("set") {
            let is_get = self.is_word("get");
            let mark = self.save();
            self.bump();
            if !self.is_punct(Punct::Colon)
                && !self.is_punct(Punct::Comma)
                && !self.is_punct(Punct::RBrace)
                && !self.is_punct(Punct::LParen)
            {
                if let Ok(key) = self.parse_property_key() {
                    if self.is_punct(Punct::LParen) {
                        let function = self.parse_method_function(false)?;
                        return Ok(ObjectProp::Method {
                            key,
                            function: Box::new(function),
                            kind: if is_get {
                                MethodKind::Get
                            } else {
                                MethodKind::Set
                            },
                        });
                    }
                }
            }
            self.restore(mark);
        }

        let is_async = if self.is_word("async")
            && !self.peek_kind_newline(1)
            && !self.peek_is_punct(1, Punct::Colon)
            && !self.peek_is_punct(1, Punct::Comma)
        {
            self.bump();
            true
        } else {
            false
        };

        let key = self.parse_property_key()?;
        if self.is_punct(Punct::LParen) {
            let function = self.parse_method_function(is_async)?;
            return Ok(ObjectProp::Method {
                key,
                function: Box::new(function),
                kind: MethodKind::Normal,
            });
        }
        if self.eat_punct(Punct::Colon) {
            let value = self.parse_assign_expr()?;
            return Ok(ObjectProp::KeyValue {
                key,
                value: Box::new(value),
                shorthand: false,
            });
        }
        // ショートハンド { name }
        let name = match &key {
            PropertyKey::Ident(s) => s.clone(),
            _ => return Err(self.err("プロパティの省略記法には識別子が必要です")),
        };
        let value_span = self.cur_span();
        let value = self.mk_expr(value_span, ExprKind::Ident(name));
        Ok(ObjectProp::KeyValue {
            key,
            value: Box::new(value),
            shorthand: true,
        })
    }

    /// アロー関数を投機的に解析する。マッチしなければ None を返し位置を戻す。
    pub(super) fn try_parse_arrow(&mut self) -> PResult<Option<Expr>> {
        // 大半の式は `(` / `async` / `ident =>` ではないため最速で弾く。
        let quick_possible = match &self.cur().kind {
            TokenKind::Punct(Punct::LParen) => true,
            TokenKind::Ident(s) => s == "async" || self.peek_is_punct(1, Punct::Arrow),
            _ => false,
        };
        if !quick_possible {
            return Ok(None);
        }
        let mark = self.save();
        let start = self.cur_span();
        let mut is_async = false;
        if self.is_word("async") && !self.peek_kind_newline(1) {
            let async_arrow_head = self.peek_is_punct(1, Punct::LParen)
                || (matches!(self.peek_kind(1), TokenKind::Ident(_))
                    && self.peek_is_punct(2, Punct::Arrow));
            if async_arrow_head {
                self.bump();
                is_async = true;
            }
        }

        // 裸パラメータ 1 個: x => ...
        if self.peek_is_punct(1, Punct::Arrow) {
            if let Some(word) = self.cur_ident() {
                if !is_keyword_like(word) {
                    let name = word.to_owned();
                    self.bump();
                    self.bump(); // =>
                    let param = Param {
                        pattern: Pattern::Ident(name),
                        default: None,
                        rest: false,
                    };
                    let (body, expr_body) = self.parse_arrow_body()?;
                    let func = Function {
                        id: self.fresh_id(),
                        span: Span::new(start.start, self.prev_end()),
                        name: None,
                        params: vec![param],
                        body,
                        expr_body,
                        is_async,
                        is_arrow: true,
                    };
                    return Ok(Some(self.mk_expr(start, ExprKind::Arrow(Box::new(func)))));
                }
            }
        }

        if self.is_punct(Punct::LParen) {
            let attempt = self.parse_param_list();
            if let Ok(params) = attempt {
                if self.eat_punct(Punct::Colon) {
                    let _ = self.skip_type();
                }
                if self.is_punct(Punct::Arrow) {
                    self.bump();
                    let (body, expr_body) = self.parse_arrow_body()?;
                    let func = Function {
                        id: self.fresh_id(),
                        span: Span::new(start.start, self.prev_end()),
                        name: None,
                        params,
                        body,
                        expr_body,
                        is_async,
                        is_arrow: true,
                    };
                    return Ok(Some(self.mk_expr(start, ExprKind::Arrow(Box::new(func)))));
                }
            }
        }

        self.restore(mark);
        Ok(None)
    }

    fn parse_arrow_body(&mut self) -> PResult<ArrowBody> {
        if self.is_punct(Punct::LBrace) {
            let body = self.parse_block_stmts()?;
            Ok((Some(body), None))
        } else {
            let e = self.parse_assign_expr()?;
            Ok((None, Some(Box::new(e))))
        }
    }

    /// 仮引数リスト `(a, b: T = 1, ...rest)` を読む。型注釈・デフォルト値・
    /// rest・分割代入パターンすべてに対応する。
    pub(super) fn parse_param_list(&mut self) -> PResult<Vec<Param>> {
        self.expect_punct(Punct::LParen)?;
        let mut params = Vec::new();
        while !self.is_punct(Punct::RParen) {
            // アクセス修飾子 (constructor 引数プロパティ) は読み飛ばす。
            loop {
                if self.is_word("public")
                    || self.is_word("private")
                    || self.is_word("protected")
                    || self.is_word("readonly")
                {
                    self.bump();
                } else {
                    break;
                }
            }
            let rest = self.eat_punct(Punct::DotDotDot);
            let pattern = self.parse_binding_pattern()?;
            self.eat_punct(Punct::Question); // 任意パラメータ `?`
            if self.eat_punct(Punct::Colon) {
                self.skip_type()?;
            }
            let default = if self.eat_punct(Punct::Eq) {
                Some(Box::new(self.parse_assign_expr()?))
            } else {
                None
            };
            params.push(Param {
                pattern,
                default,
                rest,
            });
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        self.expect_punct(Punct::RParen)?;
        Ok(params)
    }

    /// `function` キーワードの後ろから読む関数式。
    pub(super) fn parse_function_expr(&mut self, is_async: bool) -> PResult<Expr> {
        let start = self.cur_span();
        let func = self.parse_function_tail(is_async, true)?;
        Ok(self.mk_expr(start, ExprKind::Function(Box::new(func))))
    }

    /// `function` の次のトークン（関数名の可能性がある識別子、または `(`）から、
    /// ジェネリクス・引数リスト・戻り値型・本体までを読む共通処理。
    /// `name_optional` は式（無名可）か宣言（有名前提）かの違いに使う。
    pub(super) fn parse_function_tail(
        &mut self,
        is_async: bool,
        name_optional: bool,
    ) -> PResult<Function> {
        let start = self.cur_span();
        self.eat_punct(Punct::Star); // generator の '*' は読み飛ばす（非対応）
        let name = if !self.is_punct(Punct::LParen) && !self.is_punct(Punct::Lt) {
            match self.ident_name() {
                Some(n) => {
                    self.bump();
                    Some(n)
                }
                None => None,
            }
        } else {
            None
        };
        if name.is_none() && !name_optional {
            return Err(self.err("関数名が必要です"));
        }
        self.skip_type_params()?;
        let params = self.parse_param_list()?;
        if self.eat_punct(Punct::Colon) {
            self.skip_type()?;
        }
        let body = if self.is_punct(Punct::LBrace) {
            Some(self.parse_block_stmts()?)
        } else {
            // アンビエント宣言（本体なし）。ASI で `;` を読む。
            self.consume_semi()?;
            None
        };
        Ok(Function {
            id: self.fresh_id(),
            span: Span::new(start.start, self.prev_end()),
            name,
            params,
            body,
            expr_body: None,
            is_async,
            is_arrow: false,
        })
    }

    fn parse_class_expr(&mut self) -> PResult<Expr> {
        let start = self.cur_span();
        let class = self.parse_class_tail()?;
        Ok(self.mk_expr(start, ExprKind::Class(Box::new(class))))
    }

    fn parse_method_function(&mut self, is_async: bool) -> PResult<Function> {
        self.parse_function_tail(is_async, true)
    }

    /// クラスメンバ/オブジェクトメソッドなど、名前を呼び出し元がすでに
    /// 読んだ後（プロパティキーとして）から、ジェネリクス・引数・戻り値型・
    /// 本体を読む。
    pub(super) fn parse_function_tail_no_name(&mut self, is_async: bool) -> PResult<Function> {
        let start = self.cur_span();
        self.skip_type_params()?;
        let params = self.parse_param_list()?;
        if self.eat_punct(Punct::Colon) {
            self.skip_type()?;
        }
        let body = if self.is_punct(Punct::LBrace) {
            Some(self.parse_block_stmts()?)
        } else {
            self.consume_semi()?;
            None
        };
        Ok(Function {
            id: self.fresh_id(),
            span: Span::new(start.start, self.prev_end()),
            name: None,
            params,
            body,
            expr_body: None,
            is_async,
            is_arrow: false,
        })
    }
}

pub(super) fn is_keyword_like(name: &str) -> bool {
    matches!(
        name,
        "if" | "else"
            | "for"
            | "while"
            | "do"
            | "switch"
            | "case"
            | "default"
            | "break"
            | "continue"
            | "return"
            | "throw"
            | "try"
            | "catch"
            | "finally"
            | "function"
            | "class"
            | "new"
            | "delete"
            | "typeof"
            | "instanceof"
            | "in"
            | "of"
            | "void"
            | "this"
            | "super"
            | "null"
            | "true"
            | "false"
            | "undefined"
            | "import"
            | "export"
            | "const"
            | "let"
            | "var"
            | "yield"
            | "await"
    )
}
