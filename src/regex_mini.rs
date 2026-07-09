//! 最小限の正規表現サブセット。
//!
//! 完全な正規表現エンジンは T4 で実装する。T2 の時点ではコーパス
//! （`runtime/kag/parser.ts`）が `.test()` と `String.split()` に正規表現
//! リテラルを使っている（`/\s/.test(ch)` や `source.split(/\r\n|\n|\r/)`）
//! ため、それを動かすのに必要な最小サブセットだけをサポートする:
//!
//! - リテラル文字（エスケープされていない大半の文字）
//! - 文字クラスショートハンド `\s` `\S` `\d` `\D` `\w` `\W`
//! - 文字クラス `[...]`（否定 `[^...]`・範囲 `a-z`・上記ショートハンド込み）
//! - 代替 `|`（トップレベルのみ。グループ `()` は非対応）
//! - `.`（任意の 1 文字）
//!
//! 上記以外の構文（量指定子 `* + ? {}`、グループ、アンカー等）は
//! 非対応であり、コンパイル時に明確なエラーを返す。
//! `test`/`split` は非アンカーの「部分一致検索」として動作する
//! （実際の JS の `RegExp.prototype.test` と同じ）。

#[derive(Debug, Clone)]
enum ClassItem {
    Char(char),
    Range(char, char),
    Digit,
    Word,
    Whitespace,
}

impl ClassItem {
    fn matches(&self, c: char) -> bool {
        match self {
            ClassItem::Char(x) => *x == c,
            ClassItem::Range(a, b) => *a <= c && c <= *b,
            ClassItem::Digit => c.is_ascii_digit(),
            ClassItem::Word => c.is_alphanumeric() || c == '_',
            ClassItem::Whitespace => c.is_whitespace(),
        }
    }
}

#[derive(Debug, Clone)]
enum Atom {
    Char(char),
    AnyChar,
    Whitespace(bool),            // \s (true) / \S (false)
    Digit(bool),                 // \d / \D
    Word(bool),                  // \w / \W
    Class(bool, Vec<ClassItem>), // [...] / [^...]（bool は negate）
}

impl Atom {
    fn matches(&self, c: char) -> bool {
        match self {
            Atom::Char(x) => *x == c,
            Atom::AnyChar => c != '\n',
            Atom::Whitespace(want) => c.is_whitespace() == *want,
            Atom::Digit(want) => c.is_ascii_digit() == *want,
            Atom::Word(want) => (c.is_alphanumeric() || c == '_') == *want,
            Atom::Class(negate, items) => items.iter().any(|it| it.matches(c)) != *negate,
        }
    }
}

pub struct MiniRegex {
    pub source: String,
    pub flags: String,
    alternatives: Vec<Vec<Atom>>,
}

#[derive(Debug)]
pub struct RegexError(pub String);

impl MiniRegex {
    pub fn compile(pattern: &str, flags: &str) -> Result<MiniRegex, RegexError> {
        let mut alternatives = Vec::new();
        for branch in pattern.split('|') {
            alternatives.push(parse_seq(branch)?);
        }
        Ok(MiniRegex {
            source: pattern.to_string(),
            flags: flags.to_string(),
            alternatives,
        })
    }

    /// 非アンカーの部分一致検索。マッチした場合 (開始, 終了) を文字インデックスで返す。
    pub fn search(&self, chars: &[char], from: usize) -> Option<(usize, usize)> {
        for start in from..=chars.len() {
            for seq in &self.alternatives {
                if let Some(end) = match_seq_at(seq, chars, start) {
                    return Some((start, end));
                }
            }
        }
        None
    }

    pub fn test(&self, s: &str) -> bool {
        let chars: Vec<char> = s.chars().collect();
        self.search(&chars, 0).is_some()
    }

    /// `String.prototype.split(regex)` 用: マッチ箇所で分割した断片を返す。
    pub fn split(&self, s: &str) -> Vec<String> {
        let chars: Vec<char> = s.chars().collect();
        let mut out = Vec::new();
        let mut pos = 0usize;
        let mut search_from = 0usize;
        loop {
            match self.search(&chars, search_from) {
                Some((start, end)) if start < chars.len() => {
                    // ゼロ幅マッチはコーパスで使わないため単純に 1 文字分進めて無限ループを回避する。
                    let end = if end == start { start + 1 } else { end };
                    out.push(chars[pos..start].iter().collect());
                    pos = end;
                    search_from = end;
                }
                _ => break,
            }
        }
        out.push(chars[pos..].iter().collect());
        out
    }
}

