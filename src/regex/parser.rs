//! 正規表現パターンの AST と再帰下降パーサ。

use super::charclass::{CharClass, ClassAtom};
use super::RegexError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum QuantifierMode {
    Greedy,
    Lazy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WordBoundary {
    Match,
    NonMatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LookaheadKind {
    Positive,
    Negative,
}

#[derive(Debug, Clone)]
pub(super) enum Ast {
    Empty,
    Char(char),
    Any,
    Class(CharClass),
    Concat(Vec<Ast>),
    Alt(Vec<Ast>),
    Group(usize, Box<Ast>),
    NonCap(Box<Ast>),
    Repeat(Box<Ast>, u32, Option<u32>, QuantifierMode),
    Start,
    End,
    WordB(WordBoundary),
    Backref(usize),
    Look(Box<Ast>, LookaheadKind),
}

enum ClassEscape {
    Char(char),
    Atom(ClassAtom),
}

pub(super) struct Parser {
    chars: Vec<char>,
    pos: usize,
    pub(super) ncaptures: usize,
    total_captures: usize,
}

/// パターン全体を前もって走査し、キャプチャグループの総数を数える
/// （前方参照される後方参照 `\1` の妥当性チェックに使う）。
/// 名前付きキャプチャ・後読みは本実装では未対応（明確なエラーになる）ため、
/// ここでは「非キャプチャ考慮なしの `(` を全部数える」簡易実装で十分。
pub(super) fn count_capturing_groups(chars: &[char]) -> usize {
    let mut i = 0;
    let mut count = 0;
    let mut in_class = false;
    while i < chars.len() {
        match chars[i] {
            '\\' => {
                i += 2;
                continue;
            }
            '[' if !in_class => in_class = true,
            ']' if in_class => in_class = false,
            '(' if !in_class && chars.get(i + 1) != Some(&'?') => {
                count += 1;
            }
            _ => {}
        }
        i += 1;
    }
    count
}

impl Parser {
    pub(super) fn new(chars: Vec<char>, total_captures: usize) -> Self {
        Parser {
            chars,
            pos: 0,
            ncaptures: 0,
            total_captures,
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }
    fn peek_at(&self, offset: usize) -> Option<char> {
        self.chars.get(self.pos + offset).copied()
    }
    fn advance(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }
    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn expect(&mut self, c: char) -> Result<(), RegexError> {
        if self.eat(c) {
            Ok(())
        } else {
            Err(RegexError(format!(
                "正規表現の構文エラー: '{c}' が期待されましたが見つかりません"
            )))
        }
    }

    pub(super) fn parse_pattern(&mut self) -> Result<Ast, RegexError> {
        let a = self.parse_alt()?;
        if self.pos != self.chars.len() {
            return Err(RegexError(format!(
                "正規表現の構文エラー: 予期しない文字 '{}'（位置 {}）",
                self.chars[self.pos], self.pos
            )));
        }
        Ok(a)
    }

    fn parse_alt(&mut self) -> Result<Ast, RegexError> {
        let mut branches = vec![self.parse_concat()?];
        while self.eat('|') {
            branches.push(self.parse_concat()?);
        }
        if branches.len() == 1 {
            Ok(branches.pop().unwrap())
        } else {
            Ok(Ast::Alt(branches))
        }
    }

    fn parse_concat(&mut self) -> Result<Ast, RegexError> {
        let mut items = vec![];
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            items.push(self.parse_repeat()?);
        }
        if items.is_empty() {
            Ok(Ast::Empty)
        } else if items.len() == 1 {
            Ok(items.pop().unwrap())
        } else {
            Ok(Ast::Concat(items))
        }
    }

    fn parse_repeat(&mut self) -> Result<Ast, RegexError> {
        let atom = self.parse_atom()?;
        if let Some((min, max)) = self.try_parse_quantifier_bounds()? {
            let mode = if self.eat('?') {
                QuantifierMode::Lazy
            } else {
                QuantifierMode::Greedy
            };
            Ok(Ast::Repeat(Box::new(atom), min, max, mode))
        } else {
            Ok(atom)
        }
    }

    fn try_parse_quantifier_bounds(&mut self) -> Result<Option<(u32, Option<u32>)>, RegexError> {
        match self.peek() {
            Some('*') => {
                self.pos += 1;
                Ok(Some((0, None)))
            }
            Some('+') => {
                self.pos += 1;
                Ok(Some((1, None)))
            }
            Some('?') => {
                self.pos += 1;
                Ok(Some((0, Some(1))))
            }
            Some('{') => {
                let save = self.pos;
                self.pos += 1;
                if let Some((min, max)) = self.try_parse_brace_body() {
                    if self.eat('}') {
                        if let Some(m) = max {
                            if m < min {
                                return Err(RegexError(
                                    "量指定子の範囲が不正です（最小値が最大値を超えています）"
                                        .to_string(),
                                ));
                            }
                        }
                        return Ok(Some((min, max)));
                    }
                }
                // `{` が量指定子として解釈できない場合、JS ではリテラル文字として扱う。
                self.pos = save;
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    fn try_parse_brace_body(&mut self) -> Option<(u32, Option<u32>)> {
        let start = self.pos;
        let min = self.read_u32_digits()?;
        if self.eat(',') {
            let max = self.read_u32_digits();
            // `,` の後に数字が 1 桁もなければ `{min,}`、あれば `{min,max}`。
            if max.is_none() && matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                // オーバーフローした場合
                self.pos = start;
                return None;
            }
            Some((min, max))
        } else {
            Some((min, Some(min)))
        }
    }

    fn read_u32_digits(&mut self) -> Option<u32> {
        let start = self.pos;
        let mut val: u32 = 0;
        let mut any = false;
        while let Some(c) = self.peek() {
            if let Some(d) = c.to_digit(10) {
                val = val.checked_mul(10)?.checked_add(d)?;
                any = true;
                self.pos += 1;
            } else {
                break;
            }
        }
        if any {
            Some(val)
        } else {
            self.pos = start;
            None
        }
    }

    fn parse_atom(&mut self) -> Result<Ast, RegexError> {
        match self.peek() {
            None => Err(RegexError("パターンの終端で構文エラーです".to_string())),
            Some('*') | Some('+') | Some('?') => Err(RegexError(
                "繰り返し演算子の対象がありません（'Nothing to repeat'）".to_string(),
            )),
            Some('^') => {
                self.pos += 1;
                Ok(Ast::Start)
            }
            Some('$') => {
                self.pos += 1;
                Ok(Ast::End)
            }
            Some('.') => {
                self.pos += 1;
                Ok(Ast::Any)
            }
            Some('(') => self.parse_group(),
            Some('[') => self.parse_class(),
            Some('\\') => self.parse_escape(),
            Some(')') => Err(RegexError("対応する '(' が無い ')' です".to_string())),
            Some(c) => {
                self.pos += 1;
                Ok(Ast::Char(c))
            }
        }
    }

    fn parse_group(&mut self) -> Result<Ast, RegexError> {
        self.pos += 1; // '('
        let wrap: Option<fn(Box<Ast>) -> Ast> = if self.eat('?') {
            if self.eat(':') {
                Some(Ast::NonCap)
            } else if self.eat('=') {
                Some(|a| Ast::Look(a, LookaheadKind::Positive))
            } else if self.eat('!') {
                Some(|a| Ast::Look(a, LookaheadKind::Negative))
            } else if self.peek() == Some('<') {
                if matches!(self.peek_at(1), Some('=' | '!')) {
                    return Err(RegexError(
                        "後読み (lookbehind, (?<=...) / (?<!...)) は tsint の正規表現エンジンでは未対応です"
                            .to_string(),
                    ));
                }
                return Err(RegexError(
                    "名前付きキャプチャグループ (?<name>...) は未対応です".to_string(),
                ));
            } else {
                return Err(RegexError(format!(
                    "未対応のグループ構文です（'(?{}'）",
                    self.peek().unwrap_or(' ')
                )));
            }
        } else {
            None
        };
        let cap_idx = if wrap.is_none() {
            self.ncaptures += 1;
            self.ncaptures
        } else {
            0
        };
        let inner = Box::new(self.parse_alt()?);
        self.expect(')')?;
        Ok(match wrap {
            Some(f) => f(inner),
            None => Ast::Group(cap_idx, inner),
        })
    }

    fn parse_hex_escape(&mut self, n: usize) -> Result<char, RegexError> {
        let mut code: u32 = 0;
        for _ in 0..n {
            match self.advance().and_then(|c| c.to_digit(16)) {
                Some(d) => code = (code << 4) | d,
                None => return Err(RegexError("不正な16進エスケープです".to_string())),
            }
        }
        char::from_u32(code).ok_or_else(|| RegexError("不正なコードポイントです".to_string()))
    }

    fn parse_unicode_escape(&mut self) -> Result<char, RegexError> {
        if self.eat('{') {
            let mut s = String::new();
            while let Some(c) = self.peek() {
                if c == '}' {
                    break;
                }
                s.push(c);
                self.pos += 1;
            }
            self.expect('}')?;
            let code = u32::from_str_radix(&s, 16)
                .map_err(|_| RegexError("不正な \\u{...} エスケープです".to_string()))?;
            char::from_u32(code).ok_or_else(|| RegexError("不正なコードポイントです".to_string()))
        } else {
            self.parse_hex_escape(4)
        }
    }

    /// パターン本体と文字クラス内で共通するエスケープ（ショートハンド・制御文字・`\x`/`\u`）を解釈する。
    fn parse_common_escape(&mut self, c: char) -> Result<Option<ClassEscape>, RegexError> {
        Ok(Some(match c {
            'd' => ClassEscape::Atom(ClassAtom::Digit),
            'D' => ClassEscape::Atom(ClassAtom::NotDigit),
            'w' => ClassEscape::Atom(ClassAtom::Word),
            'W' => ClassEscape::Atom(ClassAtom::NotWord),
            's' => ClassEscape::Atom(ClassAtom::Space),
            'S' => ClassEscape::Atom(ClassAtom::NotSpace),
            'n' => ClassEscape::Char('\n'),
            'r' => ClassEscape::Char('\r'),
            't' => ClassEscape::Char('\t'),
            'v' => ClassEscape::Char('\u{000B}'),
            'f' => ClassEscape::Char('\u{000C}'),
            'x' => ClassEscape::Char(self.parse_hex_escape(2)?),
            'u' => ClassEscape::Char(self.parse_unicode_escape()?),
            _ => return Ok(None),
        }))
    }

    fn parse_escape(&mut self) -> Result<Ast, RegexError> {
        self.pos += 1; // '\'
        let c = self
            .advance()
            .ok_or_else(|| RegexError("パターンがバックスラッシュで終わっています".to_string()))?;
        if let Some(esc) = self.parse_common_escape(c)? {
            return Ok(match esc {
                ClassEscape::Atom(a) => Ast::Class(CharClass::new(false, vec![a])),
                ClassEscape::Char(ch) => Ast::Char(ch),
            });
        }
        Ok(match c {
            'b' => Ast::WordB(WordBoundary::Match),
            'B' => Ast::WordB(WordBoundary::NonMatch),
            'k' => {
                return Err(RegexError(
                    "名前付き後方参照 \\k<...> は未対応です".to_string(),
                ))
            }
            'p' | 'P' => {
                return Err(RegexError(
                    "Unicode プロパティエスケープ \\p{...} / \\P{...} は未対応です".to_string(),
                ))
            }
            '0' if !matches!(self.peek(), Some(d) if d.is_ascii_digit()) => Ast::Char('\0'),
            c if c.is_ascii_digit() => {
                let mut n: usize = (c as u8 - b'0') as usize;
                while let Some(d) = self.peek() {
                    if d.is_ascii_digit() {
                        n = n
                            .saturating_mul(10)
                            .saturating_add((d as u8 - b'0') as usize);
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                if n == 0 || n > self.total_captures {
                    return Err(RegexError(format!(
                        "無効な後方参照 \\{n} です（キャプチャグループが {} 個しかありません）",
                        self.total_captures
                    )));
                }
                Ast::Backref(n)
            }
            // それ以外のエスケープ（`\.` `\\` `\/` `\(` 等の記号や、未知の
            // アルファベットエスケープ）は寛容にその文字自身として扱う。
            other => Ast::Char(other),
        })
    }

    fn parse_class_escape(&mut self) -> Result<ClassEscape, RegexError> {
        self.pos += 1; // '\'
        let c = self
            .advance()
            .ok_or_else(|| RegexError("パターンがバックスラッシュで終わっています".to_string()))?;
        if let Some(esc) = self.parse_common_escape(c)? {
            return Ok(esc);
        }
        Ok(match c {
            'b' => ClassEscape::Char('\u{0008}'), // クラス内の \b はバックスペース
            '0' => ClassEscape::Char('\0'),
            other => ClassEscape::Char(other),
        })
    }

    fn parse_class(&mut self) -> Result<Ast, RegexError> {
        self.pos += 1; // '['
        let negate = self.eat('^');
        let mut atoms = vec![];
        loop {
            match self.peek() {
                None => {
                    return Err(RegexError(
                        "文字クラス '[' が閉じられていません".to_string(),
                    ))
                }
                Some(']') => {
                    self.pos += 1;
                    break;
                }
                Some('\\') => match self.parse_class_escape()? {
                    ClassEscape::Atom(a) => atoms.push(a),
                    ClassEscape::Char(start) => atoms.push(self.maybe_class_range(start)?),
                },
                Some(c) => {
                    self.pos += 1;
                    atoms.push(self.maybe_class_range(c)?);
                }
            }
        }
        Ok(Ast::Class(CharClass::new(negate, atoms)))
    }

    /// `start` の直後が `-<終端>` の形なら範囲として、そうでなければ単独の
    /// 文字としてクラス要素を作る。
    fn maybe_class_range(&mut self, start: char) -> Result<ClassAtom, RegexError> {
        if self.peek() == Some('-') && self.peek_at(1).is_some() && self.peek_at(1) != Some(']') {
            self.pos += 1; // '-'
            let end = match self.peek() {
                Some('\\') => match self.parse_class_escape()? {
                    ClassEscape::Char(c) => c,
                    ClassEscape::Atom(_) => {
                        return Err(RegexError(
                            "文字クラスの範囲指定にショートハンド（\\d 等）は使えません"
                                .to_string(),
                        ))
                    }
                },
                Some(c) => {
                    self.pos += 1;
                    c
                }
                None => return Err(RegexError("文字クラスの範囲指定が不正です".to_string())),
            };
            if end < start {
                return Err(RegexError("文字クラスの範囲が逆転しています".to_string()));
            }
            Ok(ClassAtom::Range(start, end))
        } else {
            Ok(ClassAtom::Char(start))
        }
    }
}
