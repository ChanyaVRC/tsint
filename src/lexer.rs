//! レキサ（トークナイザ）。
//!
//! 主な難所:
//! - `/` の曖昧性: 除算演算子と正規表現リテラルの先頭は同じ文字。直前の
//!   「意味のある」トークンの種類から判定する（標準的なヒューリスティック）。
//! - テンプレートリテラルのネスト: `` `a${ `b${c}` }` `` のように `${...}` の中に
//!   さらにテンプレートやオブジェクトリテラル `{}` が現れてよい。ブレースの
//!   スタック（種類: 通常の `{}` かテンプレート代入の `${...}` か）を持つことで
//!   1 パスのトークナイズで正しく処理する。
//! - ASI（自動セミコロン挿入）: 各トークンに「直前に改行を挟んだか」フラグを
//!   付与し、パーサ側で判定できるようにする。

use crate::ast::{Pos, Span};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegexToken {
    pub pattern: String,
    pub flags: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    Ident(String),
    Number(f64),
    Str(String),
    /// バッククォート単体で完結する（`${` を含まない）テンプレート文字列。
    NoSubTemplate(String),
    /// `` `...${ `` — 先頭断片。
    TemplateHead(String),
    /// `` }...${ `` — 中間断片。
    TemplateMiddle(String),
    /// `` }...` `` — 末尾断片。
    TemplateTail(String),
    Regex(Box<RegexToken>),
    Punct(Punct),
    Eof,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Punct {
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Semi,
    Comma,
    Dot,
    DotDotDot,
    Colon,
    Question,
    QuestionDot,
    QuestionQuestion,
    QuestionQuestionEq,
    Arrow,
    Plus,
    Minus,
    Star,
    StarStar,
    Slash,
    Percent,
    Eq,
    EqEq,
    EqEqEq,
    NotEq,
    NotEqEq,
    Lt,
    Gt,
    LtEq,
    GtEq,
    LtLt,
    GtGt,
    GtGtGt,
    Amp,
    Pipe,
    Caret,
    Tilde,
    Bang,
    AmpAmp,
    PipePipe,
    PlusPlus,
    MinusMinus,
    PlusEq,
    MinusEq,
    StarEq,
    SlashEq,
    PercentEq,
    StarStarEq,
    LtLtEq,
    GtGtEq,
    GtGtGtEq,
    AmpEq,
    PipeEq,
    CaretEq,
    AmpAmpEq,
    PipePipeEq,
    At,
    Hash,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
    /// このトークンより前（直前トークンの終わりからこのトークンの始まりまで）
    /// に改行が 1 つ以上含まれていたか。ASI 判定に使う。
    pub newline_before: bool,
}

#[derive(Debug, Clone)]
pub struct LexError {
    pub message: String,
    pub pos: Pos,
}

impl std::fmt::Display for LexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "字句エラー {}:{}: {}",
            self.pos.line, self.pos.col, self.message
        )
    }
}

impl std::error::Error for LexError {}

/// テンプレートリテラルの `${` によって開かれたブレース文脈か、
/// 通常の `{` かを区別するためのスタック要素。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BraceCtx {
    Block,
    Template,
}

/// テンプレートリテラル断片の開始位置。
#[derive(Clone, Copy, PartialEq, Eq)]
enum TemplatePartPos {
    /// 先頭バッククォート直後（`NoSubTemplate` / `TemplateHead`）。
    Head,
    /// `${...}` を閉じた `}` の直後（`TemplateTail` / `TemplateMiddle`）。
    Continuation,
}

/// `f64` で厳密に表現できる 10 進整数桁数の高速パス上限。
const FAST_INT_MAX_DIGITS: usize = 15;

/// 正規表現リテラルが許されるかどうかを、直前の意味のあるトークンから判定する。
/// 「識別子・数値・文字列・`)` `]` `++` `--` の直後」は除算、それ以外は正規表現。
#[inline]
fn regex_allowed_after(kind: &TokenKind) -> bool {
    match kind {
        TokenKind::Ident(name) => {
            // ほとんどの識別子の後ろは除算だが、キーワードの後ろは正規表現。
            matches!(
                name.as_str(),
                "return"
                    | "typeof"
                    | "instanceof"
                    | "in"
                    | "of"
                    | "new"
                    | "delete"
                    | "void"
                    | "throw"
                    | "case"
                    | "do"
                    | "else"
                    | "yield"
                    | "await"
                    | "extends"
                    | "default"
            )
        }
        TokenKind::Number(_)
        | TokenKind::Str(_)
        | TokenKind::NoSubTemplate(_)
        | TokenKind::TemplateTail(_)
        | TokenKind::Regex(_) => false,
        TokenKind::Punct(p) => !matches!(
            p,
            Punct::RParen | Punct::RBracket | Punct::PlusPlus | Punct::MinusMinus
        ),
        _ => true,
    }
}

