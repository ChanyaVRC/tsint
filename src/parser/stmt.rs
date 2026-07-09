//! 文のパース: 宣言・制御構造・class・import/export・TS のアンビエント宣言。

use super::expr::is_keyword_like;
use super::{PResult, Parser};
use crate::ast::*;
use crate::lexer::{Punct, TokenKind};

impl Parser {
    pub(super) fn parse_stmt(&mut self) -> PResult<Stmt> {
        let start = self.cur_span();
        if self.is_punct(Punct::Semi) {
            self.bump();
            return Ok(self.mk_stmt(start, StmtKind::Empty));
        }
        if self.is_punct(Punct::LBrace) {
            let body = self.parse_block_stmts()?;
            return Ok(self.mk_stmt(start, StmtKind::Block(body)));
        }
        let word = match self.ident_name() {
            Some(w) => w,
            None => return self.parse_expr_stmt(),
        };
        match word.as_str() {
            "var" | "let" | "const" => self.parse_var_decl_stmt(),
            "function" => {
                self.bump();
                let f = self.parse_function_tail(false, false)?;
                Ok(self.mk_stmt(start, StmtKind::FunctionDecl(Box::new(f))))
            }
            "async" if self.peek_word_is(1, "function") && !self.peek_kind_newline(1) => {
                self.bump();
                self.bump();
                let f = self.parse_function_tail(true, false)?;
                Ok(self.mk_stmt(start, StmtKind::FunctionDecl(Box::new(f))))
            }
            "class" => {
                let c = self.parse_class_tail()?;
                Ok(self.mk_stmt(start, StmtKind::ClassDecl(Box::new(c))))
            }
            "if" => self.parse_if_stmt(),
            "while" => self.parse_while_stmt(),
            "do" => self.parse_do_while_stmt(),
            "for" => self.parse_for_stmt(),
            "switch" => self.parse_switch_stmt(),
            "try" => self.parse_try_stmt(),
            "throw" => self.parse_throw_stmt(),
            "return" => self.parse_return_stmt(),
            "break" => self.parse_break_continue(true),
            "continue" => self.parse_break_continue(false),
            "import" => self.parse_import_stmt(),
            "export" => self.parse_export_stmt(),
            "interface" => {
                let name = self.parse_interface_decl()?;
                Ok(self.mk_stmt(start, StmtKind::TsInterface { name }))
            }
            "type" if matches!(self.peek_kind(1), TokenKind::Ident(_)) => {
                let name = self.parse_type_alias_decl()?;
                Ok(self.mk_stmt(start, StmtKind::TsTypeAlias { name }))
            }
            "declare" => self.parse_declare_stmt(),
            "debugger" => {
                self.bump();
                self.consume_semi()?;
                Ok(self.mk_stmt(start, StmtKind::Debugger))
            }
            _ => {
                if self.peek_is_punct(1, Punct::Colon) && !is_keyword_like(&word) {
                    self.bump(); // ラベル名
                    self.bump(); // ':'
                    let body = Box::new(self.parse_stmt()?);
                    return Ok(self.mk_stmt(start, StmtKind::Labeled { label: word, body }));
                }
                self.parse_expr_stmt()
            }
        }
    }

    pub(super) fn parse_block_stmts(&mut self) -> PResult<Vec<Stmt>> {
        self.expect_punct(Punct::LBrace)?;
        let mut stmts = Vec::new();
        while !self.is_punct(Punct::RBrace) {
            stmts.push(self.parse_stmt()?);
        }
        self.expect_punct(Punct::RBrace)?;
        Ok(stmts)
    }

    fn parse_expr_stmt(&mut self) -> PResult<Stmt> {
        let start = self.cur_span();
        let e = self.parse_expr()?;
        self.consume_semi()?;
        Ok(self.mk_stmt(start, StmtKind::Expr(Box::new(e))))
    }

    // ---------- 変数宣言 ----------

