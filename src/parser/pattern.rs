//! 分割代入パターン（変数宣言・関数引数・catch 節・for-of/in の左辺で共有）。

use super::{PResult, Parser};
use crate::ast::*;
use crate::lexer::Punct;

impl Parser {
    /// 束縛パターン 1 つを読む（デフォルト値は含まない。呼び出し元が
    /// 必要に応じて `= expr` を読んで `Pattern::Assign` にラップする）。
    pub(super) fn parse_binding_pattern(&mut self) -> PResult<Pattern> {
        if self.is_punct(Punct::LBrace) {
            self.parse_object_pattern()
        } else if self.is_punct(Punct::LBracket) {
            self.parse_array_pattern()
        } else {
            let name = self.parse_binding_ident()?;
            Ok(Pattern::Ident(name))
        }
    }

    /// パターン + 任意のデフォルト値。関数引数やオブジェクトパターンの
    /// 値部分など、デフォルトが付きうる位置で使う。
    pub(super) fn parse_binding_pattern_with_default(&mut self) -> PResult<Pattern> {
        let pat = self.parse_binding_pattern()?;
        if self.eat_punct(Punct::Eq) {
            let default = self.parse_assign_expr()?;
            Ok(Pattern::Assign {
                left: Box::new(pat),
                right: Box::new(default),
            })
        } else {
            Ok(pat)
        }
    }

    fn parse_object_pattern(&mut self) -> PResult<Pattern> {
        self.expect_punct(Punct::LBrace)?;
        let mut props = Vec::new();
        let mut rest = None;
        while !self.is_punct(Punct::RBrace) {
            if self.eat_punct(Punct::DotDotDot) {
                rest = Some(Box::new(self.parse_binding_pattern()?));
                break;
            }
            let key = self.parse_property_key()?;
            if self.eat_punct(Punct::Colon) {
                // { key: renamed } — renamed 側にさらにデフォルト値がありうる。
                let value = self.parse_binding_pattern_with_default()?;
                props.push(ObjectPatternProp {
                    key,
                    value,
                    shorthand: false,
                });
            } else {
                // ショートハンド { name } または { name = default }
                let name = match &key {
                    PropertyKey::Ident(s) => s.clone(),
                    _ => return Err(self.err("分割代入の省略記法には識別子が必要です")),
                };
                let value = if self.eat_punct(Punct::Eq) {
                    let default = self.parse_assign_expr()?;
                    Pattern::Assign {
                        left: Box::new(Pattern::Ident(name)),
                        right: Box::new(default),
                    }
                } else {
                    Pattern::Ident(name)
                };
                props.push(ObjectPatternProp {
                    key,
                    value,
                    shorthand: true,
                });
            }
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        self.expect_punct(Punct::RBrace)?;
        Ok(Pattern::Object { props, rest })
    }

    fn parse_array_pattern(&mut self) -> PResult<Pattern> {
        self.expect_punct(Punct::LBracket)?;
        let mut elements = Vec::new();
        let mut rest = None;
        while !self.is_punct(Punct::RBracket) {
            if self.is_punct(Punct::Comma) {
                elements.push(None); // 穴（elision）
                self.bump();
                continue;
            }
            if self.eat_punct(Punct::DotDotDot) {
                rest = Some(Box::new(self.parse_binding_pattern()?));
                break;
            }
            elements.push(Some(self.parse_binding_pattern_with_default()?));
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        self.expect_punct(Punct::RBracket)?;
        Ok(Pattern::Array { elements, rest })
    }

    /// プロパティキー（分割代入・オブジェクトリテラル共通）。
    pub(super) fn parse_property_key(&mut self) -> PResult<PropertyKey> {
        if self.eat_punct(Punct::LBracket) {
            let e = self.parse_assign_expr()?;
            self.expect_punct(Punct::RBracket)?;
            return Ok(PropertyKey::Computed(Box::new(e)));
        }
        match &self.cur().kind {
            crate::lexer::TokenKind::Str(s) => {
                let s = s.clone();
                self.bump();
                Ok(PropertyKey::Str(s))
            }
            crate::lexer::TokenKind::Number(n) => {
                let n = *n;
                self.bump();
                Ok(PropertyKey::Num(n))
            }
            crate::lexer::TokenKind::Ident(name) => {
                let name = name.clone();
                self.bump();
                Ok(PropertyKey::Ident(name))
            }
            _ => Err(self.err("プロパティ名が必要です")),
        }
    }
}
