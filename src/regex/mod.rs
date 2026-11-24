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
//! ## サブモジュール構成
//!
//! - `charclass`: 文字クラスと 128-bit ASCII ビットマップ
//! - `parser`: 正規表現パターンの AST と再帰下降パーサ
//! - `compiler`: AST → フラットな命令列 (`Inst`) コンパイラ
//! - `engine`: 明示バックトラックスタック＋アンドゥログ実行器 (`Exec`)
//!
//! ## Unicode / UTF-16 に関する既知の差異
//!
//! 本エンジンは Rust の `char`（Unicode スカラ値）単位で動作する。JS の
//! 文字列は内部的に UTF-16 コード単位の列であり、サロゲートペア
//! （絵文字等の追加面文字）は `u` フラグ無しでは 2 つの別々の「文字」として
//! 扱われる。本エンジンは `u` フラグの有無に関わらず常にコードポイント
//! 単位で動作するため、サロゲートペアの挙動は JS の非 `u` モードとは異なる
//! （`u` モードの挙動に近い）。`.length` や `charCodeAt` 等、文字列側の
//! UTF-16 セマンティクスは `builtins` 側の既存実装に準拠する。

mod charclass;
mod compiler;
mod engine;
mod parser;
#[cfg(test)]
mod tests;

use charclass::CharClass;
use compiler::{Compiler, Inst};
use engine::{BacktrackFrame, Exec, Saved, Trail, DEFAULT_MAX_DEPTH, DEFAULT_MAX_STEPS};
use parser::{count_capturing_groups, Parser};

/// 正規表現のコンパイルまたは実行時に発生するエラー。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegexError(pub String);

impl std::fmt::Display for RegexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RegexError {}

/// マッチ結果。位置はすべて文字（Unicode スカラ値）インデックス。
#[derive(Debug, Clone)]
pub struct RegexMatch {
    pub start: usize,
    pub end: usize,
    /// 1 番から順に、キャプチャグループの (開始, 終了)。未参加のグループは
    /// `None`（JS の `undefined` に対応）。
    pub groups: Vec<Option<(usize, usize)>>,
}

/// 正規表現実行時の再利用可能バッファ（`saved` / `trail` / `stack`）。
/// `match`/`replace`/`split` のような繰り返し検索で毎回のヒープ確保を避けるために使う。
#[derive(Default)]
pub(crate) struct RegexScratch {
    saved: Saved,
    trail: Trail,
    stack: Vec<BacktrackFrame>,
}

/// 検索開始位置の高速フィルタ（先頭 `Save` 列の直後にある最初の消費/アンカー命令）。
#[derive(Clone)]
enum StartFilter {
    None,
    /// `!multiline` かつ先頭が `^`（`s == 0` でのみマッチ可能）。
    AnchoredStart,
    /// `!ignore_case` かつ先頭が単一リテラル文字。
    FirstChar(char),
    /// 先頭が文字クラス（ASCII ビットマップで O(1) スキップ）。
    FirstClass(CharClass),
}

/// コンパイル済み正規表現。
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
    start_filter: StartFilter,
}

impl Regex {
    /// パターン文字列とフラグ（`g` / `i` / `m` / `s` / `u`）から正規表現をコンパイルする。
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
        let mut p = Parser::new(chars, total_captures);
        let ast = p.parse_pattern()?;
        let ncaptures = p.ncaptures;
        let mut c = Compiler::new(2 * (ncaptures + 1));
        c.emit(Inst::Save(0));
        c.compile(&ast);
        c.emit(Inst::Save(1));
        c.emit(Inst::Match);
        for inst in &mut c.prog {
            if let Inst::Class(cc) = inst {
                cc.compile_bitmap(ignore_case);
            }
        }
        let start_filter = derive_start_filter(&c.prog, multiline, ignore_case);
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
            start_filter,
        })
    }

    /// キャプチャグループ数を返す。
    pub fn ncaptures(&self) -> usize {
        self.ncaptures
    }

    /// `from`（文字インデックス）以降の最初の一致を探す（非アンカーの部分
    /// 一致検索。実際の JS の `RegExp.prototype.exec` の挙動に相当）。
    pub fn search(&self, chars: &[char], from: usize) -> Result<Option<RegexMatch>, RegexError> {
        let mut scratch = RegexScratch::default();
        self.search_with_scratch(chars, from, &mut scratch)
    }

    pub(crate) fn search_with_scratch(
        &self,
        chars: &[char],
        from: usize,
        scratch: &mut RegexScratch,
    ) -> Result<Option<RegexMatch>, RegexError> {
        if matches!(self.start_filter, StartFilter::AnchoredStart) && from > 0 {
            return Ok(None);
        }
        let nslots = self.total_slots;
        let mut exec = Exec::new(
            &self.prog,
            chars,
            self.ignore_case,
            self.multiline,
            self.dot_all,
            DEFAULT_MAX_STEPS,
            DEFAULT_MAX_DEPTH,
        );
        scratch.saved.clear();
        scratch.saved.resize(nslots, None);
        scratch.trail.clear();
        scratch.stack.clear();
        let mut s = from;
        loop {
            if s > chars.len() {
                return Ok(None);
            }
            match &self.start_filter {
                StartFilter::None => {}
                StartFilter::AnchoredStart => {
                    if s > 0 {
                        return Ok(None);
                    }
                }
                StartFilter::FirstChar(target) => {
                    while s < chars.len() && chars[s] != *target {
                        s += 1;
                    }
                    if s >= chars.len() {
                        return Ok(None);
                    }
                }
                StartFilter::FirstClass(cc) => {
                    while s < chars.len() && !cc.matches(chars[s], self.ignore_case) {
                        s += 1;
                    }
                    if s >= chars.len() {
                        return Ok(None);
                    }
                }
            }
            if exec
                .run_from(
                    0,
                    s,
                    &mut scratch.saved,
                    &mut scratch.trail,
                    &mut scratch.stack,
                    0,
                )?
                .is_some()
            {
                let saved = &scratch.saved;
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
            debug_assert!(scratch.trail.is_empty());
            debug_assert!(scratch.stack.is_empty());
            debug_assert!(scratch.saved.iter().all(|v| v.is_none()));
            if matches!(self.start_filter, StartFilter::AnchoredStart) {
                return Ok(None);
            }
            s += 1;
        }
    }

    /// 文字列 `s` のいずれかの位置にマッチするかを判定する。
    pub fn test(&self, s: &str) -> Result<bool, RegexError> {
        let chars: Vec<char> = s.chars().collect();
        Ok(self.search(&chars, 0)?.is_some())
    }
}

fn derive_start_filter(prog: &[Inst], multiline: bool, ignore_case: bool) -> StartFilter {
    let mut idx = 0usize;
    while matches!(prog.get(idx), Some(Inst::Save(_))) {
        idx += 1;
    }
    match prog.get(idx) {
        Some(Inst::Bol) if !multiline => StartFilter::AnchoredStart,
        Some(Inst::Char(ch)) if !ignore_case => StartFilter::FirstChar(*ch),
        Some(Inst::Class(cc)) => StartFilter::FirstClass(cc.clone()),
        _ => StartFilter::None,
    }
}
