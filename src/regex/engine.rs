//! 正規表現実行器（明示スタック＋アンドゥログによるバックトラッキング）。

use super::charclass::{char_eq, is_line_terminator, is_word, CharClass};
use super::compiler::{AtomMatcher, Inst};
use super::parser::{LookaheadKind, WordBoundary};
use super::RegexError;

/// 壊滅的バックトラック対策のデフォルト上限。超過時は `RegexError` を返す。
pub(super) const DEFAULT_MAX_STEPS: u64 = 2_000_000;
/// ヒープ上のバックトラックスタック深さの上限（過大なメモリ確保を防ぐ）。
pub(super) const DEFAULT_MAX_DEPTH: usize = 50_000;
/// 先読み `(?=...)` / `(?!...)` のネスト上限（OS コールスタック保護）。
pub(super) const MAX_LOOK_DEPTH: usize = 256;

pub(super) type Saved = Vec<Option<usize>>;
pub(super) type Trail = Vec<(usize, Option<usize>)>;

#[derive(Clone, Copy)]
pub(super) struct BacktrackFrame {
    pc: usize,
    sp: usize,
    trail_mark: usize,
    /// `GreedyAtomRepeat` のバックトラック下限（通常の `Split` は `usize::MAX`）。
    repeat_min_sp: usize,
}

#[inline]
fn rewind_trail(saved: &mut [Option<usize>], trail: &mut Trail, mark: usize) {
    while trail.len() > mark {
        let (slot, prev) = trail.pop().unwrap();
        saved[slot] = prev;
    }
}

pub(super) struct Exec<'a> {
    prog: &'a [Inst],
    classes: &'a [CharClass],
    chars: &'a [char],
    ignore_case: bool,
    multiline: bool,
    dot_all: bool,
    steps: u64,
    max_steps: u64,
    max_depth: usize,
}

impl<'a> Exec<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        prog: &'a [Inst],
        classes: &'a [CharClass],
        chars: &'a [char],
        ignore_case: bool,
        multiline: bool,
        dot_all: bool,
        max_steps: u64,
        max_depth: usize,
    ) -> Self {
        Exec {
            prog,
            classes,
            chars,
            ignore_case,
            multiline,
            dot_all,
            steps: 0,
            max_steps,
            max_depth,
        }
    }

    #[inline]
    fn step(&mut self) -> Result<(), RegexError> {
        self.steps += 1;
        if self.steps > self.max_steps {
            return Err(RegexError(
                "正規表現の実行ステップ数上限を超えました（入力またはバックトラック量が大きすぎます）"
                    .to_string(),
            ));
        }
        Ok(())
    }

    #[inline]
    fn charge_steps(&mut self, count: u64) -> Result<(), RegexError> {
        self.steps = self.steps.saturating_add(count);
        if self.steps > self.max_steps {
            return Err(RegexError(
                "正規表現の実行ステップ数上限を超えました（入力またはバックトラック量が大きすぎます）"
                    .to_string(),
            ));
        }
        Ok(())
    }

    #[inline]
    fn match_atom(&self, atom: AtomMatcher, ch: char) -> bool {
        match atom {
            AtomMatcher::Char(c) => char_eq(ch, c, self.ignore_case),
            AtomMatcher::Any => self.dot_all || !is_line_terminator(ch),
            AtomMatcher::Class(idx) => self.classes[idx as usize].matches(ch, self.ignore_case),
        }
    }

    pub(super) fn run_from(
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
            let ok = match self.prog[pc] {
                Inst::Char(c) => {
                    if sp < self.chars.len() && char_eq(self.chars[sp], c, self.ignore_case) {
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
                Inst::Class(idx) => {
                    if sp < self.chars.len()
                        && self.classes[idx as usize].matches(self.chars[sp], self.ignore_case)
                    {
                        pc += 1;
                        sp += 1;
                        true
                    } else {
                        false
                    }
                }
                Inst::GreedyAtomRepeat { atom, min } => {
                    let mut cur = sp;
                    while cur < self.chars.len() && self.match_atom(atom, self.chars[cur]) {
                        cur += 1;
                    }
                    let consumed = cur - sp;
                    if consumed > 0 {
                        self.charge_steps(consumed as u64)?;
                    }
                    let min_count = min as usize;
                    if consumed < min_count {
                        false
                    } else {
                        let min_sp = sp + min_count;
                        if cur > min_sp {
                            if stack.len() >= self.max_depth {
                                return Err(RegexError(
                                    "正規表現の再帰の深さ上限を超えました（パターンまたは入力が大きすぎます）"
                                        .to_string(),
                                ));
                            }
                            stack.push(BacktrackFrame {
                                pc: pc + 1,
                                sp: cur - 1,
                                trail_mark: trail.len(),
                                repeat_min_sp: min_sp,
                            });
                        }
                        sp = cur;
                        pc += 1;
                        true
                    }
                }
                Inst::Save(slot) => {
                    let slot = slot as usize;
                    let prev = saved[slot];
                    if prev != Some(sp) {
                        trail.push((slot, prev));
                        saved[slot] = Some(sp);
                    }
                    pc += 1;
                    true
                }
                Inst::Jmp(t) => {
                    pc = t as usize;
                    true
                }
                Inst::Split(a, b) => {
                    if stack.len() >= self.max_depth {
                        return Err(RegexError(
                            "正規表現の再帰の深さ上限を超えました（パターンまたは入力が大きすぎます）"
                                .to_string(),
                        ));
                    }
                    stack.push(BacktrackFrame {
                        pc: b as usize,
                        sp,
                        trail_mark: trail.len(),
                        repeat_min_sp: usize::MAX,
                    });
                    pc = a as usize;
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
                Inst::WordB(wb) => {
                    let before = sp > 0 && is_word(self.chars[sp - 1]);
                    let after = sp < self.chars.len() && is_word(self.chars[sp]);
                    let want_boundary = wb == WordBoundary::Match;
                    if (before != after) == want_boundary {
                        pc += 1;
                        true
                    } else {
                        false
                    }
                }
                Inst::Backref(n) => {
                    let slot = (n as usize) * 2;
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
                    kind,
                } => {
                    let look_trail_mark = trail.len();
                    let matched = self
                        .run_from(sub_pc as usize, sp, saved, trail, stack, look_depth + 1)?
                        .is_some();
                    let ok = match kind {
                        LookaheadKind::Positive => matched,
                        LookaheadKind::Negative => !matched,
                    };
                    if ok {
                        pc = after_pc as usize;
                        true
                    } else {
                        rewind_trail(saved, trail, look_trail_mark);
                        false
                    }
                }
                Inst::LoopBack { mark_slot, loop_pc } => {
                    let progressed = saved.get(mark_slot as usize).copied().flatten() != Some(sp);
                    if progressed {
                        pc = loop_pc as usize;
                    } else {
                        pc += 1;
                    }
                    true
                }
            };

            if !ok {
                if stack.len() > stack_base {
                    let mut frame = stack.pop().unwrap();
                    rewind_trail(saved, trail, frame.trail_mark);
                    pc = frame.pc;
                    sp = frame.sp;
                    if frame.repeat_min_sp != usize::MAX && frame.sp > frame.repeat_min_sp {
                        frame.sp -= 1;
                        stack.push(frame);
                    }
                } else {
                    rewind_trail(saved, trail, base_trail_mark);
                    return Ok(None);
                }
            }
        }
    }
}
