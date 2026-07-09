//! ミニ JS プログラム集の出力比較テスト（`console.log` フック経由）。

#[path = "vm_common/mod.rs"]
mod vm_common;
use vm_common::run_capture;

#[test]
fn program_fizzbuzz() {
    let src = r#"
        for (let i = 1; i <= 15; i++) {
            if (i % 15 === 0) console.log("FizzBuzz");
            else if (i % 3 === 0) console.log("Fizz");
            else if (i % 5 === 0) console.log("Buzz");
            else console.log(String(i));
        }
    "#;
    let out = run_capture(src);
    let expected: Vec<String> = vec![
        "1", "2", "Fizz", "4", "Buzz", "Fizz", "7", "8", "Fizz", "Buzz", "11", "Fizz", "13", "14",
        "FizzBuzz",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_eq!(out, expected);
}

#[test]
fn program_fibonacci_with_closure_memoization() {
    let src = r#"
        function makeFib() {
            const cache = new Map();
            function fib(n) {
                if (n <= 1) return n;
                if (cache.has(n)) return cache.get(n);
                const v = fib(n - 1) + fib(n - 2);
                cache.set(n, v);
                return v;
            }
            return fib;
        }
        const fib = makeFib();
        for (let i = 0; i <= 10; i++) {
            console.log(`fib(${i}) = ${fib(i)}`);
        }
    "#;
    let out = run_capture(src);
    let expected: Vec<String> = vec![0, 1, 1, 2, 3, 5, 8, 13, 21, 34, 55]
        .into_iter()
        .enumerate()
        .map(|(i, v)| format!("fib({i}) = {v}"))
        .collect();
    assert_eq!(out, expected);
}

#[test]
fn program_bubble_sort() {
    let src = r#"
        function bubbleSort(arr) {
            const a = arr.slice();
            for (let i = 0; i < a.length; i++) {
                for (let j = 0; j < a.length - i - 1; j++) {
                    if (a[j] > a[j + 1]) {
                        const tmp = a[j];
                        a[j] = a[j + 1];
                        a[j + 1] = tmp;
                    }
                }
            }
            return a;
        }
        const input = [5, 3, 8, 1, 9, 2, 7];
        console.log("input: " + input.join(","));
        console.log("sorted: " + bubbleSort(input).join(","));
        console.log("input unchanged: " + input.join(","));
    "#;
    let out = run_capture(src);
    assert_eq!(
        out,
        vec![
            "input: 5,3,8,1,9,2,7".to_string(),
            "sorted: 1,2,3,5,7,8,9".to_string(),
            "input unchanged: 5,3,8,1,9,2,7".to_string(),
        ]
    );
}

#[test]
fn program_class_based_stack() {
    let src = r#"
        class Stack {
            #items = [];
            push(v) { this.#items.push(v); return this; }
            pop() { return this.#items.pop(); }
            get size() { return this.#items.length; }
            toString() { return "[" + this.#items.join(",") + "]"; }
        }
        const s = new Stack();
        s.push(1).push(2).push(3);
        console.log("size=" + s.size + " stack=" + s.toString());
        const popped = s.pop();
        console.log("popped=" + popped + " size=" + s.size);
    "#;
    let out = run_capture(src);
    assert_eq!(
        out,
        vec![
            "size=3 stack=[1,2,3]".to_string(),
            "popped=3 size=2".to_string()
        ]
    );
}

#[test]
fn program_word_frequency_count() {
    let src = r#"
        function wordFreq(text) {
            const words = text.split(" ");
            const freq = new Map();
            for (const w of words) {
                freq.set(w, (freq.get(w) ?? 0) + 1);
            }
            return freq;
        }
        const freq = wordFreq("the cat sat on the mat the cat ran");
        const entries = [...freq.entries()];
        entries.sort ? null : null; // sort は要求仕様に無いため未使用（挿入順のまま比較する）
        for (const [word, count] of entries) {
            console.log(word + ": " + count);
        }
    "#;
    let out = run_capture(src);
    assert_eq!(
        out,
        vec![
            "the: 3".to_string(),
            "cat: 2".to_string(),
            "sat: 1".to_string(),
            "on: 1".to_string(),
            "mat: 1".to_string(),
            "ran: 1".to_string(),
        ]
    );
}
