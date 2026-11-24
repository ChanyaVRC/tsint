//! 正規表現の文字クラスと 128-bit ASCII ビットマップ。

#[derive(Debug, Clone)]
pub(super) enum ClassAtom {
    Char(char),
    Range(char, char),
    Digit,
    NotDigit,
    Word,
    NotWord,
    Space,
    NotSpace,
}

pub(super) fn is_digit(c: char) -> bool {
    c.is_ascii_digit()
}

/// JS の `\w` は Unicode 全体ではなく ASCII の `[A-Za-z0-9_]` のみ
/// （`u` フラグを付けても変わらない）。
pub(super) fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// JS の `\s` は ASCII 空白に加え Unicode の空白カテゴリ・BOM(U+FEFF) を含む。
/// Rust の `char::is_whitespace()`（Unicode `White_Space` プロパティ）に
/// BOM を足すことで近似する（既知の差異として README に明記）。
pub(super) fn is_space(c: char) -> bool {
    c.is_whitespace() || c == '\u{FEFF}'
}

pub(super) fn is_line_terminator(c: char) -> bool {
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
#[inline]
pub(super) fn char_eq(a: char, b: char, ignore_case: bool) -> bool {
    if a == b {
        return true;
    }
    if !ignore_case {
        return false;
    }
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

#[inline]
fn set_ascii_bit(bits: &mut [u64; 2], b: u8, ignore_case: bool) {
    bits[(b >> 6) as usize] |= 1u64 << (b & 63);
    if ignore_case {
        if b.is_ascii_lowercase() {
            let u = b - 32;
            bits[(u >> 6) as usize] |= 1u64 << (u & 63);
        } else if b.is_ascii_uppercase() {
            let l = b + 32;
            bits[(l >> 6) as usize] |= 1u64 << (l & 63);
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct CharClass {
    /// 128 個の ASCII 文字（0..128）に対する事前計算ビットマップ（`negate`/`ignore_case` 反映済み）。
    ascii_bits: [u64; 2],
    /// 非 ASCII 文字マッチ用アトム（ASCII のみで構成されるクラスはコンパイル後に空にして解放）。
    atoms: Box<[ClassAtom]>,
    negate: bool,
    /// 非 ASCII 文字に対して `atoms` を実際に走査する必要があるか。
    has_non_ascii_atoms: bool,
}

impl CharClass {
    pub(super) fn new(negate: bool, atoms: Vec<ClassAtom>) -> Self {
        CharClass {
            ascii_bits: [0; 2],
            atoms: atoms.into_boxed_slice(),
            negate,
            has_non_ascii_atoms: true,
        }
    }

    pub(super) fn compile_bitmap(&mut self, ignore_case: bool) {
        const DIGIT_LO: u64 = 0x03ff_0000_0000_0000; // '0'..='9' (48..=57)
        const WORD_LO: u64 = DIGIT_LO;
        const WORD_HI: u64 = 0x07ff_fffe_87ff_fffe; // 'A'..='Z' (65..=90), '_' (95), 'a'..='z' (97..=122)
        const SPACE_LO: u64 =
            (1u64 << 9) | (1u64 << 10) | (1u64 << 11) | (1u64 << 12) | (1u64 << 13) | (1u64 << 32);

        let mut bits = [0u64; 2];
        let mut has_non_ascii = false;
        for a in &self.atoms {
            match a {
                ClassAtom::Char(x) => {
                    let code = *x as u32;
                    if code < 128 {
                        set_ascii_bit(&mut bits, code as u8, ignore_case);
                    } else {
                        has_non_ascii = true;
                        if ignore_case {
                            // 例: K (U+212A Kelvin sign) が ASCII 'k' と一致するケースへの保険
                            if let Some(lo) = x.to_lowercase().next() {
                                if (lo as u32) < 128 {
                                    set_ascii_bit(&mut bits, lo as u8, true);
                                }
                            }
                        }
                    }
                }
                ClassAtom::Range(lo, hi) => {
                    let lo_u = *lo as u32;
                    let hi_u = *hi as u32;
                    if hi_u >= 128 {
                        has_non_ascii = true;
                    }
                    if lo_u < 128 {
                        let end = hi_u.min(127) as u8;
                        for b in (lo_u as u8)..=end {
                            set_ascii_bit(&mut bits, b, ignore_case);
                        }
                    }
                }
                ClassAtom::Digit => bits[0] |= DIGIT_LO,
                ClassAtom::NotDigit => {
                    bits[0] |= !DIGIT_LO;
                    bits[1] |= !0u64;
                    has_non_ascii = true;
                }
                ClassAtom::Word => {
                    bits[0] |= WORD_LO;
                    bits[1] |= WORD_HI;
                }
                ClassAtom::NotWord => {
                    bits[0] |= !WORD_LO;
                    bits[1] |= !WORD_HI;
                    has_non_ascii = true;
                }
                ClassAtom::Space => {
                    bits[0] |= SPACE_LO;
                    has_non_ascii = true;
                }
                ClassAtom::NotSpace => {
                    bits[0] |= !SPACE_LO;
                    bits[1] |= !0u64;
                    has_non_ascii = true;
                }
            }
        }
        if self.negate {
            bits[0] = !bits[0];
            bits[1] = !bits[1];
        }
        self.ascii_bits = bits;
        self.has_non_ascii_atoms = has_non_ascii;
        if !has_non_ascii {
            self.atoms = Box::new([]);
        }
    }

    #[inline]
    pub(super) fn matches(&self, c: char, ignore_case: bool) -> bool {
        let code = c as u32;
        if code < 128 {
            return ((self.ascii_bits[(code >> 6) as usize] >> (code & 63)) & 1) != 0;
        }
        if !self.has_non_ascii_atoms {
            return self.negate;
        }
        let hit = self.atoms.iter().any(|a| atom_matches(a, c, ignore_case));
        hit != self.negate
    }
}
