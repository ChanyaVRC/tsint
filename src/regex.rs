//! T4: 正規表現エンジン本実装。
//!
//! T2 の `regex_mini.rs`（リテラル・文字クラス・代替・エスケープ
//! ショートハンドのみの最小サブセット）を置き換える、バックトラッキング
//! 方式の本実装。「JS の RegExp の実用サブセット」（Noiria コーパスで
//! 実際に使われる構文 + ゲーム開発者が普通に書く範囲）を対象にする。
//!
//! ## 対応構文
//!
//! - リテラル文字、`.`（任意の 1 文字。`s` フラグで改行にもマッチ）
//! - 文字クラス `[...]`（否定 `[^...]`・範囲 `a-z`・クラス内エスケープ・
//!   クラス内ショートハンド `\d\D\w\W\s\S`）
//! - エスケープショートハンド `\d\D\w\W\s\S`、制御エスケープ
//!   `\n\r\t\v\f\0`、16 進エスケープ `\xHH`、Unicode エスケープ
//!   `\uHHHH` / `\u{H...}`
//! - 量指定子 `* + ? {n} {n,} {n,m}`（末尾 `?` で非貪欲化）
//! - グループ `(...)`（キャプチャ）/ `(?:...)`（非キャプチャ）
//! - 選択 `|`（優先順位あり: 左の枝から順に試す）
//! - アンカー `^` `$`（`m` フラグで行頭・行末にも一致）
//! - 単語境界 `\b` `\B`
//! - 後方参照 `\1`〜（未マッチのグループへの参照は空文字列として扱う。
//!   JS 仕様通り）
//! - 先読み `(?=...)` `(?!...)`
//!
//! 非対応（明確なコンパイルエラーになる）: 後読み `(?<=...)` `(?<!...)`、
//! 名前付きキャプチャ `(?<name>...)`・名前付き後方参照 `\k<name>`、
//! `y`（sticky）・`d`（indices）フラグ、Unicode プロパティエスケープ
//! `\p{...}`。
//!
//! ## 実行方式
//!
//! AST をいったん「Save/Jmp/Split/Match」などからなるフラットな命令列
//! （`Inst`）へコンパイルし（ジャンプ先はコンパイル時に確定するアドレス
//! 方式。バックパッチはこのモジュール内で完結する）、その命令列を
//! ヒープ上の明示的なバックトラックスタックとアンドゥログ（trail）を用いる
//! 反復ループ `Exec::run_from` でバックトラッキング実行する。
//!
//! 選択 `|` と量指定子 `* + ? {n,m}` はいずれも `Split`/`Jmp` を使って
//! **地続きの命令列としてインライン展開**する（量指定子の外側にある残りの
//! パターンとの継続関係を、Rust の関数呼び出しの戻り値ではなく通常の pc の
//! 流れで表現する）。こうすることで、量指定子の中にネストした別の量指定子や
//! 選択があっても、外側が後から失敗してバックトラックした時に本体側の
//! 「何文字消費するか」の選択を正しく再試行できる（`{n,m}` の上限分は
//! 有限個の `Split` を入れ子に展開し、`*`/`+`/`{n,}` の無制限部分は
//! `Split` + 進捗チェック付きの `LoopBack` によるループとして展開する。
//! 空文字列にしかマッチしない本体を持つループは `LoopBack` が進捗の有無を
//! 検知して打ち切ることで無限ループを防ぐ）。
//!
//! 一方、先読み `(?=...)` `(?!...)` は常にゼロ幅（`sp` を進めない）なので
//! 話が別で、`Inst::Look` は対応する部分命令列を「独立したサブルーチン」
//! として命令列内に埋め込み、`sub_pc` から実行してみて成否だけを判定してから
//! 本来の後続アドレス `after_pc` へ進む、という設計で問題ない（後続の
//! マッチが失敗しても、先読みの中身をどう解釈し直すかで結果が変わることは
//! 無いため、先読みの内側だけを独立に「最初に見つかった成功」で確定して
//! 良い）。
//!
//! バックトラッキングの選択点（`Split`・`Look`）では、`saved` キャプチャ配列
//! 全体をクローンする代わりに、変更前の値を記録するアンドゥログ（`trail`）
//! の長さをマークしておき、枝が失敗したらそのマーク位置まで逆順に巻き戻す
//! ことで、バックトラック時のヒープ再確保をゼロに抑えている。
//!
//! 後方参照・先読みは正規言語ではないため、Thompson 構成の NFA
//! シミュレーションでは実行できない（これが本エンジンがバックトラッキング
//! 方式を選んだ理由）。トレードオフとして、病的なパターン（壊滅的
//! バックトラック）に対する脆弱性があるため、実行ステップ数・再帰の深さに
//! 上限を設け、超過時は `RegexError` を返す（JS の実際の挙動とは異なるが、
//! 「無限に固まるより明確なエラーの方が良い」という方針を採る）。
//!
//! ## Unicode / UTF-16 に関する既知の差異
//!
//! 本エンジンは Rust の `char`（Unicode スカラ値）単位で動作する。JS の
//! 文字列は内部的に UTF-16 コード単位の列であり、サロゲートペア
//! （絵文字等の追加面文字）は `u` フラグ無しでは 2 つの別々の「文字」として
//! 扱われる。本エンジンは `u` フラグの有無に関わらず常にコードポイント
//! 単位で動作するため、サロゲートペアの挙動は JS の非 `u` モードとは異なる
//! （`u` モードの挙動に近い）。`.length` や `charCodeAt` 等、文字列側の
//! UTF-16 セマンティクスは `builtins.rs` 側の既存実装に準拠する。

// =====================================================================
// 文字クラス
// =====================================================================

#[derive(Debug, Clone)]
enum ClassAtom {
    Char(char),
    Range(char, char),
    Digit,
    NotDigit,
    Word,
    NotWord,
    Space,
    NotSpace,
}

fn is_digit(c: char) -> bool {
    c.is_ascii_digit()
}

