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
    Regex {
        pattern: String,
        flags: String,
    },
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

/// テンプレートリテラルの `${` によって開かれたブレース文脈か、
/// 通常の `{` かを区別するためのスタック要素。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BraceCtx {
    Block,
    Template,
}

/// 正規表現リテラルが許されるかどうかを、直前の意味のあるトークンから判定する。
/// 「識別子・数値・文字列・`)` `]` `++` `--` の直後」は除算、それ以外は正規表現。
fn regex_allowed_after(prev: Option<&TokenKind>) -> bool {
    match prev {
        None => true,
        Some(TokenKind::Ident(name)) => {
            // ほとんどの識別子の後ろは除算だが、キーワードの後ろは正規表現。
            matches!(
                name.as_str(),
                "return" | "typeof" | "instanceof" | "in" | "of" | "new" | "delete" | "void"
                    | "throw" | "case" | "do" | "else" | "yield" | "await" | "extends" | "default"
            )
        }
        Some(TokenKind::Number(_)) | Some(TokenKind::Str(_)) => false,
        Some(TokenKind::NoSubTemplate(_)) | Some(TokenKind::TemplateTail(_)) => false,
        Some(TokenKind::Regex { .. }) => false,
        Some(TokenKind::Punct(p)) => !matches!(
            p,
            Punct::RParen | Punct::RBracket | Punct::PlusPlus | Punct::MinusMinus
        ),
        _ => true,
    }
}