#[inline]
fn is_ascii_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b == b'$'
}

#[inline]
fn is_ascii_ident_part(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

pub struct Lexer<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize, // UTF-8 バイトオフセット（常に文字境界）
    line: u32,
    col: u32, // 1 始まりの文字（Unicode スカラ値）列番号
    brace_stack: Vec<BraceCtx>,
    regex_allowed: bool,
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str) -> Self {
        Lexer {
            src,
            bytes: src.as_bytes(),
            pos: 0,
            line: 1,
            col: 1,
            brace_stack: Vec::new(),
            regex_allowed: true,
        }
    }

    #[inline]
    fn cur_pos(&self) -> Pos {
        Pos {
            line: self.line,
            col: self.col,
            byte: self.pos as u32,
        }
    }

    #[inline]
    fn peek_byte(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    #[inline]
    fn peek_byte_at(&self, offset: usize) -> Option<u8> {
        self.bytes.get(self.pos + offset).copied()
    }

    #[inline]
    fn peek_char(&self) -> Option<char> {
        let b = *self.bytes.get(self.pos)?;
        if b < 0x80 {
            Some(b as char)
        } else {
            self.src[self.pos..].chars().next()
        }
    }

    #[inline]
    fn advance_ascii(&mut self) -> u8 {
        let b = self.bytes[self.pos];
        self.pos += 1;
        if b == b'\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        b
    }

    #[inline]
    fn advance_char(&mut self) -> Option<char> {
        let b = *self.bytes.get(self.pos)?;
        if b < 0x80 {
            self.pos += 1;
            if b == b'\n' {
                self.line += 1;
                self.col = 1;
            } else {
                self.col += 1;
            }
            Some(b as char)
        } else {
            let c = self.src[self.pos..].chars().next()?;
            self.pos += c.len_utf8();
            self.col += 1;
            Some(c)
        }
    }

    #[inline]
    fn eat_byte(&mut self, expected: u8) -> bool {
        if self.peek_byte() == Some(expected) {
            self.advance_ascii();
            true
        } else {
            false
        }
    }

    fn err(&self, message: impl Into<String>) -> LexError {
        LexError {
            message: message.into(),
            pos: self.cur_pos(),
        }
    }

    /// トークン列全体を生成する。
    pub fn tokenize(src: &'a str) -> Result<Vec<Token>, LexError> {
        let mut lexer = Lexer::new(src);
        let mut out = Vec::with_capacity((src.len() / 5).max(16));
        loop {
            let tok = lexer.next_token()?;
            let is_eof = matches!(tok.kind, TokenKind::Eof);
            lexer.regex_allowed = regex_allowed_after(&tok.kind);
            out.push(tok);
            if is_eof {
                break;
            }
        }
        Ok(out)
    }

    /// 現在位置（非 EOF かつ UTF-8 文字境界）の 1 文字を読み取る。
    #[inline]
    fn cur_non_ascii_char(&self) -> char {
        self.src[self.pos..]
            .chars()
            .next()
            .expect("pos は常に有効な UTF-8 境界を指す")
    }

    /// 改行・ASCII・非 ASCII の 1 文字分だけカーソルを進める。
    #[inline]
    fn bump_byte_or_utf8(&mut self, b: u8) -> bool {
        if b == b'\n' {
            self.pos += 1;
            self.line += 1;
            self.col = 1;
            true
        } else if b < 0x80 {
            self.pos += 1;
            self.col += 1;
            false
        } else {
            let c = self.cur_non_ascii_char();
            self.pos += c.len_utf8();
            self.col += 1;
            false
        }
    }

    /// 空白・改行・コメントを読み飛ばし、改行が含まれていたかを返す。
    fn skip_trivia(&mut self) -> bool {
        let mut saw_newline = false;
        let bytes = self.bytes;
        while let Some(&b) = bytes.get(self.pos) {
            match b {
                b'\n' => {
                    saw_newline = true;
                    self.pos += 1;
                    self.line += 1;
                    self.col = 1;
                }
                b' ' | b'\t' | b'\r' | 0x0b | 0x0c => {
                    self.pos += 1;
                    self.col += 1;
                }
                b'/' => match bytes.get(self.pos + 1).copied() {
                    Some(b'/') => {
                        self.pos += 2;
                        self.col += 2;
                        while let Some(&c) = bytes.get(self.pos) {
                            if c == b'\n' {
                                break;
                            }
                            self.bump_byte_or_utf8(c);
                        }
                    }
                    Some(b'*') => {
                        self.pos += 2;
                        self.col += 2;
                        while let Some(&c) = bytes.get(self.pos) {
                            if c == b'*' && bytes.get(self.pos + 1) == Some(&b'/') {
                                self.pos += 2;
                                self.col += 2;
                                break;
                            }
                            if self.bump_byte_or_utf8(c) {
                                saw_newline = true;
                            }
                        }
                    }
                    _ => break,
                },
                0x80..=0xff => {
                    let ch = self.cur_non_ascii_char();
                    if ch.is_whitespace() {
                        self.pos += ch.len_utf8();
                        self.col += 1;
                    } else {
                        break;
                    }
                }
                _ => break,
            }
        }
        saw_newline
    }

    fn next_token(&mut self) -> Result<Token, LexError> {
        let newline_before = self.skip_trivia();
        let start = self.cur_pos();

        let Some(b) = self.peek_byte() else {
            return Ok(Token {
                kind: TokenKind::Eof,
                span: Span::new(start, start),
                newline_before,
            });
        };

        let kind = if b < 0x80 {
            if b == b'`' {
                self.advance_ascii();
                self.read_template_part(TemplatePartPos::Head)?
            } else if b == b'}' && self.brace_stack.last() == Some(&BraceCtx::Template) {
                self.brace_stack.pop();
                self.advance_ascii();
                self.read_template_part(TemplatePartPos::Continuation)?
            } else if b.is_ascii_digit()
                || (b == b'.' && self.peek_byte_at(1).is_some_and(|d| d.is_ascii_digit()))
            {
                self.read_number()?
            } else if b == b'"' || b == b'\'' {
                self.read_string(b)?
            } else if b == b'/' && self.regex_allowed {
                self.read_regex()?
            } else if is_ascii_ident_start(b) {
                self.read_ident()
            } else {
                self.read_punct()?
            }
        } else {
            let c = self.cur_non_ascii_char();
            if is_ident_start(c) {
                self.read_ident()
            } else {
                return Err(self.err(format!("不正な文字です: {c:?}")));
            }
        };

        let end = self.cur_pos();
        Ok(Token {
            kind,
            span: Span::new(start, end),
            newline_before,
        })
    }

    fn read_ident(&mut self) -> TokenKind {
        let start = self.pos;
        let bytes = self.bytes;
        while let Some(&b) = bytes.get(self.pos) {
            if b < 0x80 {
                if is_ascii_ident_part(b) {
                    self.pos += 1;
                    self.col += 1;
                } else {
                    break;
                }
            } else {
                let c = self.cur_non_ascii_char();
                if is_ident_part(c) {
                    self.pos += c.len_utf8();
                    self.col += 1;
                } else {
                    break;
                }
            }
        }
        TokenKind::Ident(self.src[start..self.pos].to_owned())
    }

    fn read_number(&mut self) -> Result<TokenKind, LexError> {
        if self.peek_byte() == Some(b'0')
            && matches!(
                self.peek_byte_at(1),
                Some(b'x' | b'X' | b'o' | b'O' | b'b' | b'B')
            )
        {
            self.advance_ascii(); // '0'
            let radix_byte = self.advance_ascii();
            let radix = match radix_byte.to_ascii_lowercase() {
                b'x' => 16u32,
                b'o' => 8u32,
                b'b' => 2u32,
                _ => unreachable!(),
            };
            let dig_start = self.pos;
            let mut has_sep = false;
            while let Some(b) = self.peek_byte() {
                if b == b'_' {
                    has_sep = true;
                    self.advance_ascii();
                } else if (b as char).is_digit(radix) {
                    self.advance_ascii();
                } else {
                    break;
                }
            }
            let dig_end = self.pos;
            // BigInt サフィックス 'n' は読み飛ばす（値としては通常数値扱い）。
            self.eat_byte(b'n');
            let v = if !has_sep {
                i64::from_str_radix(&self.src[dig_start..dig_end], radix).unwrap_or(0) as f64
            } else {
                let digits: String = self.src[dig_start..dig_end]
                    .chars()
                    .filter(|&c| c != '_')
                    .collect();
                i64::from_str_radix(&digits, radix).unwrap_or(0) as f64
            };
            return Ok(TokenKind::Number(v));
        }

        let start = self.pos;
        let mut has_sep = false;
        let mut is_float = false;
        let mut int_acc: i64 = 0;
        let mut int_digits = 0usize;

        while let Some(b) = self.peek_byte() {
            if b.is_ascii_digit() {
                if int_digits < FAST_INT_MAX_DIGITS {
                    int_acc = int_acc * 10 + (b - b'0') as i64;
                }
                int_digits += 1;
                self.advance_ascii();
            } else if b == b'_' {
                has_sep = true;
                self.advance_ascii();
            } else {
                break;
            }
        }
        if self.peek_byte() == Some(b'.') {
            is_float = true;
            self.advance_ascii();
            while let Some(b) = self.peek_byte() {
                if b.is_ascii_digit() {
                    self.advance_ascii();
                } else if b == b'_' {
                    has_sep = true;
                    self.advance_ascii();
                } else {
                    break;
                }
            }
        }
        if matches!(self.peek_byte(), Some(b'e' | b'E')) {
            is_float = true;
            self.advance_ascii();
            if matches!(self.peek_byte(), Some(b'+' | b'-')) {
                self.advance_ascii();
            }
            while let Some(b) = self.peek_byte() {
                if b.is_ascii_digit() {
                    self.advance_ascii();
                } else {
                    break;
                }
            }
        }
        let end = self.pos;
        self.eat_byte(b'n'); // BigInt サフィックス

        if !is_float && int_digits > 0 && int_digits <= FAST_INT_MAX_DIGITS {
            return Ok(TokenKind::Number(int_acc as f64));
        }
        let v: f64 = if !has_sep {
            self.src[start..end]
                .parse()
                .map_err(|_| self.err(format!("不正な数値リテラル: {}", &self.src[start..end])))?
        } else {
            let cleaned: String = self.src[start..end].chars().filter(|&c| c != '_').collect();
            cleaned
                .parse()
                .map_err(|_| self.err(format!("不正な数値リテラル: {cleaned}")))?
        };
        Ok(TokenKind::Number(v))
    }

    fn read_string(&mut self, quote: u8) -> Result<TokenKind, LexError> {
        self.advance_ascii(); // 開き quote
        let start = self.pos;
        let bytes = self.bytes;
        // エスケープ無し文字列のゼロコピー高速パス
        while let Some(&b) = bytes.get(self.pos) {
            if b == quote {
                let s = self.src[start..self.pos].to_owned();
                self.pos += 1;
                self.col += 1;
                return Ok(TokenKind::Str(s));
            }
            if b == b'\\' {
                break;
            }
            self.bump_byte_or_utf8(b);
        }
        if self.pos >= bytes.len() {
            return Err(self.err("文字列リテラルが閉じられていません"));
        }
        let mut s = String::with_capacity(self.pos - start + 16);
        s.push_str(&self.src[start..self.pos]);
        loop {
            match self.peek_byte() {
                None => return Err(self.err("文字列リテラルが閉じられていません")),
                Some(b) if b == quote => {
                    self.advance_ascii();
                    break;
                }
                Some(b'\\') => {
                    self.advance_ascii();
                    self.read_escape(&mut s)?;
                }
                Some(_) => {
                    let chunk_start = self.pos;
                    while let Some(&b) = bytes.get(self.pos) {
                        if b == quote || b == b'\\' {
                            break;
                        }
                        self.bump_byte_or_utf8(b);
                    }
                    s.push_str(&self.src[chunk_start..self.pos]);
                }
            }
        }
        Ok(TokenKind::Str(s))
    }

    fn read_fixed_hex_escape(&mut self, digits: usize, out: &mut String) {
        let mut code = 0u32;
        let mut ok = true;
        for _ in 0..digits {
            if let Some(h) = self.peek_char() {
                self.advance_char();
                if let Some(d) = h.to_digit(16) {
                    code = (code << 4) | d;
                } else {
                    ok = false;
                }
            } else {
                ok = false;
            }
        }
        if ok {
            if let Some(ch) = char::from_u32(code) {
                out.push(ch);
            }
        }
    }

    fn read_escape(&mut self, out: &mut String) -> Result<(), LexError> {
        let Some(c) = self.advance_char() else {
            return Err(self.err("不正なエスケープシーケンス"));
        };
        match c {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            'b' => out.push('\u{8}'),
            'f' => out.push('\u{c}'),
            'v' => out.push('\u{b}'),
            '0' => out.push('\0'),
            '\n' => {} // 行継続
            '\r' => {
                self.eat_byte(b'\n');
            }
            'x' => self.read_fixed_hex_escape(2, out),
            'u' => {
                if self.peek_byte() == Some(b'{') {
                    self.advance_ascii();
                    let mut code = 0u32;
                    let mut any = false;
                    let mut ok = true;
                    while let Some(h) = self.peek_char() {
                        if h == '}' {
                            break;
                        }
                        self.advance_char();
                        any = true;
                        match h
                            .to_digit(16)
                            .and_then(|d| code.checked_mul(16).and_then(|v| v.checked_add(d)))
                        {
                            Some(next) => code = next,
                            None => ok = false,
                        }
                    }
                    self.eat_byte(b'}');
                    if any && ok {
                        if let Some(ch) = char::from_u32(code) {
                            out.push(ch);
                        }
                    }
                } else {
                    self.read_fixed_hex_escape(4, out);
                }
            }
            other => out.push(other),
        }
        Ok(())
    }

    /// テンプレートリテラルの断片を 1 つ読む。
    fn read_template_part(&mut self, part_pos: TemplatePartPos) -> Result<TokenKind, LexError> {
        let start = self.pos;
        let bytes = self.bytes;
        let mut s: Option<String> = None;
        let mut chunk_start = start;
        let finish = |buf: Option<String>, tail: &str| match buf {
            Some(mut b) => {
                b.push_str(tail);
                b
            }
            None => tail.to_owned(),
        };
        loop {
            match bytes.get(self.pos).copied() {
                None => return Err(self.err("テンプレートリテラルが閉じられていません")),
                Some(b'`') => {
                    let result = finish(s, &self.src[chunk_start..self.pos]);
                    self.advance_ascii();
                    return Ok(match part_pos {
                        TemplatePartPos::Head => TokenKind::NoSubTemplate(result),
                        TemplatePartPos::Continuation => TokenKind::TemplateTail(result),
                    });
                }
                Some(b'$') if bytes.get(self.pos + 1) == Some(&b'{') => {
                    let result = finish(s, &self.src[chunk_start..self.pos]);
                    self.pos += 2;
                    self.col += 2;
                    self.brace_stack.push(BraceCtx::Template);
                    return Ok(match part_pos {
                        TemplatePartPos::Head => TokenKind::TemplateHead(result),
                        TemplatePartPos::Continuation => TokenKind::TemplateMiddle(result),
                    });
                }
                Some(b'\\') => {
                    let buf = s.get_or_insert_with(|| String::with_capacity(self.pos - start + 16));
                    buf.push_str(&self.src[chunk_start..self.pos]);
                    self.advance_ascii();
                    self.read_escape(buf)?;
                    chunk_start = self.pos;
                }
                Some(b) => {
                    self.bump_byte_or_utf8(b);
                }
            }
        }
    }

    fn read_regex(&mut self) -> Result<TokenKind, LexError> {
        self.advance_ascii(); // 先頭の '/'
        let pat_start = self.pos;
        let mut in_class = false;
        loop {
            match self.peek_byte() {
                None => return Err(self.err("正規表現リテラルが閉じられていません")),
                Some(b'\n') => return Err(self.err("正規表現リテラル中に改行があります")),
                Some(b'\\') => {
                    self.advance_ascii();
                    if self.peek_byte().is_some() {
                        self.advance_char();
                    }
                }
                Some(b'[') => {
                    in_class = true;
                    self.advance_ascii();
                }
                Some(b']') if in_class => {
                    in_class = false;
                    self.advance_ascii();
                }
                Some(b'/') if !in_class => {
                    let pat_end = self.pos;
                    self.advance_ascii();
                    let flags_start = self.pos;
                    while let Some(c) = self.peek_char() {
                        if c.is_alphabetic() {
                            self.advance_char();
                        } else {
                            break;
                        }
                    }
                    return Ok(TokenKind::Regex(Box::new(RegexToken {
                        pattern: self.src[pat_start..pat_end].to_owned(),
                        flags: self.src[flags_start..self.pos].to_owned(),
                    })));
                }
                Some(_) => {
                    self.advance_char();
                }
            }
        }
    }

    #[inline]
    fn eq_or(&mut self, with_eq: Punct, plain: Punct) -> Punct {
        if self.eat_byte(b'=') {
            with_eq
        } else {
            plain
        }
    }

    #[inline]
    fn double_or_eq(
        &mut self,
        b: u8,
        double_eq: Punct,
        double: Punct,
        single_eq: Punct,
        single: Punct,
    ) -> Punct {
        if self.eat_byte(b) {
            self.eq_or(double_eq, double)
        } else {
            self.eq_or(single_eq, single)
        }
    }

    fn read_punct(&mut self) -> Result<TokenKind, LexError> {
        let b = self.advance_ascii();
        use Punct::*;
        let p = match b {
            b'(' => LParen,
            b')' => RParen,
            b'{' => {
                self.brace_stack.push(BraceCtx::Block);
                LBrace
            }
            b'}' => {
                // Template 文脈でないただの '}'。ネストが崩れていても best-effort で pop。
                if self.brace_stack.last() == Some(&BraceCtx::Block) {
                    self.brace_stack.pop();
                }
                RBrace
            }
            b'[' => LBracket,
            b']' => RBracket,
            b';' => Semi,
            b',' => Comma,
            b'~' => Tilde,
            b'@' => At,
            b'#' => Hash,
            b':' => Colon,
            b'.' => {
                if self.peek_byte() == Some(b'.') && self.peek_byte_at(1) == Some(b'.') {
                    self.advance_ascii();
                    self.advance_ascii();
                    DotDotDot
                } else {
                    Dot
                }
            }
            b'?' => {
                if self.peek_byte() == Some(b'.')
                    && !self.peek_byte_at(1).is_some_and(|d| d.is_ascii_digit())
                {
                    self.advance_ascii();
                    QuestionDot
                } else if self.eat_byte(b'?') {
                    self.eq_or(QuestionQuestionEq, QuestionQuestion)
                } else {
                    Question
                }
            }
            b'+' => {
                if self.eat_byte(b'+') {
                    PlusPlus
                } else {
                    self.eq_or(PlusEq, Plus)
                }
            }
            b'-' => {
                if self.eat_byte(b'-') {
                    MinusMinus
                } else {
                    self.eq_or(MinusEq, Minus)
                }
            }
            b'*' => self.double_or_eq(b'*', StarStarEq, StarStar, StarEq, Star),
            b'/' => self.eq_or(SlashEq, Slash),
            b'%' => self.eq_or(PercentEq, Percent),
            b'^' => self.eq_or(CaretEq, Caret),
            b'<' => self.double_or_eq(b'<', LtLtEq, LtLt, LtEq, Lt),
            b'&' => self.double_or_eq(b'&', AmpAmpEq, AmpAmp, AmpEq, Amp),
            b'|' => self.double_or_eq(b'|', PipePipeEq, PipePipe, PipeEq, Pipe),
            b'=' => {
                if self.eat_byte(b'=') {
                    self.eq_or(EqEqEq, EqEq)
                } else if self.eat_byte(b'>') {
                    Arrow
                } else {
                    Eq
                }
            }
            b'!' => {
                if self.eat_byte(b'=') {
                    self.eq_or(NotEqEq, NotEq)
                } else {
                    Bang
                }
            }
            b'>' => {
                if self.eat_byte(b'>') {
                    self.double_or_eq(b'>', GtGtGtEq, GtGtGt, GtGtEq, GtGt)
                } else {
                    self.eq_or(GtEq, Gt)
                }
            }
            other => return Err(self.err(format!("不正な文字です: {:?}", other as char))),
        };
        Ok(TokenKind::Punct(p))
    }
}

fn is_ident_start(c: char) -> bool {
    c == '_' || c == '$' || c.is_alphabetic()
}

fn is_ident_part(c: char) -> bool {
    is_ident_start(c) || c.is_ascii_digit()
}