/// JS の `\w` は Unicode 全体ではなく ASCII の `[A-Za-z0-9_]` のみ
/// （`u` フラグを付けても変わらない。T2 の `regex_mini.rs` では
/// `is_alphanumeric()` を使っておりここが不正確だったため、本実装で修正した）。
fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// JS の `\s` は ASCII 空白に加え Unicode の空白カテゴリ・BOM(U+FEFF) を含む。
/// Rust の `char::is_whitespace()`（Unicode `White_Space` プロパティ）に
/// BOM を足すことで近似する（既知の差異として README に明記）。
fn is_space(c: char) -> bool {
    c.is_whitespace() || c == '\u{FEFF}'
}

fn is_line_terminator(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

fn atom_matches(a: &ClassAtom, c: char, ignore_case: bool) -> bool {
    match a {
        ClassAtom::Char(x) => char_eq(c, *x, ignore_case),
        ClassAtom::Range(lo, hi) => range_matches(*lo, *hi, c, ignore_case),
        ClassAtom::Digit => is_digit(c),
        ClassAtom::NotDigit => !is_digit(c),
        ClassAtom::Word => is_word(c),
        ClassAtom::NotWord => !is_word(c),
        ClassAtom::Space => is_space(c),
        ClassAtom::NotSpace => !is_space(c),
    }
}

/// 大文字小文字を無視した文字比較の簡易実装（`to_lowercase` の結果同士を
/// 比較する。1 文字が複数文字に展開されるケース（独 ß 等）は先頭のみ比較する
/// 近似実装。実用上のゲームテキストでは問題にならない想定）。
fn char_eq(a: char, b: char, ignore_case: bool) -> bool {
    if a == b {
        return true;
    }
    if !ignore_case {
        return false;
    }
    // 簡易的な大文字小文字比較（`to_lowercase()` の結果の 1 文字目同士を
    // 比較する近似実装。独 ß のように 1 文字が複数文字へ展開されるケースは
    // 厳密な Unicode ケースフォールディングとは異なりうるが、ゲームテキスト
    // の実用上は問題にならない想定。README の既知の差異を参照）。
    a.to_lowercase().next() == b.to_lowercase().next()
}

fn case_variants(c: char) -> [Option<char>; 2] {
    let lower = c.to_lowercase().next();
    let upper = c.to_uppercase().next();
    [lower, upper]
}

fn range_matches(lo: char, hi: char, c: char, ignore_case: bool) -> bool {
    if lo <= c && c <= hi {
        return true;
    }
    if ignore_case {
        for v in case_variants(c).into_iter().flatten() {
            if lo <= v && v <= hi {
                return true;
            }
        }
    }
    false
}

#[derive(Debug, Clone)]
struct CharClass {
    negate: bool,
    atoms: Vec<ClassAtom>,
}

impl CharClass {
    fn matches(&self, c: char, ignore_case: bool) -> bool {
        let hit = self.atoms.iter().any(|a| atom_matches(a, c, ignore_case));
        hit != self.negate
    }
}

// =====================================================================
// AST
// =====================================================================

#[derive(Debug, Clone)]
enum Ast {
    Empty,
    Char(char),
    Any,
    Class(CharClass),
    Concat(Vec<Ast>),
    Alt(Vec<Ast>),
    Group(usize, Box<Ast>),
    NonCap(Box<Ast>),
    Repeat(Box<Ast>, u32, Option<u32>, bool),
    Start,
    End,
    WordB(bool),
    Backref(usize),
    Look(Box<Ast>, bool),
}

#[derive(Debug)]
pub struct RegexError(pub String);

// =====================================================================
// パーサ
// =====================================================================

enum ClassEscape {
    Char(char),
    Atom(ClassAtom),
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
    ncaptures: usize,
    total_captures: usize,
}

/// パターン全体を前もって走査し、キャプチャグループの総数を数える
/// （前方参照される後方参照 `\1` の妥当性チェックに使う）。
/// 名前付きキャプチャ・後読みは本実装では未対応（明確なエラーになる）ため、
/// ここでは「非キャプチャ考慮なしの `(` を全部数える」簡易実装で十分。
fn count_capturing_groups(chars: &[char]) -> usize {
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

    fn parse_pattern(&mut self) -> Result<Ast, RegexError> {
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
            let greedy = !self.eat('?');
            Ok(Ast::Repeat(Box::new(atom), min, max, greedy))
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
        let mut digits1 = String::new();
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                digits1.push(c);
                self.pos += 1;
            } else {
                break;
            }
        }
        if digits1.is_empty() {
            self.pos = start;
            return None;
        }
        let min: u32 = digits1.parse().ok()?;
        if self.eat(',') {
            let mut digits2 = String::new();
            while let Some(c) = self.peek() {
                if c.is_ascii_digit() {
                    digits2.push(c);
                    self.pos += 1;
                } else {
                    break;
                }
            }
            if digits2.is_empty() {
                Some((min, None))
            } else {
                let max: u32 = digits2.parse().ok()?;
                Some((min, Some(max)))
            }
        } else {
            Some((min, Some(min)))
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
        if self.eat('?') {
            if self.eat(':') {
                let inner = self.parse_alt()?;
                self.expect(')')?;
                return Ok(Ast::NonCap(Box::new(inner)));
            } else if self.eat('=') {
                let inner = self.parse_alt()?;
                self.expect(')')?;
                return Ok(Ast::Look(Box::new(inner), false));
            } else if self.eat('!') {
                let inner = self.parse_alt()?;
                self.expect(')')?;
                return Ok(Ast::Look(Box::new(inner), true));
            } else if self.peek() == Some('<') {
                if self.peek_at(1) == Some('=') || self.peek_at(1) == Some('!') {
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
        }
        self.ncaptures += 1;
        let idx = self.ncaptures;
        let inner = self.parse_alt()?;
        self.expect(')')?;
        Ok(Ast::Group(idx, Box::new(inner)))
    }

    fn parse_hex_escape(&mut self, n: usize) -> Result<char, RegexError> {
        let mut s = String::new();
        for _ in 0..n {
            match self.advance() {
                Some(c) if c.is_ascii_hexdigit() => s.push(c),
                _ => return Err(RegexError("不正な16進エスケープです".to_string())),
            }
        }
        let code = u32::from_str_radix(&s, 16)
            .map_err(|_| RegexError("不正な16進エスケープです".to_string()))?;
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

    fn parse_escape(&mut self) -> Result<Ast, RegexError> {
        self.pos += 1; // '\'
        let c = self
            .advance()
            .ok_or_else(|| RegexError("パターンがバックスラッシュで終わっています".to_string()))?;
        Ok(match c {
            'd' => Ast::Class(CharClass {
                negate: false,
                atoms: vec![ClassAtom::Digit],
            }),
            'D' => Ast::Class(CharClass {
                negate: false,
                atoms: vec![ClassAtom::NotDigit],
            }),
            'w' => Ast::Class(CharClass {
                negate: false,
                atoms: vec![ClassAtom::Word],
            }),
            'W' => Ast::Class(CharClass {
                negate: false,
                atoms: vec![ClassAtom::NotWord],
            }),
            's' => Ast::Class(CharClass {
                negate: false,
                atoms: vec![ClassAtom::Space],
            }),
            'S' => Ast::Class(CharClass {
                negate: false,
                atoms: vec![ClassAtom::NotSpace],
            }),
            'b' => Ast::WordB(true),
            'B' => Ast::WordB(false),
            'n' => Ast::Char('\n'),
            'r' => Ast::Char('\r'),
            't' => Ast::Char('\t'),
            'v' => Ast::Char('\u{000B}'),
            'f' => Ast::Char('\u{000C}'),
            'x' => Ast::Char(self.parse_hex_escape(2)?),
            'u' => Ast::Char(self.parse_unicode_escape()?),
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
                let mut num = String::from(c);
                while let Some(d) = self.peek() {
                    if d.is_ascii_digit() {
                        num.push(d);
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                let n: usize = num.parse().unwrap_or(0);
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
        Ok(match c {
            'd' => ClassEscape::Atom(ClassAtom::Digit),
            'D' => ClassEscape::Atom(ClassAtom::NotDigit),
            'w' => ClassEscape::Atom(ClassAtom::Word),
            'W' => ClassEscape::Atom(ClassAtom::NotWord),
            's' => ClassEscape::Atom(ClassAtom::Space),
            'S' => ClassEscape::Atom(ClassAtom::NotSpace),
            'b' => ClassEscape::Char('\u{0008}'), // クラス内の \b はバックスペース
            'n' => ClassEscape::Char('\n'),
            'r' => ClassEscape::Char('\r'),
            't' => ClassEscape::Char('\t'),
            'v' => ClassEscape::Char('\u{000B}'),
            'f' => ClassEscape::Char('\u{000C}'),
            '0' => ClassEscape::Char('\0'),
            'x' => ClassEscape::Char(self.parse_hex_escape(2)?),
            'u' => ClassEscape::Char(self.parse_unicode_escape()?),
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
        Ok(Ast::Class(CharClass { negate, atoms }))
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

// =====================================================================
// コンパイル: AST → フラットな命令列
// =====================================================================

#[derive(Debug, Clone)]
enum Inst {
    Char(char),
    Any,
    Class(CharClass),
    Save(usize),
    Jmp(usize),
    Split(usize, usize),
    Match,
    Bol,
    Eol,
    WordB(bool),
    Backref(usize),
    /// 先読み。`sub_pc` から始まるサブプログラム（`Inst::Match` で終わる）を
    /// 現在位置 `sp` から試し、成否（`negate` で反転）を判定してから
    /// `after_pc`（サブプログラムの直後ではなく、先読みブロック全体の直後）
    /// へ進む。先読みは常にゼロ幅（`sp` を進めない）なので、量指定子と違い
    /// 「本体の別解を後から試す」必要が無く、サブルーチン的な構造のままで
    /// 問題ない。
    Look {
        sub_pc: usize,
        after_pc: usize,
        negate: bool,
    },
    /// 無制限（`*`/`+`/`{n,}`）ループの「もう1周するか、抜けるか」の判定。
    /// `mark_slot`（`Inst::Save` と同じ `saved` 配列上のスクラッチスロット。
    /// 実際のキャプチャ番号とは重ならない領域を使う）に記録された「このループを
    /// 開始した時点の `sp`」と現在の `sp` を比較し、進捗があれば `loop_pc`
    /// （ループ本体の先頭。実体は `Split`）へ戻り、無ければそのまま
    /// 素通りする（＝ループを抜ける）。これにより、本体が空文字列にしか
    /// マッチしない場合の無限ループを防ぐ。
    LoopBack {
        mark_slot: usize,
        loop_pc: usize,
    },
}

struct Compiler {
    prog: Vec<Inst>,
    /// キャプチャ用スロット数（`2*(ncaptures+1)`）。スクラッチスロットは
    /// この直後の領域に割り当てるので、衝突しないようにここで把握しておく。
    capture_slots: usize,
    /// 量指定子の「進捗チェック」に使うスクラッチスロットの個数。
    scratch_count: usize,
}

impl Compiler {
    fn emit(&mut self, i: Inst) -> usize {
        self.prog.push(i);
        self.prog.len() - 1
    }
    fn here(&self) -> usize {
        self.prog.len()
    }
    /// キャプチャ用スロットと重ならない、量指定子専用のスクラッチスロットを
    /// 1 つ確保する。
    fn alloc_scratch(&mut self) -> usize {
        let s = self.capture_slots + self.scratch_count;
        self.scratch_count += 1;
        s
    }

    fn compile(&mut self, ast: &Ast) {
        match ast {
            Ast::Empty => {}
            Ast::Char(c) => {
                self.emit(Inst::Char(*c));
            }
            Ast::Any => {
                self.emit(Inst::Any);
            }
            Ast::Class(cc) => {
                self.emit(Inst::Class(cc.clone()));
            }
            Ast::Concat(items) => {
                for it in items {
                    self.compile(it);
                }
            }
            Ast::Alt(branches) => {
                let mut jmp_ends = vec![];
                for (i, b) in branches.iter().enumerate() {
                    if i + 1 == branches.len() {
                        self.compile(b);
                    } else {
                        let split_pc = self.emit(Inst::Split(0, 0));
                        let branch_start = self.here();
                        self.compile(b);
                        let jmp_pc = self.emit(Inst::Jmp(0));
                        jmp_ends.push(jmp_pc);
                        let next_branch_start = self.here();
                        if let Inst::Split(ref mut a, ref mut b2) = self.prog[split_pc] {
                            *a = branch_start;
                            *b2 = next_branch_start;
                        }
                    }
                }
                let end = self.here();
                for pc in jmp_ends {
                    if let Inst::Jmp(ref mut t) = self.prog[pc] {
                        *t = end;
                    }
                }
            }
            Ast::Group(idx, inner) => {
                self.emit(Inst::Save(idx * 2));
                self.compile(inner);
                self.emit(Inst::Save(idx * 2 + 1));
            }
            Ast::NonCap(inner) => self.compile(inner),
            Ast::Repeat(inner, min, max, greedy) => self.compile_repeat(inner, *min, *max, *greedy),
            Ast::Start => {
                self.emit(Inst::Bol);
            }
            Ast::End => {
                self.emit(Inst::Eol);
            }
            Ast::WordB(b) => {
                self.emit(Inst::WordB(*b));
            }
            Ast::Backref(n) => {
                self.emit(Inst::Backref(*n));
            }
            Ast::Look(inner, negate) => {
                let look_pc = self.emit(Inst::Look {
                    sub_pc: 0,
                    after_pc: 0,
                    negate: *negate,
                });
                let sub_start = self.here();
                self.compile(inner);
                self.emit(Inst::Match);
                let after = self.here();
                if let Inst::Look {
                    ref mut sub_pc,
                    ref mut after_pc,
                    ..
                } = self.prog[look_pc]
                {
                    *sub_pc = sub_start;
                    *after_pc = after;
                }
            }
        }
    }

    /// 量指定子本体は、他の構文と同じ「地続きの命令列」としてインライン展開
    /// する（量指定子の外側にある残りのパターンとの継続関係を、Rust の関数
    /// 呼び出しの戻り値ではなく通常の pc の流れで表現する）。こうすることで、
    /// 量指定子の外側が後から失敗してバックトラックした時に、本体側の
    /// 「何文字消費するか」の選択（本体の中にネストした別の量指定子・選択が
    /// あるケース）も通常の `Split` の巻き戻しでそのまま正しく再試行される。
    /// （先読み `Look` と違い、量指定子の本体は「消費する文字数」が結果に
    /// 直結するため、`Look` のように「本体を 1 回だけ呼んでサブルーチン的に
    /// 結果を確定させる」設計にはできない。本体を呼ぶたびに 1 つの結果しか
    /// 返せない設計だと、外側の継続が失敗した時に本体の「別の消費文字数」を
    /// 再試行できず、量指定子のネストを含むバックトラックが不完全になる）。
    fn compile_repeat(&mut self, inner: &Ast, min: u32, max: Option<u32>, greedy: bool) {
        // 必須の min 回分。
        for _ in 0..min {
            self.compile(inner);
        }
        match max {
            Some(m) => {
                let extra = m.saturating_sub(min);
                self.compile_optional_chain(inner, extra, greedy);
            }
            None => self.compile_unbounded_loop(inner, greedy),
        }
    }

    /// 「あってもなくても良い」コピーを `remaining` 個、入れ子の `Split` で
    /// 展開する（`{n,m}` の m-n 分。有限個なので無限ループの心配は無い。
    /// 大きな `remaining` でもコンパイル時にコールスタックを消費しないよう反復で展開する）。
    fn compile_optional_chain(&mut self, inner: &Ast, remaining: u32, greedy: bool) {
        if remaining == 0 {
            return;
        }
        let mut splits = Vec::with_capacity(remaining as usize);
        for _ in 0..remaining {
            let split_pc = self.emit(Inst::Split(0, 0));
            let body_start = self.here();
            splits.push((split_pc, body_start));
            self.compile(inner);
        }
        let after = self.here();
        for (split_pc, body_start) in splits {
            if let Inst::Split(ref mut a, ref mut b) = self.prog[split_pc] {
                if greedy {
                    *a = body_start;
                    *b = after;
                } else {
                    *a = after;
                    *b = body_start;
                }
            }
        }
    }

    /// `*`/`+`/`{n,}` の無制限部分（Kleene 的ループ）。
    fn compile_unbounded_loop(&mut self, inner: &Ast, greedy: bool) {
        let mark_slot = self.alloc_scratch();
        let split_pc = self.emit(Inst::Split(0, 0));
        let body_start = self.here();
        self.emit(Inst::Save(mark_slot));
        self.compile(inner);
        self.emit(Inst::LoopBack {
            mark_slot,
            loop_pc: split_pc,
        });
        let after = self.here();
        if let Inst::Split(ref mut a, ref mut b) = self.prog[split_pc] {
            if greedy {
                *a = body_start;
                *b = after;
            } else {
                *a = after;
                *b = body_start;
            }
        }
    }
}

// =====================================================================
// 実行器（明示スタック＋アンドゥログによるバックトラッキング）
// =====================================================================

/// 壊滅的バックトラック対策のデフォルト上限。超過時は `RegexError` を返す
/// （JS の実際の挙動とは異なるが、無限に固まるよりは明確なエラーの方が
/// 良いという方針。詳細はモジュール冒頭のドキュメント参照）。
const DEFAULT_MAX_STEPS: u64 = 2_000_000;
/// ヒープ上のバックトラックスタック深さの上限（過大なメモリ確保を防ぐ）。
const DEFAULT_MAX_DEPTH: usize = 50_000;
/// 先読み `(?=...)` / `(?!...)` のネスト上限（OS コールスタック保護）。
const MAX_LOOK_DEPTH: usize = 256;

type Saved = Vec<Option<usize>>;
type Trail = Vec<(usize, Option<usize>)>;

#[derive(Clone, Copy)]
struct BacktrackFrame {
    pc: usize,
    sp: usize,
    trail_mark: usize,
}

#[inline]
fn rewind_trail(saved: &mut [Option<usize>], trail: &mut Trail, mark: usize) {
    while trail.len() > mark {
        let (slot, prev) = trail.pop().unwrap();
        saved[slot] = prev;
    }
}

struct Exec<'a> {
    prog: &'a [Inst],
    chars: &'a [char],
    ignore_case: bool,
    multiline: bool,
    dot_all: bool,
    steps: u64,
    max_steps: u64,
    max_depth: usize,
}

impl<'a> Exec<'a> {
    #[inline]
    fn step(&mut self) -> Result<(), RegexError> {
        self.steps += 1;
        if self.steps > self.max_steps {
            return Err(RegexError(
                "正規表現の実行ステップ数上限を超えました（壊滅的バックトラックの可能性があります）"
                    .to_string(),
            ));
        }
        Ok(())
    }

    fn run_from(
        &mut self,
        mut pc: usize,
        mut sp: usize,
        saved: &mut [Option<usize>],
        trail: &mut Trail,
        stack: &mut Vec<BacktrackFrame>,
        look_depth: usize,
    ) -> Result<Option<usize>, RegexError> {
        if look_depth > MAX_LOOK_DEPTH {
            return Err(RegexError(
                "正規表現の再帰の深さ上限を超えました（先読みのネストが深すぎます）".to_string(),
            ));
        }
        let stack_base = stack.len();
        let base_trail_mark = trail.len();

        loop {
            self.step()?;
            let ok = match &self.prog[pc] {
                Inst::Char(c) => {
                    if sp < self.chars.len() && char_eq(self.chars[sp], *c, self.ignore_case) {
                        pc += 1;
                        sp += 1;
                        true
                    } else {
                        false
                    }
                }
                Inst::Any => {
                    if sp < self.chars.len()
                        && (self.dot_all || !is_line_terminator(self.chars[sp]))
                    {
                        pc += 1;
                        sp += 1;
                        true
                    } else {
                        false
                    }
                }
                Inst::Class(cc) => {
                    if sp < self.chars.len() && cc.matches(self.chars[sp], self.ignore_case) {
                        pc += 1;
                        sp += 1;
                        true
                    } else {
                        false
                    }
                }
                Inst::Save(slot) => {
                    let slot = *slot;
                    let prev = saved[slot];
                    if prev != Some(sp) {
                        trail.push((slot, prev));
                        saved[slot] = Some(sp);
                    }
                    pc += 1;
                    true
                }
                Inst::Jmp(t) => {
                    pc = *t;
                    true
                }
                Inst::Split(a, b) => {
                    let (a, b) = (*a, *b);
                    if stack.len() >= self.max_depth {
                        return Err(RegexError(
                            "正規表現の再帰の深さ上限を超えました（パターンまたは入力が大きすぎます）"
                                .to_string(),
                        ));
                    }
                    stack.push(BacktrackFrame {
                        pc: b,
                        sp,
                        trail_mark: trail.len(),
                    });
                    pc = a;
                    true
                }
                Inst::Match => {
                    stack.truncate(stack_base);
                    return Ok(Some(sp));
                }
                Inst::Bol => {
                    let ok = sp == 0
                        || (self.multiline && sp > 0 && is_line_terminator(self.chars[sp - 1]));
                    if ok {
                        pc += 1;
                        true
                    } else {
                        false
                    }
                }
                Inst::Eol => {
                    let ok = sp == self.chars.len()
                        || (self.multiline && is_line_terminator(self.chars[sp]));
                    if ok {
                        pc += 1;
                        true
                    } else {
                        false
                    }
                }
                Inst::WordB(want) => {
                    let before = sp > 0 && is_word(self.chars[sp - 1]);
                    let after = sp < self.chars.len() && is_word(self.chars[sp]);
                    if (before != after) == *want {
                        pc += 1;
                        true
                    } else {
                        false
                    }
                }
                Inst::Backref(n) => {
                    let slot = n * 2;
                    match (
                        saved.get(slot).copied().flatten(),
                        saved.get(slot + 1).copied().flatten(),
                    ) {
                        (Some(s), Some(e)) if e >= s => {
                            let len = e - s;
                            let matched = sp + len <= self.chars.len()
                                && (0..len).all(|i| {
                                    char_eq(self.chars[sp + i], self.chars[s + i], self.ignore_case)
                                });
                            if matched {
                                pc += 1;
                                sp += len;
                                true
                            } else {
                                false
                            }
                        }
                        // 未マッチのグループへの後方参照は空文字列として扱う（JS 仕様）。
                        _ => {
                            pc += 1;
                            true
                        }
                    }
                }
                Inst::Look {
                    sub_pc,
                    after_pc,
                    negate,
                } => {
                    let (sub_pc, after_pc, negate) = (*sub_pc, *after_pc, *negate);
                    let look_trail_mark = trail.len();
                    let matched = self
                        .run_from(sub_pc, sp, saved, trail, stack, look_depth + 1)?
                        .is_some();
                    if matched != negate {
                        // 肯定先読み成功時は saved への変更と trail のアンドゥ記録をそのまま残す
                        // （外側が後でバックトラックした場合も自動で巻き戻される）。
                        // 否定先読み成功時は内部が不成立だったため run_from 内で巻き戻し済み。
                        pc = after_pc;
                        true
                    } else {
                        // 否定先読み失敗（= 内部がマッチ）時は内部キャプチャを巻き戻す。
                        rewind_trail(saved, trail, look_trail_mark);
                        false
                    }
                }
                Inst::LoopBack { mark_slot, loop_pc } => {
                    let (mark_slot, loop_pc) = (*mark_slot, *loop_pc);
                    let progressed = saved.get(mark_slot).copied().flatten() != Some(sp);
                    if progressed {
                        pc = loop_pc;
                    } else {
                        // 前回のこのループ反復開始時と `sp` が変わっていない
                        // （＝本体が空文字列にしかマッチしなかった）。無限ループを
                        // 避けるため、ここでループを打ち切って素通りする。
                        pc += 1;
                    }
                    true
                }
            };

            if !ok {
                if stack.len() > stack_base {
                    let frame = stack.pop().unwrap();
                    rewind_trail(saved, trail, frame.trail_mark);
                    pc = frame.pc;
                    sp = frame.sp;
                } else {
                    rewind_trail(saved, trail, base_trail_mark);
                    return Ok(None);
                }
            }
        }
    }
}

// =====================================================================
// 公開 API
// =====================================================================

/// マッチ結果。位置はすべて文字（Unicode スカラ値）インデックス。
#[derive(Debug, Clone)]
pub struct RegexMatch {
    pub start: usize,
    pub end: usize,
    /// 1 番から順に、キャプチャグループの (開始, 終了)。未参加のグループは
    /// `None`（JS の `undefined` に対応）。
    pub groups: Vec<Option<(usize, usize)>>,
}

pub struct Regex {
    pub source: String,
    pub flags: String,
    pub global: bool,
    pub ignore_case: bool,
    pub multiline: bool,
    pub dot_all: bool,
    pub unicode: bool,
    ncaptures: usize,
    /// `saved` 配列の総スロット数（キャプチャ用 `2*(ncaptures+1)` +
    /// 量指定子の進捗チェック用スクラッチスロット）。
    total_slots: usize,
    prog: Vec<Inst>,
}

impl Regex {
    pub fn compile(pattern: &str, flags: &str) -> Result<Regex, RegexError> {
        let mut global = false;
        let mut ignore_case = false;
        let mut multiline = false;
        let mut dot_all = false;
        let mut unicode = false;
        for f in flags.chars() {
            let dup = match f {
                'g' => &mut global,
                'i' => &mut ignore_case,
                'm' => &mut multiline,
                's' => &mut dot_all,
                'u' => &mut unicode,
                other => {
                    return Err(RegexError(format!(
                        "未対応の正規表現フラグ '{other}' です（tsint T4 が対応するのは g i m s u のみ）"
                    )))
                }
            };
            if *dup {
                return Err(RegexError(format!("フラグ '{f}' が重複しています")));
            }
            *dup = true;
        }
        let chars: Vec<char> = pattern.chars().collect();
        let total_captures = count_capturing_groups(&chars);
        let mut p = Parser {
            chars,
            pos: 0,
            ncaptures: 0,
            total_captures,
        };
        let ast = p.parse_pattern()?;
        let ncaptures = p.ncaptures;
        let mut c = Compiler {
            prog: vec![],
            capture_slots: 2 * (ncaptures + 1),
            scratch_count: 0,
        };
        c.emit(Inst::Save(0));
        c.compile(&ast);
        c.emit(Inst::Save(1));
        c.emit(Inst::Match);
        let total_slots = 2 * (ncaptures + 1) + c.scratch_count;
        Ok(Regex {
            source: pattern.to_string(),
            flags: flags.to_string(),
            global,
            ignore_case,
            multiline,
            dot_all,
            unicode,
            ncaptures,
            total_slots,
            prog: c.prog,
        })
    }

    pub fn ncaptures(&self) -> usize {
        self.ncaptures
    }

    /// `from`（文字インデックス）以降の最初の一致を探す（非アンカーの部分
    /// 一致検索。実際の JS の `RegExp.prototype.exec` の挙動に相当）。
    pub fn search(&self, chars: &[char], from: usize) -> Result<Option<RegexMatch>, RegexError> {
        let nslots = self.total_slots;
        let mut exec = Exec {
            prog: &self.prog,
            chars,
            ignore_case: self.ignore_case,
            multiline: self.multiline,
            dot_all: self.dot_all,
            steps: 0,
            max_steps: DEFAULT_MAX_STEPS,
            max_depth: DEFAULT_MAX_DEPTH,
        };
        let mut saved: Saved = vec![None; nslots];
        let mut trail: Trail = Vec::new();
        let mut stack: Vec<BacktrackFrame> = Vec::new();
        let mut s = from;
        loop {
            if s > chars.len() {
                return Ok(None);
            }
            if exec
                .run_from(0, s, &mut saved, &mut trail, &mut stack, 0)?
                .is_some()
            {
                let get = |i: usize| saved.get(i).copied().flatten();
                let start = get(0).unwrap();
                let end = get(1).unwrap();
                let mut groups = Vec::with_capacity(self.ncaptures);
                for gi in 1..=self.ncaptures {
                    groups.push(match (get(gi * 2), get(gi * 2 + 1)) {
                        (Some(a), Some(b)) => Some((a, b)),
                        _ => None,
                    });
                }
                return Ok(Some(RegexMatch { start, end, groups }));
            }
            debug_assert!(trail.is_empty());
            debug_assert!(stack.is_empty());
            debug_assert!(saved.iter().all(|v| v.is_none()));
            s += 1;
        }
    }

    pub fn test(&self, s: &str) -> Result<bool, RegexError> {
        let chars: Vec<char> = s.chars().collect();
        Ok(self.search(&chars, 0)?.is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pattern: &str, flags: &str, s: &str) -> Option<(usize, usize)> {
        let re = Regex::compile(pattern, flags).unwrap();
        let chars: Vec<char> = s.chars().collect();
        re.search(&chars, 0).unwrap().map(|r| (r.start, r.end))
    }

    fn groups(pattern: &str, flags: &str, s: &str) -> Vec<Option<String>> {
        let re = Regex::compile(pattern, flags).unwrap();
        let chars: Vec<char> = s.chars().collect();
        let mat = re.search(&chars, 0).unwrap().unwrap();
        mat.groups
            .iter()
            .map(|g| g.map(|(a, b)| chars[a..b].iter().collect()))
            .collect()
    }

    // ---------------- リテラル・文字クラス ----------------

    #[test]
    fn literal_char_match() {
        assert_eq!(m("abc", "", "xxabcyy"), Some((2, 5)));
    }

    #[test]
    fn dot_matches_any_but_newline() {
        assert_eq!(m(".", "", "a"), Some((0, 1)));
        assert_eq!(m("^.$", "", "\n"), None);
    }

    #[test]
    fn dot_all_flag_matches_newline() {
        assert_eq!(m("^.$", "s", "\n"), Some((0, 1)));
    }

    #[test]
    fn digit_shorthand() {
        assert_eq!(m(r"\d+", "", "ab123cd"), Some((2, 5)));
    }

    #[test]
    fn not_digit_shorthand() {
        assert_eq!(m(r"\D+", "", "12ab34"), Some((2, 4)));
    }

    #[test]
    fn word_shorthand_is_ascii_only() {
        // JS の \w は ASCII のみ（Unicode の英字は含まない）。
        assert_eq!(m(r"\w+", "", "あabあ"), Some((1, 3)));
    }

    #[test]
    fn whitespace_shorthand() {
        assert!(Regex::compile(r"\s", "").unwrap().test(" ").unwrap());
        assert!(Regex::compile(r"\s", "").unwrap().test("\t").unwrap());
        assert!(!Regex::compile(r"\s", "").unwrap().test("a").unwrap());
    }

    #[test]
    fn char_class_range_and_negate() {
        assert_eq!(m("[a-c]+", "", "xxabcxx"), Some((2, 5)));
        assert_eq!(m("[^a-c]+", "", "abcXYZabc"), Some((3, 6)));
    }

    #[test]
    fn char_class_with_shorthands() {
        assert_eq!(m(r"[\d\s]+", "", "ab12 3cd"), Some((2, 6)));
    }

    #[test]
    fn char_class_literal_bracket_close_must_be_escaped() {
        assert_eq!(m(r"[\]]", "", "]"), Some((0, 1)));
    }

    // ---------------- 量指定子 ----------------

    #[test]
    fn star_greedy_takes_maximum() {
        assert_eq!(m("a*", "", "aaab"), Some((0, 3)));
    }

    #[test]
    fn star_lazy_takes_minimum() {
        assert_eq!(m("a*?", "", "aaab"), Some((0, 0)));
    }

    #[test]
    fn plus_requires_at_least_one() {
        assert_eq!(m("a+", "", "bbb"), None);
        assert_eq!(m("a+", "", "aab"), Some((0, 2)));
    }

    #[test]
    fn question_mark_optional() {
        assert_eq!(m("colou?r", "", "color"), Some((0, 5)));
        assert_eq!(m("colou?r", "", "colour"), Some((0, 6)));
    }

    #[test]
    fn brace_exact_count() {
        assert_eq!(m("a{3}", "", "aaaa"), Some((0, 3)));
        assert_eq!(m("a{3}", "", "aa"), None);
    }

    #[test]
    fn brace_min_only() {
        assert_eq!(m("a{2,}", "", "aaaaa"), Some((0, 5)));
    }

    #[test]
    fn brace_min_max() {
        assert_eq!(m("a{2,4}", "", "aaaaaa"), Some((0, 4)));
    }

    #[test]
    fn brace_lazy() {
        assert_eq!(m("a{2,4}?", "", "aaaaaa"), Some((0, 2)));
    }

    #[test]
    fn invalid_brace_range_is_error() {
        assert!(Regex::compile("a{4,2}", "").is_err());
    }

    #[test]
    fn unrecognized_brace_is_literal() {
        // `{` が量指定子として解釈できない場合はリテラル文字として扱う。
        assert_eq!(m(r"a\{foo\}", "", "a{foo}"), Some((0, 6)));
    }

    #[test]
    fn nothing_to_repeat_is_error() {
        assert!(Regex::compile("*abc", "").is_err());
    }

    #[test]
    fn nullable_star_does_not_infinite_loop() {
        // `(a?)*` は空文字列にもマッチでき、無限ループしないことを確認する。
        let re = Regex::compile("(a?)*", "").unwrap();
        let chars: Vec<char> = "".chars().collect();
        let r = re.search(&chars, 0).unwrap().unwrap();
        assert_eq!((r.start, r.end), (0, 0));
    }

    // ---------------- グループ・選択 ----------------

    #[test]
    fn alternation_priority_left_to_right() {
        // 選択は左から順に試し、最初に成功した枝を採用する（最長一致ではない）。
        assert_eq!(m("a|ab", "", "ab"), Some((0, 1)));
    }

    #[test]
    fn non_capturing_group_does_not_capture() {
        let re = Regex::compile("(?:abc)(def)", "").unwrap();
        let chars: Vec<char> = "abcdef".chars().collect();
        let r = re.search(&chars, 0).unwrap().unwrap();
        assert_eq!(r.groups.len(), 1);
    }

    #[test]
    fn nested_group_capture_numbering_is_by_open_paren_order() {
        let g = groups(r"((a)(b))(c)", "", "abc");
        assert_eq!(
            g,
            vec![
                Some("ab".into()),
                Some("a".into()),
                Some("b".into()),
                Some("c".into())
            ]
        );
    }

    #[test]
    fn optional_group_capture_is_undefined_when_not_participating() {
        let g = groups(r"(a)(b)?", "", "a");
        assert_eq!(g, vec![Some("a".into()), None]);
    }

    // ---------------- アンカー・単語境界 ----------------

    #[test]
    fn caret_dollar_without_multiline_match_whole_string_only() {
        assert_eq!(m("^abc$", "", "abc"), Some((0, 3)));
        assert_eq!(m("^abc$", "", "xabc"), None);
        assert_eq!(m("^foo$", "", "foo\n"), None);
    }

    #[test]
    fn caret_dollar_with_multiline_match_line_boundaries() {
        assert_eq!(m("^b", "m", "a\nb"), Some((2, 3)));
        assert_eq!(m("a$", "m", "a\nb"), Some((0, 1)));
    }

    #[test]
    fn word_boundary_basic() {
        assert_eq!(m(r"\bcat\b", "", "a cat sat"), Some((2, 5)));
        assert_eq!(m(r"\bcat\b", "", "concatenate"), None);
    }

    #[test]
    fn non_word_boundary_basic() {
        assert_eq!(m(r"\Bcat", "", "concatenate"), Some((3, 6)));
    }

    // ---------------- 後方参照 ----------------

    #[test]
    fn backreference_matches_same_text() {
        assert_eq!(m(r"(\w+) \1", "", "hello hello"), Some((0, 11)));
        assert_eq!(m(r"(\w+) \1", "", "hello world"), None);
    }

    #[test]
    fn backreference_to_unmatched_group_is_empty() {
        // JS 仕様: 未マッチのグループへの後方参照は空文字列にマッチする。
        assert_eq!(m(r"(a)?\1b", "", "b"), Some((0, 1)));
    }

    #[test]
    fn invalid_backreference_number_is_compile_error() {
        assert!(Regex::compile(r"\1", "").is_err());
    }

    // ---------------- 先読み ----------------

    #[test]
    fn positive_lookahead() {
        assert_eq!(m(r"foo(?=bar)", "", "foobar"), Some((0, 3)));
        assert_eq!(m(r"foo(?=bar)", "", "foobaz"), None);
    }

    #[test]
    fn negative_lookahead() {
        assert_eq!(m(r"foo(?!bar)", "", "foobaz"), Some((0, 3)));
        assert_eq!(m(r"foo(?!bar)", "", "foobar"), None);
    }

    #[test]
    fn lookbehind_is_unsupported_error() {
        assert!(Regex::compile(r"(?<=foo)bar", "").is_err());
        assert!(Regex::compile(r"(?<!foo)bar", "").is_err());
    }

    #[test]
    fn named_group_is_unsupported_error() {
        assert!(Regex::compile(r"(?<name>foo)", "").is_err());
    }

    // ---------------- フラグ ----------------

    #[test]
    fn ignore_case_flag() {
        assert_eq!(m("abc", "i", "ABC"), Some((0, 3)));
    }

    #[test]
    fn unsupported_flag_is_error() {
        assert!(Regex::compile("a", "y").is_err());
        assert!(Regex::compile("a", "d").is_err());
    }

    #[test]
    fn duplicate_flag_is_error() {
        assert!(Regex::compile("a", "gg").is_err());
    }

    // ---------------- 元コーパスの正規表現 ----------------

    #[test]
    fn corpus_line_ending_alternation() {
        let re = Regex::compile(r"\r\n|\n|\r", "").unwrap();
        assert!(re.test("a\r\nb").unwrap());
    }

    #[test]
    fn corpus_leading_whitespace_test() {
        let re = Regex::compile(r"\s", "").unwrap();
        assert!(re.test(" ").unwrap());
        assert!(!re.test("x").unwrap());
    }

    #[test]
    fn corpus_scenario_prefix_replace_pattern() {
        assert_eq!(m(r"^scenario/", "", "scenario/foo.ks"), Some((0, 9)));
    }

    #[test]
    fn corpus_label_star_prefix_pattern() {
        assert_eq!(m(r"^\*", "", "*start"), Some((0, 1)));
        assert_eq!(m(r"^\*", "", "start"), None);
    }

    // ---------------- ステップ上限・アンドゥログ・スタック保護 ----------------

    #[test]
    fn catastrophic_backtracking_hits_step_limit() {
        // 典型的な「壊滅的バックトラック」パターン。無限に固まらず、
        // 明確なエラーを返すことを確認する。
        let re = Regex::compile(r"^(a+)+$", "").unwrap();
        let s = "a".repeat(35) + "!";
        let chars: Vec<char> = s.chars().collect();
        let err = re.search(&chars, 0).unwrap_err();
        assert!(err.0.contains("ステップ数") || err.0.contains("再帰の深さ"));
    }

    #[test]
    fn redos_unanchored_nested_plus_completes_fast_without_hang_or_overflow() {
        let re = Regex::compile(r"(a+)+b", "").unwrap();
        let s = "a".repeat(30) + "c";
        let chars: Vec<char> = s.chars().collect();
        let start = std::time::Instant::now();
        let err = re.search(&chars, 0).unwrap_err();
        assert!(err.0.contains("ステップ数") || err.0.contains("再帰の深さ"));
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "ReDoS 検出に時間がかかりすぎています: {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn deep_repetition_does_not_stack_overflow() {
        // 明示スタック＋反復ループにより、深い繰り返しでも OS スタックオーバーフローしない。
        let s = "a".repeat(5000);
        assert_eq!(m(r"^a{5000}$", "", &s), Some((0, 5000)));
        assert_eq!(m(r"^a{0,5000}$", "", &s), Some((0, 5000)));
        let s10k = "a".repeat(10_000);
        assert_eq!(m(r"^a+$", "", &s10k), Some((0, 10_000)));
    }

    #[test]
    fn backtracking_rolls_back_captures_from_failed_branch() {
        // 第1枝でグループ 1, 2 をキャプチャしてから失敗 → 第2枝へバックトラックした際、
        // グループ 1, 2 が確実に None に巻き戻されていること。
        assert_eq!(
            groups(r"^(?:(a)(b)c|(a)bd)$", "", "abd"),
            vec![None, None, Some("a".into())]
        );
        // ネストした量指定子＋キャプチャのバックトラック
        assert_eq!(
            groups(r"^(a+)(a+)b$", "", "aaaab"),
            vec![Some("aaa".into()), Some("a".into())]
        );
        assert_eq!(
            groups(r"^(a|ab)+(c)$", "", "ababc"),
            vec![Some("ab".into()), Some("c".into())]
        );
    }

    #[test]
    fn backreference_with_backtracking_and_trail_rewind() {
        // `(\w+)` が貪欲に "abab" を取った後 `\1` で失敗し、"ab" まで巻き戻って成功する。
        assert_eq!(groups(r"^(\w+)\1$", "", "abab"), vec![Some("ab".into())]);
        assert_eq!(m(r"^(\w+)\1$", "", "abac"), None);
    }

    #[test]
    fn positive_lookahead_commits_captures_and_rolls_back_on_outer_backtrack() {
        // JS 仕様: 肯定先読み内のキャプチャは後続の後方参照や結果に保持される。
        assert_eq!(groups(r"(?=(a+))\1b", "", "aaab"), vec![Some("aaa".into())]);
        // 肯定先読みが一度成功してキャプチャを確定しても、後続が失敗して外側の枝へ
        // バックトラックした場合は、先読み内のキャプチャも巻き戻されること。
        assert_eq!(
            groups(r"^(?:(?=(a))x|(?=(ab))ab)$", "", "ab"),
            vec![None, Some("ab".into())]
        );
        // ネストした肯定先読みのキャプチャ保持
        assert_eq!(
            groups(r"^(?=(?=(ab))a)ab$", "", "ab"),
            vec![Some("ab".into())]
        );
    }

    #[test]
    fn negative_lookahead_rolls_back_captures_on_both_success_and_failure() {
        // 否定先読みが成功（内部が不成立）した場合、内部で途中まで書いたキャプチャは巻き戻される。
        assert_eq!(
            groups(r"^(?!(a)x)(a)y$", "", "ay"),
            vec![None, Some("a".into())]
        );
        // 否定先読みが失敗（内部がマッチ）して外側の別枝へバックトラックした場合も、
        // 否定先読み内でマッチしたキャプチャは確実に巻き戻される。
        assert_eq!(groups(r"^(?:(?!(a))x|a)$", "", "a"), vec![None]);
    }

    #[test]
    fn deeply_nested_lookahead_hits_depth_limit_cleanly() {
        let depth = MAX_LOOK_DEPTH + 10;
        let pat = format!("{}{}{}", "(?=".repeat(depth), "a", ")".repeat(depth));
        let re = Regex::compile(&pat, "").unwrap();
        let chars: Vec<char> = "a".chars().collect();
        let err = re.search(&chars, 0).unwrap_err();
        assert!(err.0.contains("再帰の深さ"));
    }
}