pub struct Lexer<'a> {
    src: &'a [u8],
    chars: Vec<char>,
    byte_offsets: Vec<u32>,
    pos: usize, // chars 上のインデックス
    line: u32,
    col: u32,
    brace_stack: Vec<BraceCtx>,
    last_kind: Option<TokenKind>,
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str) -> Self {
        let chars: Vec<char> = src.chars().collect();
        let mut byte_offsets = Vec::with_capacity(chars.len() + 1);
        let mut b = 0u32;
        for c in &chars {
            byte_offsets.push(b);
            b += c.len_utf8() as u32;
        }
        byte_offsets.push(b);
        Lexer {
            src: src.as_bytes(),
            chars,
            byte_offsets,
            pos: 0,
            line: 1,
            col: 1,
            brace_stack: Vec::new(),
            last_kind: None,
        }
    }

    fn cur_pos(&self) -> Pos {
        Pos {
            line: self.line,
            col: self.col,
            byte: *self.byte_offsets.get(self.pos).unwrap_or(&(self.src.len() as u32)),
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.chars.get(self.pos + offset).copied()
    }

    fn advance(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += 1;
        if c == '\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(c)
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.advance();
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
        let mut out = Vec::new();
        loop {
            let tok = lexer.next_token()?;
            let is_eof = matches!(tok.kind, TokenKind::Eof);
            lexer.last_kind = Some(tok.kind.clone());
            out.push(tok);
            if is_eof {
                break;
            }
        }
        Ok(out)
    }

    /// 空白・改行・コメントを読み飛ばし、改行が含まれていたかを返す。
    fn skip_trivia(&mut self) -> bool {
        let mut saw_newline = false;
        loop {
            match self.peek() {
                Some('\n') => {
                    saw_newline = true;
                    self.advance();
                }
                Some(c) if c.is_whitespace() => {
                    self.advance();
                }
                Some('/') if self.peek_at(1) == Some('/') => {
                    while let Some(c) = self.peek() {
                        if c == '\n' {
                            break;
                        }
                        self.advance();
                    }
                }
                Some('/') if self.peek_at(1) == Some('*') => {
                    self.advance();
                    self.advance();
                    loop {
                        match self.peek() {
                            None => break,
                            Some('*') if self.peek_at(1) == Some('/') => {
                                self.advance();
                                self.advance();
                                break;
                            }
                            Some(c) => {
                                if c == '\n' {
                                    saw_newline = true;
                                }
                                self.advance();
                            }
                        }
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

        let Some(c) = self.peek() else {
            return Ok(Token {
                kind: TokenKind::Eof,
                span: Span::new(start, start),
                newline_before,
            });
        };

        let kind = if c == '`' {
            self.advance();
            self.read_template_part(true)?
        } else if c == '}' && self.brace_stack.last() == Some(&BraceCtx::Template) {
            self.brace_stack.pop();
            self.advance();
            self.read_template_part(false)?
        } else if c.is_ascii_digit() || (c == '.' && self.peek_at(1).map(|d| d.is_ascii_digit()).unwrap_or(false)) {
            self.read_number()?
        } else if c == '"' || c == '\'' {
            self.read_string(c)?
        } else if c == '/' && regex_allowed_after(self.last_kind.as_ref()) {
            self.read_regex()?
        } else if is_ident_start(c) {
            self.read_ident()
        } else {
            self.read_punct()?
        };

        let end = self.cur_pos();
        Ok(Token {
            kind,
            span: Span::new(start, end),
            newline_before,
        })
    }

    fn read_ident(&mut self) -> TokenKind {
        let mut s = String::new();
        while let Some(c) = self.peek() {
            if is_ident_part(c) {
                s.push(c);
                self.advance();
            } else {
                break;
            }
        }
        TokenKind::Ident(s)
    }

    fn read_number(&mut self) -> Result<TokenKind, LexError> {
        let mut s = String::new();
        if self.peek() == Some('0')
            && matches!(self.peek_at(1), Some('x') | Some('X') | Some('o') | Some('O') | Some('b') | Some('B'))
        {
            s.push(self.advance().unwrap()); // '0'
            let radix_char = self.advance().unwrap(); // x/o/b
            s.push(radix_char);
            let radix = match radix_char.to_ascii_lowercase() {
                'x' => 16,
                'o' => 8,
                'b' => 2,
                _ => unreachable!(),
            };
            let mut digits = String::new();
            while let Some(c) = self.peek() {
                if c.is_digit(radix) || c == '_' {
                    if c != '_' {
                        digits.push(c);
                    }
                    self.advance();
                } else {
                    break;
                }
            }
            // BigInt サフィックス 'n' は読み飛ばす（値としては通常数値扱い）。
            self.eat('n');
            let v = i64::from_str_radix(&digits, radix).unwrap_or(0) as f64;
            return Ok(TokenKind::Number(v));
        }

        while let Some(c) = self.peek() {
            if c.is_ascii_digit() || c == '_' {
                if c != '_' {
                    s.push(c);
                }
                self.advance();
            } else {
                break;
            }
        }
        if self.peek() == Some('.') {
            s.push('.');
            self.advance();
            while let Some(c) = self.peek() {
                if c.is_ascii_digit() || c == '_' {
                    if c != '_' {
                        s.push(c);
                    }
                    self.advance();
                } else {
                    break;
                }
            }
        }
        if matches!(self.peek(), Some('e') | Some('E')) {
            s.push('e');
            self.advance();
            if matches!(self.peek(), Some('+') | Some('-')) {
                s.push(self.advance().unwrap());
            }
            while let Some(c) = self.peek() {
                if c.is_ascii_digit() {
                    s.push(c);
                    self.advance();
                } else {
                    break;
                }
            }
        }
        self.eat('n'); // BigInt サフィックス
        let v: f64 = s.parse().map_err(|_| self.err(format!("不正な数値リテラル: {s}")))?;
        Ok(TokenKind::Number(v))
    }

    fn read_string(&mut self, quote: char) -> Result<TokenKind, LexError> {
        self.advance(); // 開き quote
        let mut s = String::new();
        loop {
            match self.peek() {
                None => return Err(self.err("文字列リテラルが閉じられていません")),
                Some(c) if c == quote => {
                    self.advance();
                    break;
                }
                Some('\\') => {
                    self.advance();
                    self.read_escape(&mut s)?;
                }
                Some(c) => {
                    s.push(c);
                    self.advance();
                }
            }
        }
        Ok(TokenKind::Str(s))
    }

    fn read_escape(&mut self, out: &mut String) -> Result<(), LexError> {
        let Some(c) = self.advance() else {
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
                self.eat('\n');
            }
            'x' => {
                let mut hex = String::new();
                for _ in 0..2 {
                    if let Some(h) = self.peek() {
                        hex.push(h);
                        self.advance();
                    }
                }
                if let Ok(code) = u32::from_str_radix(&hex, 16) {
                    if let Some(ch) = char::from_u32(code) {
                        out.push(ch);
                    }
                }
            }
            'u' => {
                if self.peek() == Some('{') {
                    self.advance();
                    let mut hex = String::new();
                    while let Some(h) = self.peek() {
                        if h == '}' {
                            break;
                        }
                        hex.push(h);
                        self.advance();
                    }
                    self.eat('}');
                    if let Ok(code) = u32::from_str_radix(&hex, 16) {
                        if let Some(ch) = char::from_u32(code) {
                            out.push(ch);
                        }
                    }
                } else {
                    let mut hex = String::new();
                    for _ in 0..4 {
                        if let Some(h) = self.peek() {
                            hex.push(h);
                            self.advance();
                        }
                    }
                    if let Ok(code) = u32::from_str_radix(&hex, 16) {
                        if let Some(ch) = char::from_u32(code) {
                            out.push(ch);
                        }
                    }
                }
            }
            other => out.push(other),
        }
        Ok(())
    }

    /// テンプレートリテラルの断片を 1 つ読む。
    /// `is_head` が true ならバッククォート直後（Head/NoSub）、false なら
    /// `}` 直後（Middle/Tail）の読み取り。
    fn read_template_part(&mut self, is_head: bool) -> Result<TokenKind, LexError> {
        let mut s = String::new();
        loop {
            match self.peek() {
                None => return Err(self.err("テンプレートリテラルが閉じられていません")),
                Some('`') => {
                    self.advance();
                    return Ok(if is_head {
                        TokenKind::NoSubTemplate(s)
                    } else {
                        TokenKind::TemplateTail(s)
                    });
                }
                Some('$') if self.peek_at(1) == Some('{') => {
                    self.advance();
                    self.advance();
                    self.brace_stack.push(BraceCtx::Template);
                    return Ok(if is_head {
                        TokenKind::TemplateHead(s)
                    } else {
                        TokenKind::TemplateMiddle(s)
                    });
                }
                Some('\\') => {
                    self.advance();
                    self.read_escape(&mut s)?;
                }
                Some(c) => {
                    s.push(c);
                    self.advance();
                }
            }
        }
    }

    fn read_regex(&mut self) -> Result<TokenKind, LexError> {
        self.advance(); // 先頭の '/'
        let mut pattern = String::new();
        let mut in_class = false;
        loop {
            match self.peek() {
                None => return Err(self.err("正規表現リテラルが閉じられていません")),
                Some('\n') => return Err(self.err("正規表現リテラル中に改行があります")),
                Some('\\') => {
                    pattern.push('\\');
                    self.advance();
                    if let Some(c) = self.peek() {
                        pattern.push(c);
                        self.advance();
                    }
                }
                Some('[') => {
                    in_class = true;
                    pattern.push('[');
                    self.advance();
                }
                Some(']') if in_class => {
                    in_class = false;
                    pattern.push(']');
                    self.advance();
                }
                Some('/') if !in_class => {
                    self.advance();
                    break;
                }
                Some(c) => {
                    pattern.push(c);
                    self.advance();
                }
            }
        }
        let mut flags = String::new();
        while let Some(c) = self.peek() {
            if c.is_alphabetic() {
                flags.push(c);
                self.advance();
            } else {
                break;
            }
        }
        Ok(TokenKind::Regex { pattern, flags })
    }

    fn read_punct(&mut self) -> Result<TokenKind, LexError> {
        let c = self.advance().unwrap();
        use Punct::*;
        let p = match c {
            '(' => LParen,
            ')' => RParen,
            '{' => {
                self.brace_stack.push(BraceCtx::Block);
                LBrace
            }
            '}' => {
                // Template 文脈でないただの '}'。ネストが崩れていても best-effort で pop。
                if self.brace_stack.last() == Some(&BraceCtx::Block) {
                    self.brace_stack.pop();
                }
                RBrace
            }
            '[' => LBracket,
            ']' => RBracket,
            ';' => Semi,
            ',' => Comma,
            '~' => Tilde,
            '@' => At,
            '#' => Hash,
            '.' => {
                if self.peek() == Some('.') && self.peek_at(1) == Some('.') {
                    self.advance();
                    self.advance();
                    DotDotDot
                } else {
                    Dot
                }
            }
            ':' => Colon,
            '?' => {
                if self.peek() == Some('.') && !self.peek_at(1).map(|d| d.is_ascii_digit()).unwrap_or(false) {
                    self.advance();
                    QuestionDot
                } else if self.peek() == Some('?') {
                    self.advance();
                    if self.eat('=') {
                        QuestionQuestionEq
                    } else {
                        QuestionQuestion
                    }
                } else {
                    Question
                }
            }
            '+' => {
                if self.eat('+') {
                    PlusPlus
                } else if self.eat('=') {
                    PlusEq
                } else {
                    Plus
                }
            }
            '-' => {
                if self.eat('-') {
                    MinusMinus
                } else if self.eat('=') {
                    MinusEq
                } else {
                    Minus
                }
            }
            '*' => {
                if self.eat('*') {
                    if self.eat('=') {
                        StarStarEq
                    } else {
                        StarStar
                    }
                } else if self.eat('=') {
                    StarEq
                } else {
                    Star
                }
            }
            '/' => {
                if self.eat('=') {
                    SlashEq
                } else {
                    Slash
                }
            }
            '%' => {
                if self.eat('=') {
                    PercentEq
                } else {
                    Percent
                }
            }
            '=' => {
                if self.peek() == Some('=') && self.peek_at(1) == Some('=') {
                    self.advance();
                    self.advance();
                    EqEqEq
                } else if self.eat('=') {
                    EqEq
                } else if self.eat('>') {
                    Arrow
                } else {
                    Eq
                }
            }
            '!' => {
                if self.peek() == Some('=') && self.peek_at(1) == Some('=') {
                    self.advance();
                    self.advance();
                    NotEqEq
                } else if self.eat('=') {
                    NotEq
                } else {
                    Bang
                }
            }
            '<' => {
                if self.peek() == Some('<') {
                    self.advance();
                    if self.eat('=') {
                        LtLtEq
                    } else {
                        LtLt
                    }
                } else if self.eat('=') {
                    LtEq
                } else {
                    Lt
                }
            }
            '>' => {
                if self.peek() == Some('>') && self.peek_at(1) == Some('>') {
                    self.advance();
                    self.advance();
                    if self.eat('=') {
                        GtGtGtEq
                    } else {
                        GtGtGt
                    }
                } else if self.peek() == Some('>') {
                    self.advance();
                    if self.eat('=') {
                        GtGtEq
                    } else {
                        GtGt
                    }
                } else if self.eat('=') {
                    GtEq
                } else {
                    Gt
                }
            }
            '&' => {
                if self.peek() == Some('&') {
                    self.advance();
                    if self.eat('=') {
                        AmpAmpEq
                    } else {
                        AmpAmp
                    }
                } else if self.eat('=') {
                    AmpEq
                } else {
                    Amp
                }
            }
            '|' => {
                if self.peek() == Some('|') {
                    self.advance();
                    if self.eat('=') {
                        PipePipeEq
                    } else {
                        PipePipe
                    }
                } else if self.eat('=') {
                    PipeEq
                } else {
                    Pipe
                }
            }
            '^' => {
                if self.eat('=') {
                    CaretEq
                } else {
                    Caret
                }
            }
            other => return Err(self.err(format!("不正な文字です: {other:?}"))),
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