    fn var_kind_from_word(word: &str) -> VarKind {
        match word {
            "let" => VarKind::Let,
            "const" => VarKind::Const,
            _ => VarKind::Var,
        }
    }

    fn parse_var_decl_stmt(&mut self) -> PResult<Stmt> {
        let start = self.cur_span();
        let word = self.parse_binding_ident()?;
        let kind = Self::var_kind_from_word(&word);
        let decls = self.parse_var_declarators()?;
        self.consume_semi()?;
        Ok(self.mk_stmt(start, StmtKind::VarDecl { kind, decls }))
    }

    /// `,` 区切りの宣言子リスト。`for (let ...)` の init 部分とも共有する。
    fn parse_var_declarators(&mut self) -> PResult<Vec<VarDeclarator>> {
        let mut decls = Vec::new();
        loop {
            let pattern = self.parse_binding_pattern()?;
            self.eat_punct(Punct::Bang); // 確定代入アサーション `let x!: T;`
            if self.eat_punct(Punct::Colon) {
                self.skip_type()?;
            }
            let init = if self.eat_punct(Punct::Eq) {
                Some(Box::new(self.parse_assign_expr()?))
            } else {
                None
            };
            decls.push(VarDeclarator {
                name: pattern,
                init,
            });
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        Ok(decls)
    }

    // ---------- 制御構造 ----------

    fn parse_if_stmt(&mut self) -> PResult<Stmt> {
        let start = self.cur_span();
        self.expect_word("if")?;
        self.expect_punct(Punct::LParen)?;
        let test = self.parse_expr()?;
        self.expect_punct(Punct::RParen)?;
        let cons = Box::new(self.parse_stmt()?);
        let alt = if self.eat_word("else") {
            Some(Box::new(self.parse_stmt()?))
        } else {
            None
        };
        Ok(self.mk_stmt(
            start,
            StmtKind::If {
                test: Box::new(test),
                cons,
                alt,
            },
        ))
    }

    fn parse_while_stmt(&mut self) -> PResult<Stmt> {
        let start = self.cur_span();
        self.expect_word("while")?;
        self.expect_punct(Punct::LParen)?;
        let test = self.parse_expr()?;
        self.expect_punct(Punct::RParen)?;
        let body = Box::new(self.parse_stmt()?);
        Ok(self.mk_stmt(
            start,
            StmtKind::While {
                test: Box::new(test),
                body,
            },
        ))
    }

    fn parse_do_while_stmt(&mut self) -> PResult<Stmt> {
        let start = self.cur_span();
        self.expect_word("do")?;
        let body = Box::new(self.parse_stmt()?);
        self.expect_word("while")?;
        self.expect_punct(Punct::LParen)?;
        let test = self.parse_expr()?;
        self.expect_punct(Punct::RParen)?;
        self.consume_semi()?;
        Ok(self.mk_stmt(
            start,
            StmtKind::DoWhile {
                body,
                test: Box::new(test),
            },
        ))
    }

    fn parse_for_stmt(&mut self) -> PResult<Stmt> {
        let start = self.cur_span();
        self.expect_word("for")?;
        let is_await = self.eat_word("await");
        self.expect_punct(Punct::LParen)?;

        if self.is_word("var") || self.is_word("let") || self.is_word("const") {
            let word = self.parse_binding_ident()?;
            let kind = Self::var_kind_from_word(&word);
            let first_pattern = self.parse_binding_pattern()?;
            if self.eat_punct(Punct::Colon) {
                self.skip_type()?;
            }
            if self.is_word("of") || self.is_word("in") {
                let is_of = self.is_word("of");
                self.bump();
                let right = if is_of {
                    self.parse_assign_expr()?
                } else {
                    self.parse_expr()?
                };
                self.expect_punct(Punct::RParen)?;
                let body = Box::new(self.parse_stmt()?);
                let left = Box::new(ForInit::VarDecl(
                    kind,
                    vec![VarDeclarator {
                        name: first_pattern,
                        init: None,
                    }],
                ));
                return Ok(self.mk_stmt(
                    start,
                    StmtKind::ForIn {
                        left,
                        right: Box::new(right),
                        body,
                        is_of,
                        is_await,
                    },
                ));
            }
            let first_init = if self.eat_punct(Punct::Eq) {
                Some(Box::new(self.parse_assign_expr()?))
            } else {
                None
            };
            let mut decls = vec![VarDeclarator {
                name: first_pattern,
                init: first_init,
            }];
            while self.eat_punct(Punct::Comma) {
                let pat = self.parse_binding_pattern()?;
                if self.eat_punct(Punct::Colon) {
                    self.skip_type()?;
                }
                let init = if self.eat_punct(Punct::Eq) {
                    Some(Box::new(self.parse_assign_expr()?))
                } else {
                    None
                };
                decls.push(VarDeclarator { name: pat, init });
            }
            self.expect_punct(Punct::Semi)?;
            let test = if !self.is_punct(Punct::Semi) {
                Some(Box::new(self.parse_expr()?))
            } else {
                None
            };
            self.expect_punct(Punct::Semi)?;
            let update = if !self.is_punct(Punct::RParen) {
                Some(Box::new(self.parse_expr()?))
            } else {
                None
            };
            self.expect_punct(Punct::RParen)?;
            let body = Box::new(self.parse_stmt()?);
            return Ok(self.mk_stmt(
                start,
                StmtKind::For {
                    init: Some(Box::new(ForInit::VarDecl(kind, decls))),
                    test,
                    update,
                    body,
                },
            ));
        }

        if self.is_punct(Punct::Semi) {
            self.bump();
            let test = if !self.is_punct(Punct::Semi) {
                Some(Box::new(self.parse_expr()?))
            } else {
                None
            };
            self.expect_punct(Punct::Semi)?;
            let update = if !self.is_punct(Punct::RParen) {
                Some(Box::new(self.parse_expr()?))
            } else {
                None
            };
            self.expect_punct(Punct::RParen)?;
            let body = Box::new(self.parse_stmt()?);
            return Ok(self.mk_stmt(
                start,
                StmtKind::For {
                    init: None,
                    test,
                    update,
                    body,
                },
            ));
        }

        // 式から始まる init（`for (a of b)` / 通常の for-loop 両対応）。
        let first_expr = self.parse_expr()?;
        if self.is_word("of") || self.is_word("in") {
            let is_of = self.is_word("of");
            self.bump();
            let right = if is_of {
                self.parse_assign_expr()?
            } else {
                self.parse_expr()?
            };
            self.expect_punct(Punct::RParen)?;
            let body = Box::new(self.parse_stmt()?);
            return Ok(self.mk_stmt(
                start,
                StmtKind::ForIn {
                    left: Box::new(ForInit::Expr(Box::new(first_expr))),
                    right: Box::new(right),
                    body,
                    is_of,
                    is_await,
                },
            ));
        }
        self.expect_punct(Punct::Semi)?;
        let test = if !self.is_punct(Punct::Semi) {
            Some(Box::new(self.parse_expr()?))
        } else {
            None
        };
        self.expect_punct(Punct::Semi)?;
        let update = if !self.is_punct(Punct::RParen) {
            Some(Box::new(self.parse_expr()?))
        } else {
            None
        };
        self.expect_punct(Punct::RParen)?;
        let body = Box::new(self.parse_stmt()?);
        Ok(self.mk_stmt(
            start,
            StmtKind::For {
                init: Some(Box::new(ForInit::Expr(Box::new(first_expr)))),
                test,
                update,
                body,
            },
        ))
    }

    fn parse_switch_stmt(&mut self) -> PResult<Stmt> {
        let start = self.cur_span();
        self.expect_word("switch")?;
        self.expect_punct(Punct::LParen)?;
        let disc = self.parse_expr()?;
        self.expect_punct(Punct::RParen)?;
        self.expect_punct(Punct::LBrace)?;
        let mut cases = Vec::new();
        while !self.is_punct(Punct::RBrace) {
            if self.eat_word("case") {
                let test = self.parse_expr()?;
                self.expect_punct(Punct::Colon)?;
                let body = self.parse_case_body()?;
                cases.push(SwitchCase {
                    test: Some(Box::new(test)),
                    body,
                });
            } else if self.eat_word("default") {
                self.expect_punct(Punct::Colon)?;
                let body = self.parse_case_body()?;
                cases.push(SwitchCase { test: None, body });
            } else {
                return Err(self.err("'case' か 'default' が必要です"));
            }
        }
        self.expect_punct(Punct::RBrace)?;
        Ok(self.mk_stmt(
            start,
            StmtKind::Switch {
                disc: Box::new(disc),
                cases,
            },
        ))
    }

    fn parse_case_body(&mut self) -> PResult<Vec<Stmt>> {
        let mut stmts = Vec::new();
        while !self.is_punct(Punct::RBrace) && !self.is_word("case") && !self.is_word("default") {
            stmts.push(self.parse_stmt()?);
        }
        Ok(stmts)
    }

    fn parse_try_stmt(&mut self) -> PResult<Stmt> {
        let start = self.cur_span();
        self.expect_word("try")?;
        let block = self.parse_block_stmts()?;
        let mut handler = None;
        let mut finalizer = None;
        if self.eat_word("catch") {
            let param = if self.eat_punct(Punct::LParen) {
                let p = self.parse_binding_pattern()?;
                if self.eat_punct(Punct::Colon) {
                    self.skip_type()?;
                }
                self.expect_punct(Punct::RParen)?;
                Some(p)
            } else {
                None
            };
            let body = self.parse_block_stmts()?;
            handler = Some(CatchClause { param, body });
        }
        if self.eat_word("finally") {
            finalizer = Some(self.parse_block_stmts()?);
        }
        if handler.is_none() && finalizer.is_none() {
            return Err(self.err("try には catch か finally が必要です"));
        }
        Ok(self.mk_stmt(
            start,
            StmtKind::Try {
                block,
                handler,
                finalizer,
            },
        ))
    }

    fn parse_throw_stmt(&mut self) -> PResult<Stmt> {
        let start = self.cur_span();
        self.expect_word("throw")?;
        if self.cur().newline_before {
            return Err(self.err("'throw' の直後で改行することはできません"));
        }
        let e = self.parse_expr()?;
        self.consume_semi()?;
        Ok(self.mk_stmt(start, StmtKind::Throw(Box::new(e))))
    }

    fn parse_return_stmt(&mut self) -> PResult<Stmt> {
        let start = self.cur_span();
        self.expect_word("return")?;
        let arg = if self.is_punct(Punct::Semi)
            || self.is_punct(Punct::RBrace)
            || self.is_eof()
            || self.cur().newline_before
        {
            None
        } else {
            Some(Box::new(self.parse_expr()?))
        };
        self.consume_semi()?;
        Ok(self.mk_stmt(start, StmtKind::Return(arg)))
    }

    fn parse_break_continue(&mut self, is_break: bool) -> PResult<Stmt> {
        let start = self.cur_span();
        self.bump(); // break/continue
        let label = if !self.cur().newline_before {
            match self.ident_name() {
                Some(name) if !is_keyword_like(&name) => {
                    self.bump();
                    Some(name)
                }
                _ => None,
            }
        } else {
            None
        };
        self.consume_semi()?;
        let kind = if is_break {
            StmtKind::Break(label)
        } else {
            StmtKind::Continue(label)
        };
        Ok(self.mk_stmt(start, kind))
    }

    // ---------- クラス ----------

    pub(super) fn parse_class_tail(&mut self) -> PResult<Class> {
        let start = self.cur_span();
        self.expect_word("class")?;
        let name = if self.is_word("extends") || self.is_punct(Punct::LBrace) {
            None
        } else {
            match self.ident_name() {
                Some(n) => {
                    self.bump();
                    Some(n)
                }
                None => None,
            }
        };
        self.skip_type_params()?;
        let super_class = if self.eat_word("extends") {
            let e = self.parse_assign_expr()?;
            Some(Box::new(e))
        } else {
            None
        };
        if self.eat_word("implements") {
            loop {
                self.skip_type()?;
                if !self.eat_punct(Punct::Comma) {
                    break;
                }
            }
        }
        self.expect_punct(Punct::LBrace)?;
        let mut members = Vec::new();
        while !self.is_punct(Punct::RBrace) {
            if self.eat_punct(Punct::Semi) {
                continue;
            }
            members.push(self.parse_class_member()?);
        }
        self.expect_punct(Punct::RBrace)?;
        Ok(Class {
            id: self.fresh_id(),
            span: Span::new(start.start, self.prev_end()),
            name,
            super_class,
            members,
        })
    }

    /// 次のトークンが修飾子として消費してよいか（消費すると本来のメンバ名を
    /// 読み損ねてしまわないか）を、その次のトークンから判定する。
    fn next_token_is_member_boundary(&self) -> bool {
        matches!(
            self.peek_kind(1),
            TokenKind::Punct(Punct::LParen)
                | TokenKind::Punct(Punct::Colon)
                | TokenKind::Punct(Punct::Eq)
                | TokenKind::Punct(Punct::Semi)
                | TokenKind::Punct(Punct::Question)
                | TokenKind::Punct(Punct::Bang)
                | TokenKind::Punct(Punct::Comma)
                | TokenKind::Punct(Punct::RBrace)
                | TokenKind::Punct(Punct::Lt)
        )
    }

    fn parse_class_member(&mut self) -> PResult<ClassMember> {
        let mut is_static = false;
        while let Some(word) = self.ident_name() {
            let is_modifier = matches!(
                word.as_str(),
                "public" | "private" | "protected" | "readonly" | "abstract" | "override" | "declare" | "static"
            );
            if is_modifier && !self.next_token_is_member_boundary() {
                if word == "static" {
                    is_static = true;
                }
                self.bump();
                continue;
            }
            break;
        }

        // static 初期化ブロック `static { ... }` は中身ごと読み飛ばす。
        if is_static && self.is_punct(Punct::LBrace) {
            self.skip_balanced_group()?;
            return Ok(ClassMember {
                kind: ClassMemberKind::Field,
                is_static: true,
                name: PropertyKey::Ident("<static-block>".to_string()),
                function: None,
                value: None,
            });
        }

        let mut kind = ClassMemberKind::Method;
        if (self.is_word("get") || self.is_word("set")) && !self.next_token_is_member_boundary() {
            kind = if self.is_word("get") {
                ClassMemberKind::Getter
            } else {
                ClassMemberKind::Setter
            };
            self.bump();
        }

        let mut is_async = false;
        if self.is_word("async") && !self.next_token_is_member_boundary() && !self.peek_kind_newline(1) {
            is_async = true;
            self.bump();
        }
        self.eat_punct(Punct::Star); // generator '*' は非対応、読み飛ばすだけ

        let name = if self.eat_punct(Punct::Hash) {
            let n = self.parse_binding_ident()?;
            PropertyKey::Ident(format!("#{n}"))
        } else {
            self.parse_property_key()?
        };

        self.eat_punct(Punct::Question); // 任意メンバ `?`
        self.eat_punct(Punct::Bang); // 確定代入アサーション `!`

        if self.is_punct(Punct::LParen) || self.is_punct(Punct::Lt) {
            let function = self.parse_function_tail_no_name(is_async)?;
            let is_ctor = matches!(&name, PropertyKey::Ident(n) if n == "constructor") && kind == ClassMemberKind::Method;
            return Ok(ClassMember {
                kind: if is_ctor { ClassMemberKind::Constructor } else { kind },
                is_static,
                name,
                function: Some(function),
                value: None,
            });
        }

        // フィールド宣言。
        if self.eat_punct(Punct::Colon) {
            self.skip_type()?;
        }
        let value = if self.eat_punct(Punct::Eq) {
            Some(Box::new(self.parse_assign_expr()?))
        } else {
            None
        };
        self.consume_semi()?;
        Ok(ClassMember {
            kind: ClassMemberKind::Field,
            is_static,
            name,
            function: None,
            value,
        })
    }

    // ---------- import/export ----------

    fn parse_import_stmt(&mut self) -> PResult<Stmt> {
        let start = self.cur_span();
        self.expect_word("import")?;
        if let TokenKind::Str(s) = self.cur().kind.clone() {
            self.bump();
            self.consume_semi()?;
            return Ok(self.mk_stmt(
                start,
                StmtKind::Import(ImportDecl {
                    specifiers: vec![],
                    source: s,
                    type_only: false,
                }),
            ));
        }

        let type_only = if self.is_word("type")
            && !self.peek_is_punct(1, Punct::Comma)
            && !self.peek_word_is(1, "from")
        {
            self.bump();
            true
        } else {
            false
        };

        let mut specifiers = Vec::new();
        if !self.is_punct(Punct::Star) && !self.is_punct(Punct::LBrace) {
            let name = self.parse_binding_ident()?;
            specifiers.push(ImportSpecifier::Default(name));
            self.eat_punct(Punct::Comma);
        }
        if self.eat_punct(Punct::Star) {
            self.expect_word("as")?;
            let ns = self.parse_binding_ident()?;
            specifiers.push(ImportSpecifier::Namespace(ns));
        } else if self.eat_punct(Punct::LBrace) {
            while !self.is_punct(Punct::RBrace) {
                self.eat_word("type"); // 個別指定の `type` 修飾子は読み捨てる
                let imported = self.parse_export_name()?;
                let local = if self.eat_word("as") {
                    self.parse_binding_ident()?
                } else {
                    imported.clone()
                };
                specifiers.push(ImportSpecifier::Named { imported, local });
                if !self.eat_punct(Punct::Comma) {
                    break;
                }
            }
            self.expect_punct(Punct::RBrace)?;
        }
        self.expect_word("from")?;
        let source = self.expect_string_lit()?;
        self.consume_semi()?;
        Ok(self.mk_stmt(
            start,
            StmtKind::Import(ImportDecl {
                specifiers,
                source,
                type_only,
            }),
        ))
    }

    fn parse_export_stmt(&mut self) -> PResult<Stmt> {
        let start = self.cur_span();
        self.expect_word("export")?;

        if self.eat_word("default") {
            return self.parse_export_default(start);
        }
        if self.is_punct(Punct::Star) {
            return self.parse_export_all(start);
        }

        let mut type_only = false;
        if self.is_word("type") && (self.peek_is_punct(1, Punct::LBrace) || self.peek_is_punct(1, Punct::Star)) {
            self.bump();
            type_only = true;
        }
        if self.is_punct(Punct::Star) {
            return self.parse_export_all(start);
        }

        if self.is_punct(Punct::LBrace) {
            self.bump();
            let mut specifiers = Vec::new();
            while !self.is_punct(Punct::RBrace) {
                let local = self.parse_export_name()?;
                let exported = if self.eat_word("as") {
                    self.parse_export_name()?
                } else {
                    local.clone()
                };
                specifiers.push(ExportSpecifier::Named { local, exported });
                if !self.eat_punct(Punct::Comma) {
                    break;
                }
            }
            self.expect_punct(Punct::RBrace)?;
            let source = if self.eat_word("from") {
                Some(self.expect_string_lit()?)
            } else {
                None
            };
            self.consume_semi()?;
            return Ok(self.mk_stmt(
                start,
                StmtKind::ExportNamed {
                    specifiers,
                    source,
                    type_only,
                },
            ));
        }

        if type_only {
            return Err(self.err("'export type' の後には '{' か '*' が必要です"));
        }

        if self.is_word("interface") {
            let name = self.parse_interface_decl()?;
            return Ok(self.mk_stmt(start, StmtKind::TsInterface { name }));
        }
        if self.is_word("type") {
            let name = self.parse_type_alias_decl()?;
            return Ok(self.mk_stmt(start, StmtKind::TsTypeAlias { name }));
        }

        let inner = self.parse_stmt()?;
        Ok(self.mk_stmt(start, StmtKind::ExportDecl(Box::new(inner))))
    }

    fn parse_export_default(&mut self, start: Span) -> PResult<Stmt> {
        if self.is_word("function") {
            self.bump();
            let f = self.parse_function_tail(false, true)?;
            return Ok(self.mk_stmt(
                start,
                StmtKind::ExportDefault(Box::new(DefaultExportable::Function(Box::new(f)))),
            ));
        }
        if self.is_word("async") && self.peek_word_is(1, "function") {
            self.bump();
            self.bump();
            let f = self.parse_function_tail(true, true)?;
            return Ok(self.mk_stmt(
                start,
                StmtKind::ExportDefault(Box::new(DefaultExportable::Function(Box::new(f)))),
            ));
        }
        if self.is_word("class") {
            let c = self.parse_class_tail()?;
            return Ok(self.mk_stmt(
                start,
                StmtKind::ExportDefault(Box::new(DefaultExportable::Class(Box::new(c)))),
            ));
        }
        let e = self.parse_assign_expr()?;
        self.consume_semi()?;
        Ok(self.mk_stmt(
            start,
            StmtKind::ExportDefault(Box::new(DefaultExportable::Expr(Box::new(e)))),
        ))
    }

    fn parse_export_all(&mut self, start: Span) -> PResult<Stmt> {
        self.expect_punct(Punct::Star)?;
        let as_name = if self.eat_word("as") {
            Some(self.parse_binding_ident()?)
        } else {
            None
        };
        self.expect_word("from")?;
        let source = self.expect_string_lit()?;
        self.consume_semi()?;
        Ok(self.mk_stmt(start, StmtKind::ExportAll { as_name, source }))
    }

    fn parse_export_name(&mut self) -> PResult<String> {
        self.parse_binding_ident()
    }

    fn expect_string_lit(&mut self) -> PResult<String> {
        match self.cur().kind.clone() {
            TokenKind::Str(s) => {
                self.bump();
                Ok(s)
            }
            _ => Err(self.err("文字列リテラルが必要です")),
        }
    }

    // ---------- TS declare ----------

    fn parse_declare_stmt(&mut self) -> PResult<Stmt> {
        let start = self.cur_span();
        self.expect_word("declare")?;
        if self.is_word("module") || self.is_word("namespace") || self.is_word("global") {
            self.bump();
            let name = if let TokenKind::Str(s) = self.cur().kind.clone() {
                self.bump();
                s
            } else if self.ident_name().is_some() {
                let mut n = self.parse_binding_ident()?;
                while self.eat_punct(Punct::Dot) {
                    n.push('.');
                    n.push_str(&self.parse_binding_ident()?);
                }
                n
            } else {
                String::new()
            };
            let body = self.parse_block_stmts()?;
            return Ok(self.mk_stmt(start, StmtKind::TsModule { name, body }));
        }
        // declare const/let/function/class/interface/type ... 本体はふつうに解釈し、
        // AST 的には痕跡（TsAmbientDecl）だけ残す。
        self.parse_stmt()?;
        Ok(self.mk_stmt(start, StmtKind::TsAmbientDecl))
    }
}
