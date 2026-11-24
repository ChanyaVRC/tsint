//! 正規表現実行器（明示スタック＋アンドゥログによるバックトラッキング）。

use super::charclass::{char_eq, is_line_terminator, is_word};
use super::compiler::Inst;
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
    chars: &'a [char],
    ignore_case: bool,
    multiline: bool,
    dot_all: bool,
    steps: u64,
    max_steps: u64,
    max_depth: usize,
}

impl<'a> Exec<'a> {
    pub(super) fn new(
        prog: &'a [Inst],
        chars: &'a [char],
        ignore_case: bool,
        multiline: bool,
        dot_all: bool,
        max_steps: u64,
        max_depth: usize,
    ) -> Self {
        Exec {
            prog,
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
                "正規表現の実行ステップ数上限を超えました（壊滅的バックトラックの可能性があります）"
                    .to_string(),
            ));
        }
        Ok(())
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
                        pc = after_pc;
                        true
                    } else {
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
