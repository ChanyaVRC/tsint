//! AST → フラットな命令列 (`Inst`) コンパイラ。

use super::charclass::CharClass;
use super::parser::Ast;

#[derive(Debug, Clone)]
pub(super) enum Inst {
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
    /// へ進む。
    Look {
        sub_pc: usize,
        after_pc: usize,
        negate: bool,
    },
    /// 無制限（`*`/`+`/`{n,}`）ループの「もう1周するか、抜けるか」の判定。
    LoopBack {
        mark_slot: usize,
        loop_pc: usize,
    },
}

pub(super) struct Compiler {
    pub(super) prog: Vec<Inst>,
    /// キャプチャ用スロット数（`2*(ncaptures+1)`）。スクラッチスロットは
    /// この直後の領域に割り当てるので、衝突しないようにここで把握しておく。
    capture_slots: usize,
    /// 量指定子の「進捗チェック」に使うスクラッチスロットの個数。
    pub(super) scratch_count: usize,
}

impl Compiler {
    pub(super) fn new(capture_slots: usize) -> Self {
        Compiler {
            prog: Vec::new(),
            capture_slots,
            scratch_count: 0,
        }
    }

    pub(super) fn emit(&mut self, i: Inst) -> usize {
        self.prog.push(i);
        self.prog.len() - 1
    }

    fn here(&self) -> usize {
        self.prog.len()
    }

    /// キャプチャ用スロットと重ならない、量指定子専用のスクラッチスロットを 1 つ確保する。
    fn alloc_scratch(&mut self) -> usize {
        let s = self.capture_slots + self.scratch_count;
        self.scratch_count += 1;
        s
    }

    fn patch_split(&mut self, split_pc: usize, body_start: usize, after: usize, greedy: bool) {
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

    pub(super) fn compile(&mut self, ast: &Ast) {
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

    /// 量指定子本体を地続きの命令列としてインライン展開する。
    fn compile_repeat(&mut self, inner: &Ast, min: u32, max: Option<u32>, greedy: bool) {
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

    /// 「あってもなくても良い」コピーを `remaining` 個、入れ子の `Split` で展開する。
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
            self.patch_split(split_pc, body_start, after, greedy);
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
        self.patch_split(split_pc, body_start, after, greedy);
    }
}
