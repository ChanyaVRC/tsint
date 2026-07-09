//! TS の型構文を「読み飛ばす」ためのヘルパー群。
//!
//! ここでの目標は型を正しく理解することではなく、型注釈が始まってから
//! 終わるまでのトークンを過不足なく消費し、パーサの残りの部分が正しい
//! 位置から式/文の続きを解釈できるようにすることだけ。そのため型の
//! 構造化した AST は作らない（呼び出し元は名前や span だけ拾う）。

use super::{PResult, Parser};
use crate::lexer::{Punct, TokenKind};

impl Parser {
    /// 型注釈 1 つを読み飛ばす。`skip_type` 呼び出し時点で型の先頭
    /// トークンにいることを前提とする。
    pub(super) fn skip_type(&mut self) -> PResult<()> {
        self.skip_type_conditional()
    }

    /// `T extends U ? X : Y` 形式の条件型（コーパスには現れないが安全側の対応）。
    fn skip_type_conditional(&mut self) -> PResult<()> {
        self.skip_type_union()?;
        if self.is_word("extends") {
            self.bump();
            self.skip_type_union()?;
            if self.eat_punct(Punct::Question) {
                self.skip_type_conditional()?;
                self.expect_punct(Punct::Colon)?;
                self.skip_type_conditional()?;
            }
        }
        Ok(())
    }

    fn skip_type_union(&mut self) -> PResult<()> {
        // 先頭の '|' はリーディングバー記法として許容する。
        self.eat_punct(Punct::Pipe);
        self.skip_type_intersection()?;
        while self.is_punct(Punct::Pipe) {
            self.bump();
            self.skip_type_intersection()?;
        }
        Ok(())
    }

    fn skip_type_intersection(&mut self) -> PResult<()> {
        self.eat_punct(Punct::Amp);
        self.skip_type_postfix()?;
        while self.is_punct(Punct::Amp) {
            self.bump();
            self.skip_type_postfix()?;
        }
        Ok(())
    }

    fn skip_type_postfix(&mut self) -> PResult<()> {
        self.skip_type_primary()?;
        loop {
            if self.is_punct(Punct::LBracket) {
                self.bump();
                if self.is_punct(Punct::RBracket) {
                    self.bump(); // T[]
                } else {
                    self.skip_type()?; // T[K] (indexed access)
                    self.expect_punct(Punct::RBracket)?;
                }
            } else if self.is_word("as") {
                // `T as const` 的な稀な位置。コーパスには現れないが読み飛ばす。
                break;
            } else {
                break;
            }
        }
        Ok(())
    }

    fn skip_type_primary(&mut self) -> PResult<()> {
        match &self.cur().kind {
            TokenKind::Str(_) | TokenKind::Number(_) => {
                self.bump();
                Ok(())
            }
            TokenKind::NoSubTemplate(_) => {
                self.bump();
                Ok(())
            }
            TokenKind::TemplateHead(_) => {
                // テンプレートリテラル型: `${...}` の中身も式ではなく型なので
                // ここでは単純にテンプレート断片を読みながら中の型をスキップする。
                self.bump();
                loop {
                    self.skip_type()?;
                    match &self.cur().kind {
                        TokenKind::TemplateMiddle(_) => {
                            self.bump();
                        }
                        TokenKind::TemplateTail(_) => {
                            self.bump();
                            break;
                        }
                        _ => return Err(self.err("テンプレートリテラル型が不正です")),
                    }
                }
                Ok(())
            }
            TokenKind::Punct(Punct::LBrace) => self.skip_balanced_group(),
            TokenKind::Punct(Punct::LBracket) => self.skip_balanced_group(),
            TokenKind::Punct(Punct::LParen) => self.skip_paren_type(),
            TokenKind::Ident(name) => {
                match name.as_str() {
                    "keyof" | "typeof" | "readonly" | "infer" | "unique" => {
                        self.bump();
                        self.skip_type_postfix()
                    }
                    "new" => {
                        // コンストラクタ型 `new (...) => T`
                        self.bump();
                        self.skip_type_postfix()
                    }
                    _ => {
                        self.bump();
                        // ドット区切りの修飾名 (`api.Page` など)。
                        while self.is_punct(Punct::Dot) {
                            self.bump();
                            if self.ident_name().is_some() {
                                self.bump();
                            } else {
                                break;
                            }
                        }
                        if self.is_punct(Punct::Lt) {
                            self.skip_type_args()?;
                        }
                        Ok(())
                    }
                }
            }
            _ => Err(self.err("型が必要です")),
        }
    }

    /// `(` から始まる型: 関数型 `(a: T) => U` か、丸括弧で囲まれた型。
    fn skip_paren_type(&mut self) -> PResult<()> {
        let mark = self.save();
        let ok = (|| -> PResult<()> {
            self.expect_punct(Punct::LParen)?;
            while !self.is_punct(Punct::RParen) {
                self.eat_punct(Punct::DotDotDot);
                self.skip_type_binding_target()?;
                self.eat_punct(Punct::Question);
                if self.eat_punct(Punct::Colon) {
                    self.skip_type()?;
                }
                if !self.eat_punct(Punct::Comma) {
                    break;
                }
            }
            self.expect_punct(Punct::RParen)?;
            if self.eat_punct(Punct::Arrow) {
                self.skip_type()?;
                Ok(())
            } else {
                Err(self.err("関数型ではありません"))
            }
        })();
        if ok.is_ok() {
            return Ok(());
        }
        self.restore(mark);
        self.skip_balanced_group()?;
        if self.eat_punct(Punct::Arrow) {
            self.skip_type()?;
        }
        Ok(())
    }