fn match_seq_at(seq: &[Atom], chars: &[char], start: usize) -> Option<usize> {
    if start + seq.len() > chars.len() {
        return None;
    }
    for (i, atom) in seq.iter().enumerate() {
        if !atom.matches(chars[start + i]) {
            return None;
        }
    }
    Some(start + seq.len())
}

fn parse_seq(branch: &str) -> Result<Vec<Atom>, RegexError> {
    let mut atoms = Vec::new();
    let mut it = branch.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\\' => {
                let esc = it.next().ok_or_else(|| {
                    RegexError("末尾がバックスラッシュで終わっています".to_string())
                })?;
                let atom = match esc {
                    's' => Atom::Whitespace(true),
                    'S' => Atom::Whitespace(false),
                    'd' => Atom::Digit(true),
                    'D' => Atom::Digit(false),
                    'w' => Atom::Word(true),
                    'W' => Atom::Word(false),
                    'n' => Atom::Char('\n'),
                    'r' => Atom::Char('\r'),
                    't' => Atom::Char('\t'),
                    other => Atom::Char(other),
                };
                atoms.push(atom);
            }
            '.' => atoms.push(Atom::AnyChar),
            '[' => atoms.push(parse_class(&mut it)?),
            '^' | '$' | '(' | ')' | ']' | '*' | '+' | '?' | '{' | '}' => {
                return Err(RegexError(format!(
                    "tsint T2 の最小正規表現サブセットは '{c}' に対応していません（T4 で本実装予定）"
                )));
            }
            other => atoms.push(Atom::Char(other)),
        }
    }
    Ok(atoms)
}

fn parse_class(it: &mut std::iter::Peekable<std::str::Chars>) -> Result<Atom, RegexError> {
    let negate = if it.peek() == Some(&'^') {
        it.next();
        true
    } else {
        false
    };
    let mut items = Vec::new();
    loop {
        let c = it
            .next()
            .ok_or_else(|| RegexError("文字クラス '[' が閉じられていません".to_string()))?;
        if c == ']' {
            break;
        }
        let start_char = if c == '\\' {
            let esc = it
                .next()
                .ok_or_else(|| RegexError("末尾がバックスラッシュで終わっています".to_string()))?;
            match esc {
                's' => {
                    items.push(ClassItem::Whitespace);
                    continue;
                }
                'S' => {
                    // 否定ショートハンドはクラス内では非対応（コーパス未使用）。
                    return Err(RegexError("文字クラス内の \\S は未対応です".to_string()));
                }
                'd' => {
                    items.push(ClassItem::Digit);
                    continue;
                }
                'w' => {
                    items.push(ClassItem::Word);
                    continue;
                }
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                other => other,
            }
        } else {
            c
        };
        // 範囲 `a-z` の判定（次が '-' かつその次が ']' でない場合）。
        if it.peek() == Some(&'-') {
            let mut clone = it.clone();
            clone.next();
            if let Some(&after) = clone.peek() {
                if after != ']' {
                    it.next(); // '-' を消費
                    let end_char = it.next().unwrap();
                    items.push(ClassItem::Range(start_char, end_char));
                    continue;
                }
            }
        }
        items.push(ClassItem::Char(start_char));
    }
    Ok(Atom::Class(negate, items))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitespace_class_matches_single_char() {
        let re = MiniRegex::compile(r"\s", "").unwrap();
        assert!(re.test(" "));
        assert!(re.test("\t"));
        assert!(!re.test("a"));
    }

    #[test]
    fn alternation_split_line_endings() {
        let re = MiniRegex::compile(r"\r\n|\n|\r", "").unwrap();
        let parts = re.split("a\r\nb\nc\rd");
        assert_eq!(parts, vec!["a", "b", "c", "d"]);
    }
}
