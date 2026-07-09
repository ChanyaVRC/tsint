//! エラー耐性 fuzz テスト。
//!
//! Noiria コーパスのソースを LCG（線形合同法）で決定的に破壊し、
//! 500 ケースについて「パニックしないこと」を確認する。破壊の大半は
//! （切り詰め・バイト置換の組み合わせにより）構文的に壊れるため、
//! 多くのケースで `Err` になることも合わせて確認する（ただし偶然
//! 構文的に妥当なままになるケースもありうるため、そちらは緩めの閾値で見る）。

mod common;

use std::panic::{self, AssertUnwindSafe};

/// 決定的な擬似乱数生成器（Numerical Recipes 系 LCG）。
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Lcg(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0
    }

    fn next_below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            return 0;
        }
        (self.next_u64() % bound as u64) as usize
    }
}

/// ソースのバイト列を破壊する。切り詰め・バイト置換・バイト削除・
/// バイト挿入を LCG の出目に応じて混ぜる。
fn corrupt(src: &str, rng: &mut Lcg) -> String {
    let mut bytes = src.as_bytes().to_vec();
    if bytes.is_empty() {
        return String::new();
    }

    // 30% の確率でランダムな位置から切り詰める（未閉じの括弧/文字列を
    // 作り出しやすく、構文エラーに直結しやすい）。
    if rng.next_below(10) < 3 {
        let cut = rng.next_below(bytes.len()) + 1;
        bytes.truncate(cut);
    }

    // さらに数バイトを置換/削除/挿入する。
    let mutations = 1 + rng.next_below(20);
    for _ in 0..mutations {
        if bytes.is_empty() {
            break;
        }
        match rng.next_below(3) {
            0 => {
                // 置換
                let i = rng.next_below(bytes.len());
                bytes[i] = (rng.next_u64() % 256) as u8;
            }
            1 => {
                // 削除
                let i = rng.next_below(bytes.len());
                bytes.remove(i);
            }
            _ => {
                // 挿入
                let i = rng.next_below(bytes.len() + 1);
                bytes.insert(i, (rng.next_u64() % 256) as u8);
            }
        }
    }

    // 不正な UTF-8 になっている可能性があるため、lossy に戻す
    // (パーサに渡す前提の &str を必ず作れるようにするため。この置換自体は
    // フィールドテストの対象ではなく、あくまで安全に &str を得る手段)。
    String::from_utf8_lossy(&bytes).into_owned()
}

#[test]
fn fuzz_corrupted_corpus_never_panics() {
    let sources = common::read_corpus_sources();
    assert!(!sources.is_empty(), "コーパスソースが 1 件もありません");

    let mut rng = Lcg::new(0x5EED_C0FFEE_u64);
    let total_cases = 500;
    let mut err_count = 0usize;
    let mut ok_count = 0usize;

    // panic 時のデフォルトのスタックトレース出力を抑制し、500 ケース分の
    // ログで埋まらないようにする。テスト終了時に必ず元に戻す。
    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));

    let mut panicked_cases: Vec<usize> = Vec::new();

    for case in 0..total_cases {
        let (name, src) = &sources[case % sources.len()];
        let corrupted = corrupt(src, &mut rng);

        let result =
            panic::catch_unwind(AssertUnwindSafe(|| tsint::parse_module(&corrupted, name)));

        match result {
            Ok(Ok(_)) => ok_count += 1,
            Ok(Err(_)) => err_count += 1,
            Err(_) => panicked_cases.push(case),
        }
    }

    panic::set_hook(default_hook);

    assert!(
        panicked_cases.is_empty(),
        "{} 件のケースで panic しました（ケース番号: {:?}）",
        panicked_cases.len(),
        panicked_cases
    );

    println!("fuzz: 合計={total_cases} Err={err_count} Ok={ok_count} (panic=0)");
    // 破壊のほとんどは構文エラーを引き起こすはず。大部分が Err であることを
    // 緩い閾値で確認する（偶然妥当なままのケースを許容する）。
    assert!(
        err_count * 100 >= total_cases * 60,
        "Err の割合が低すぎます: err={err_count}/{total_cases}"
    );
}