    /// 関数型の仮引数名部分（分割代入パターンかもしれない）を読み飛ばす。
    fn skip_type_binding_target(&mut self) -> PResult<()> {
        match &self.cur().kind {
            TokenKind::Punct(Punct::LBrace) | TokenKind::Punct(Punct::LBracket) => {
                self.skip_balanced_group()
            }
            TokenKind::Ident(_) => {
                self.bump();
                Ok(())
            }
            _ => Err(self.err("仮引数名が必要です")),
        }
    }

    /// `<T, U extends X = Y>` 形式の型パラメータ宣言（関数/クラス/interface）。
    pub(super) fn skip_type_params(&mut self) -> PResult<()> {
        if !self.is_punct(Punct::Lt) {
            return Ok(());
        }
        self.bump();
        loop {
            if self.close_angle_if_possible() {
                return Ok(());
            }
            // 稀な `const T` 修飾子。
            self.eat_word("const");
            self.parse_binding_ident()?;
            if self.is_word("extends") {
                self.bump();
                self.skip_type()?;
            }
            if self.eat_punct(Punct::Eq) {
                self.skip_type()?;
            }
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        self.close_angle_if_possible();
        Ok(())
    }

    /// `<T, U>` 形式の型引数（`new Foo<T>()` 等）。呼び出し時点で `<` を指す。
    pub(super) fn skip_type_args(&mut self) -> PResult<()> {
        self.bump(); // '<'
        if self.close_angle_if_possible() {
            return Ok(());
        }
        loop {
            self.skip_type()?;
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        if !self.close_angle_if_possible() {
            // 閉じられていない場合でも fuzz 耐性のため寛容にそのまま返す。
        }
        Ok(())
    }

    /// カレントの `>` / `>>` / `>>>` トークンから 1 段閉じる。
    /// 閉じられたら true。
    fn close_angle_if_possible(&mut self) -> bool {
        if self.pending_angle_closes > 0 {
            self.pending_angle_closes -= 1;
            return true;
        }
        match &self.cur().kind {
            TokenKind::Punct(Punct::Gt) => {
                self.bump();
                true
            }
            TokenKind::Punct(Punct::GtGt) => {
                self.bump();
                self.pending_angle_closes += 1;
                true
            }
            TokenKind::Punct(Punct::GtGtGt) => {
                self.bump();
                self.pending_angle_closes += 2;
                true
            }
            TokenKind::Punct(Punct::GtEq) => {
                // '>=' を '>' + '=' とみなして '=' を押し戻すことはできないため、
                // ここでは寛容にそのまま消費する（コーパスには現れないケース）。
                self.bump();
                true
            }
            _ => false,
        }
    }

    /// `(` `[` `{` のいずれかから始まるグループを、内部の入れ子を数えながら
    /// 対応する閉じ括弧まで丸ごと読み飛ばす。型の詳細を解釈しない汎用フォールバック。
    pub(super) fn skip_balanced_group(&mut self) -> PResult<()> {
        let mut depth: i32 = 0;
        loop {
            match &self.cur().kind {
                TokenKind::Punct(Punct::LParen)
                | TokenKind::Punct(Punct::LBrace)
                | TokenKind::Punct(Punct::LBracket) => {
                    depth += 1;
                    self.bump();
                }
                TokenKind::Punct(Punct::RParen)
                | TokenKind::Punct(Punct::RBrace)
                | TokenKind::Punct(Punct::RBracket) => {
                    depth -= 1;
                    self.bump();
                    if depth <= 0 {
                        return Ok(());
                    }
                }
                TokenKind::Eof => return Err(self.err("型定義が閉じられていません")),
                _ => {
                    self.bump();
                }
            }
        }
    }

    /// `interface Name<...> extends A, B { ... }` を読み飛ばして名前だけ返す。
    pub(super) fn parse_interface_decl(&mut self) -> PResult<String> {
        self.expect_word("interface")?;
        let name = self.parse_binding_ident()?;
        self.skip_type_params()?;
        if self.eat_word("extends") {
            loop {
                self.skip_type()?;
                if !self.eat_punct(Punct::Comma) {
                    break;
                }
            }
        }
        self.skip_balanced_group()?; // '{' ... '}'
        Ok(name)
    }

    /// `type Name<...> = ...;` を読み飛ばして名前だけ返す。
    pub(super) fn parse_type_alias_decl(&mut self) -> PResult<String> {
        self.expect_word("type")?;
        let name = self.parse_binding_ident()?;
        self.skip_type_params()?;
        self.expect_punct(Punct::Eq)?;
        self.skip_type()?;
        self.consume_semi()?;
        Ok(name)
    }
}
