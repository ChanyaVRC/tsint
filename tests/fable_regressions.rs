//! Claude Fable コードレビュー指摘事項（T-1〜T-29）の回帰テスト・GC ストレステスト。

mod vm_common;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use tsint::module::{ModuleHost, ModuleOutcome};
use tsint::value::Value;
use tsint::vm::{HostPromise, ModuleTaskState, PumpState};
use tsint::{parse_module, InterpError, Interpreter};
use vm_common::{as_bool, as_num, as_str, eval_bool, eval_num, eval_str, export, run_err, run_ok};

// =====================================================================
// T-1: async 駆動中の退避フレーム GC ルート & Boxed セル trial-deletion
// =====================================================================

#[test]
fn t1_gc_preserves_captured_object_across_async_sync_prefix_repro1() {
    let src = r#"
        async function work() {
            const junk = [];
            for (let i = 0; i < 400; i++) junk.push({ i });
            return junk.length;
        }
        async function scene() {
            const cfg = { speed: 2 };
            for (let i = 0; i < 3; i++) {
                const btn: any = { id: i };
                btn.onClick = () => { btn.id; return cfg; };
            }
            await work();
            return cfg.speed;
        }
        export const r = await scene();
    "#;
    let mut interp = Interpreter::new();
    interp.vm().set_gc_threshold(1);
    let exports = interp.run_module(src, "t1_repro1.ts").unwrap();
    assert_eq!(as_num(&export(&exports, "r")), 2.0);
}

#[test]
fn t1_gc_preserves_self_cyclic_captured_object_across_async_repro2() {
    let src = r#"
        async function work() {
            const junk = [];
            for (let i = 0; i < 200; i++) junk.push({ i });
            return junk.length;
        }
        async function scene() {
            const obj: any = { x: 42 };
            obj.cb = () => obj;
            await work();
            return obj.x;
        }
        export const r = await scene();
    "#;
    let mut interp = Interpreter::new();
    interp.vm().set_gc_threshold(1);
    let exports = interp.run_module(src, "t1_repro2.ts").unwrap();
    assert_eq!(as_num(&export(&exports, "r")), 42.0);
}

#[test]
fn t1_gc_stress_threshold_1_with_closures_async_and_cycles() {
    let src = r#"
        async function compute(seed: number): Promise<number> {
            const state = { total: seed };
            const getters: Array<() => number> = [];
            for (let i = 0; i < 5; i++) {
                const trash: any = { idx: i };
                trash.self = trash;
                trash.fn = () => trash.idx + state.total;
                const local = i * 2;
                getters.push(() => state.total + local);
            }
            await Promise.resolve({ nested: [1, 2, 3] });
            let acc = 0;
            for (const g of getters) acc += g();
            return acc;
        }
        async function main() {
            let sum = 0;
            for (let round = 0; round < 6; round++) {
                sum += await compute(round);
            }
            return sum;
        }
        export const result = await main();
    "#;
    let mut interp = Interpreter::new();
    interp.vm().set_gc_threshold(1);
    let exports = interp.run_module(src, "t1_stress.ts").unwrap();
    // compute(s) = sum_{i=0..5}(s + 2*i) = 5*s + 20; sum_{s=0..6}(5*s + 20) = 5*15 + 120 = 195
    assert_eq!(as_num(&export(&exports, "result")), 195.0);
}

// =====================================================================
// T-2: ループ本体 let/const & for...of / for...in の反復ごとの新セル束縛
// =====================================================================

#[test]
fn t2_loop_per_iteration_closure_bindings() {
    let src = r#"
        const g1: Array<() => number> = [];
        for (const v of [1, 2, 3]) g1.push(() => v);

        const g2: Array<() => string> = [];
        for (const k in { a: 1, b: 2, c: 3 }) g2.push(() => k);

        const g3: Array<() => number> = [];
        for (let i = 0; i < 3; i++) {
            const x = i * 10;
            g3.push(() => x);
        }

        let k = 0;
        const g4: Array<() => number> = [];
        while (k < 3) {
            let y = k;
            g4.push(() => y);
            k++;
        }

        const g5: Array<() => number> = [];
        for (let i = 0; i < 3; i++) {
            if (true) {
                const nested = i + 100;
                g5.push(() => nested);
            }
        }

        export const r1 = g1.map(f => f()).join(",");
        export const r2 = g2.map(f => f()).join(",");
        export const r3 = g3.map(f => f()).join(",");
        export const r4 = g4.map(f => f()).join(",");
        export const r5 = g5.map(f => f()).join(",");
    "#;
    let exports = run_ok(src);
    assert_eq!(as_str(&export(&exports, "r1")), "1,2,3");
    assert_eq!(as_str(&export(&exports, "r2")), "a,b,c");
    assert_eq!(as_str(&export(&exports, "r3")), "0,10,20");
    assert_eq!(as_str(&export(&exports, "r4")), "0,1,2");
    assert_eq!(as_str(&export(&exports, "r5")), "100,101,102");
}

#[test]
fn t2_loop_body_restores_tdz_each_iteration() {
    let src = r#"
        let caught = false;
        for (let i = 0; i < 2; i++) {
            try {
                if (i === 1) {
                    const _bad = x;
                }
            } catch (e) {
                if (e instanceof ReferenceError) caught = true;
            }
            let x = i;
        }
        export const result = caught;
    "#;
    let exports = run_ok(src);
    assert!(as_bool(&export(&exports, "result")));
}

// =====================================================================
// T-3: try { return } finally { try { throw } catch {} }
// =====================================================================

#[test]
fn t3_return_in_try_with_inner_try_catch_in_finally() {
    let src = r#"
        function f() {
            try {
                return 1;
            } finally {
                try {
                    throw 0;
                } catch (e) {}
            }
        }
        export const result = f();
    "#;
    let exports = run_ok(src);
    assert_eq!(as_num(&export(&exports, "result")), 1.0);
}

// =====================================================================
// T-4: ネイティブ再帰深さ上限 & JS フレーム数上限（debug ビルドでも安全）
// =====================================================================

#[test]
fn t4_deep_recursion_throws_catchable_range_error_even_on_1mb_stack() {
    let handle = std::thread::Builder::new()
        .name("t4-stack-test".into())
        .stack_size(1024 * 1024)
        .spawn(|| {
            let src = r#"
                function checkRangeError(fn: () => void): boolean {
                    try {
                        fn();
                        return false;
                    } catch (e) {
                        return e instanceof RangeError;
                    }
                }

                // 1. deep `new`
                class N {
                    c: any;
                    constructor(d: number) {
                        this.c = d > 0 ? new N(d - 1) : null;
                    }
                }
                const rNew = checkRangeError(() => { new N(100000); });

                // 2. deep native callback (`map`)
                function fMap(n: number): number {
                    return n === 0 ? 0 : [1].map(() => fMap(n - 1))[0];
                }
                const rMap = checkRangeError(() => { fMap(100000); });

                // 3. deep spread call
                function fSpread(n: number): number {
                    return n === 0 ? 0 : fSpread(...[n - 1]);
                }
                const rSpread = checkRangeError(() => { fSpread(100000); });

                // 4. deep plain JS recursion
                function fPlain(n: number): number {
                    return n === 0 ? 0 : 1 + fPlain(n - 1);
                }
                const rPlain = checkRangeError(() => { fPlain(100000); });

                export const allRangeErrors = rNew && rMap && rSpread && rPlain;
            "#;
            let exports = run_ok(src);
            assert!(as_bool(&export(&exports, "allRangeErrors")));
        })
        .unwrap();
    handle.join().unwrap();
}

// =====================================================================
// T-5: 深い連結構造の反復 Drop（debug ビルドで 20 万ノード以上解放）
// =====================================================================

#[test]
fn t5_dropping_deep_linked_list_and_dag_does_not_stack_overflow() {
    let handle = std::thread::Builder::new()
        .name("t5-drop-test".into())
        .stack_size(1024 * 1024)
        .spawn(|| {
            let src = r#"
                let list: any = null;
                for (let i = 0; i < 220000; i++) {
                    list = { next: list };
                }
                list = null;

                // DAG（各ノードが次ノードへの参照を 2 本持つ）
                let dag: any = null;
                for (let i = 0; i < 50000; i++) {
                    dag = { l: dag, r: dag };
                }
                dag = null;
                export const ok = true;
            "#;
            let exports = run_ok(src);
            assert!(as_bool(&export(&exports, "ok")));
        })
        .unwrap();
    handle.join().unwrap();
}

// =====================================================================
// T-6: 循環・深いネストに対する保護
// =====================================================================

#[test]
fn t6_json_stringify_circular() {
    let src_json_cyc = r#"
        const o: any = {};
        o.self = o;
        let isTypeErr = false;
        try { JSON.stringify(o); } catch (e) { isTypeErr = e instanceof TypeError; }
        export const result = isTypeErr;
    "#;
    assert!(as_bool(&export(&run_ok(src_json_cyc), "result")));
}

#[test]
fn t6_array_self_ref_stringify() {
    let src_arr_cyc = r#"
        const a: any[] = [];
        a.push(a);
        const b: any[] = [1];
        b.push(b);
        export const s1 = "" + a;
        export const s2 = b.join(",");
    "#;
    let ex = run_ok(src_arr_cyc);
    assert_eq!(as_str(&export(&ex, "s1")), "");
    assert_eq!(as_str(&export(&ex, "s2")), "1,");
}

#[test]
fn t6_json_parse_deep() {
    let src_json_deep = r#"
        let isSyntaxErr = false;
        try {
            JSON.parse("[".repeat(5000) + "]".repeat(5000));
        } catch (e) {
            isSyntaxErr = e instanceof SyntaxError;
        }
        export const result = isSyntaxErr;
    "#;
    assert!(as_bool(&export(&run_ok(src_json_deep), "result")));
}

#[test]
fn t6_regex_deep_groups() {
    let src_re_deep = r#"
        let isSyntaxErr = false;
        try {
            new RegExp("(".repeat(5000) + ")".repeat(5000));
        } catch (e) {
            isSyntaxErr = e instanceof SyntaxError;
        }
        export const result = isSyntaxErr;
    "#;
    assert!(as_bool(&export(&run_ok(src_re_deep), "result")));
}

#[test]
fn t6_ts_parser_deep_parens() {
    let deep_src = format!("{}1{}", "(".repeat(2000), ")".repeat(2000));
    let mut interp = Interpreter::new();
    match interp.run_module(&deep_src, "deep.ts") {
        Err(InterpError::Parse(_)) => {}
        other => panic!("ParseError を期待しましたが: {other:?}"),
    }
}

// =====================================================================
// T-7: 組み込みコンストラクタの instanceof
// =====================================================================

#[test]
fn t7_instanceof_builtin_constructors() {
    let src = r#"
        export const checks = [
            new TypeError("x") instanceof Error,
            new TypeError("x") instanceof TypeError,
            !(new Error("x") instanceof TypeError),
            new RangeError("x") instanceof Error,
            new RangeError("x") instanceof RangeError,
            new ReferenceError("x") instanceof ReferenceError,
            new SyntaxError("x") instanceof SyntaxError,
            [] instanceof Array,
            [] instanceof Object,
            new Map() instanceof Map,
            new Set() instanceof Set,
            Promise.resolve(1) instanceof Promise,
            new Date() instanceof Date,
            /a/ instanceof RegExp,
            ({}) instanceof Object,
            !(1 instanceof Object),
            ((() => {}) instanceof Function),
            !([] instanceof Map),
        ].every(Boolean);
    "#;
    assert!(as_bool(&export(&run_ok(src), "checks")));
}

// =====================================================================
// T-8 & T-24(parseInt): parseInt の radix 範囲チェックと大整数精度
// =====================================================================

#[test]
fn t8_parse_int_invalid_radix_and_large_numbers() {
    let src = r#"
        const mapped = ["1", "2", "3"].map(parseInt);
        export const r0 = mapped[0];
        export const r1_nan = isNaN(mapped[1]);
        export const r2_nan = isNaN(mapped[2]);
        export const big = parseInt("99999999999999999999");
        export const hex0 = parseInt("0x10", 0);
        export const bad37 = isNaN(parseInt("10", 37));
        export const badNeg = isNaN(parseInt("10", -2));
    "#;
    let ex = run_ok(src);
    assert_eq!(as_num(&export(&ex, "r0")), 1.0);
    assert!(as_bool(&export(&ex, "r1_nan")));
    assert!(as_bool(&export(&ex, "r2_nan")));
    assert!(as_num(&export(&ex, "big")) > 9e19);
    assert_eq!(as_num(&export(&ex, "hex0")), 16.0);
    assert!(as_bool(&export(&ex, "bad37")));
    assert!(as_bool(&export(&ex, "badNeg")));
}

// =====================================================================
// T-9: computed メソッド（オブジェクトリテラル対応・class は CompileError）
// =====================================================================

#[test]
fn t9_computed_key_methods_no_panic() {
    let src = r#"
        const k = "hello";
        const gk = "val";
        let backing = 10;
        const o: any = {
            [k](x: number) { return x + 1; },
            get [gk]() { return backing; },
            set [gk](v: number) { backing = v; },
        };
        o.val = 99;
        export const r = o.hello(41) + o.val;
    "#;
    assert_eq!(as_num(&export(&run_ok(src), "r")), 141.0);

    let mut interp = Interpreter::new();
    match interp.run_module(r#"class A { ["m"]() { return 1; } }"#, "cls.ts") {
        Err(InterpError::Compile(_)) => {}
        other => panic!("CompileError を期待しましたが: {other:?}"),
    }
}

// =====================================================================
// T-10: JSON.parse の \u 直後にマルチバイト文字があってもパニックしない
// =====================================================================

#[test]
fn t10_json_parse_u_escape_multibyte_no_panic() {
    let src = r#"
        let isSyntaxErr = false;
        try {
            JSON.parse('"\\u000é"');
        } catch (e) {
            isSyntaxErr = e instanceof SyntaxError;
        }
        export const result = isSyntaxErr;
    "#;
    assert!(as_bool(&export(&run_ok(src), "result")));
}

// =====================================================================
// T-11: ローカルスロット再利用（70,000 連続ブロック）& 非エイリアス
// =====================================================================

#[test]
fn t11_70000_sequential_blocks_reuse_slots_cleanly() {
    let mut src = String::with_capacity(70_000 * 16 + 128);
    src.push_str("let sum = 0;\n");
    for _ in 0..70_000 {
        src.push_str("{ let t = 1; sum += t; }\n");
    }
    src.push_str("export const result = sum;\n");
    let exports = run_ok(&src);
    assert_eq!(as_num(&export(&exports, "result")), 70_000.0);
}

#[test]
fn t11_sibling_blocks_do_not_alias_captured_slots() {
    let src = r#"
        let f1: any;
        let f2: any;
        { let a = 10; f1 = () => a; }
        { let b = 20; }
        { let c = 30; f2 = () => c; }
        export const result = f1() + f2();
    "#;
    assert_eq!(as_num(&export(&run_ok(src), "result")), 40.0);
}

// =====================================================================
// T-12: OOM 防止のサイズ上限チェック
// =====================================================================

#[test]
fn t12_allocation_size_caps_throw_range_error() {
    let src = r#"
        function isRange(fn: () => void): boolean {
            try { fn(); return false; } catch (e) { return e instanceof RangeError; }
        }
        const a: any[] = [];
        export const ok = [
            isRange(() => { a.length = Infinity; }),
            isRange(() => { a.length = -1; }),
            isRange(() => { a.length = 1e10; }),
            isRange(() => { a[1e9] = 0; }),
            isRange(() => { new Array(1e10); }),
            isRange(() => { new Array(-1); }),
            isRange(() => { "x".repeat(1e12); }),
            isRange(() => { "x".repeat(-1); }),
            isRange(() => { "x".repeat(Infinity); }),
            isRange(() => { "x".padStart(1e12, "a"); }),
            isRange(() => { "x".padEnd(1e12, "a"); }),
        ].every(Boolean);

        // JSON.stringify の space は 10 文字上限に丸められる
        export const jNum = JSON.stringify({ a: 1 }, null, 1e9);
        export const jStr = JSON.stringify({ a: 1 }, null, "0123456789ABCDEF");

        // 巨大展開 RegExp はエラーになる
        let re1Err = false;
        let re2Err = false;
        try { new RegExp("a{4294967295}"); } catch (e) { re1Err = e instanceof SyntaxError; }
        try { new RegExp("(?:a{60000}){60000}"); } catch (e) { re2Err = e instanceof SyntaxError; }
        export const reOk = re1Err && re2Err;
    "#;
    let ex = run_ok(src);
    assert!(as_bool(&export(&ex, "ok")));
    assert_eq!(as_str(&export(&ex, "jNum")), "{\n          \"a\":1\n}");
    assert_eq!(as_str(&export(&ex, "jStr")), "{\n0123456789\"a\":1\n}");
    assert!(as_bool(&export(&ex, "reOk")));
}

// =====================================================================
// T-13: finally の二重実行防止 & 入れ子例外スロット分離
// =====================================================================

#[test]
fn t13_finally_runs_once_on_throw_and_preserves_outer_exception() {
    let src = r#"
        let n = 0;
        function f() {
            try {
                return 1;
            } finally {
                n++;
                if (n < 5) throw new Error("x" + n);
            }
        }
        let msg = "";
        try { f(); } catch (e: any) { msg = e.message; }

        function g() {
            try {
                throw "A";
            } finally {
                try {
                    try {
                        throw "B";
                    } finally {}
                } catch (e) {}
            }
        }
        let caughtG = "";
        try { g(); } catch (e: any) { caughtG = e; }

        export const rN = n;
        export const rMsg = msg;
        export const rG = caughtG;
    "#;
    let ex = run_ok(src);
    assert_eq!(as_num(&export(&ex, "rN")), 1.0);
    assert_eq!(as_str(&export(&ex, "rMsg")), "x1");
    assert_eq!(as_str(&export(&ex, "rG")), "A");
}

// =====================================================================
// T-14: String / Array の第 2 位置引数
// =====================================================================

#[test]
fn t14_string_and_array_position_arguments() {
    assert_eq!(eval_num(r#""abcabc".indexOf("a", 1)"#), 3.0);
    assert_eq!(eval_num(r#""あいうあいう".indexOf("あ", 1)"#), 3.0);
    assert!(eval_bool(r#""abc".startsWith("b", 1)"#));
    assert!(eval_bool(r#""あいう".startsWith("い", 1)"#));
    assert!(eval_bool(r#""abc".endsWith("b", 2)"#));
    assert!(eval_bool(r#""あいう".endsWith("い", 2)"#));
    assert!(!eval_bool(r#""abc".includes("a", 1)"#));
    assert!(eval_bool(r#""あいう".includes("う", 2)"#));

    assert_eq!(eval_num("[1, 2, 1].indexOf(1, 1)"), 2.0);
    assert_eq!(eval_num("[1, 2, 1].indexOf(1, -2)"), 2.0);
    assert_eq!(eval_num("[1, 2, 1].indexOf(2, -1)"), -1.0);
    assert!(eval_bool("[1, 2, 3].includes(2, -2)"));
    assert!(!eval_bool("[1, 2, 3].includes(1, -2)"));
}

// =====================================================================
// T-15: Math.random() の決定的 PRNG と set_random_fn
// =====================================================================

#[test]
fn t15_math_random_deterministic_prng_and_custom_hook() {
    let src = "export const a = Math.random(); export const b = Math.random();";
    let ex1 = run_ok(src);
    let ex2 = run_ok(src);
    let a1 = as_num(&export(&ex1, "a"));
    let b1 = as_num(&export(&ex1, "b"));
    let a2 = as_num(&export(&ex2, "a"));
    let b2 = as_num(&export(&ex2, "b"));
    assert!((0.0..1.0).contains(&a1));
    assert!((0.0..1.0).contains(&b1));
    assert_ne!(a1, b1);
    assert_eq!(a1, a2);
    assert_eq!(b1, b2);

    let mut interp = Interpreter::new();
    let mut seq = 0.0;
    interp.set_random_fn(move || {
        seq += 0.25;
        seq
    });
    let ex = interp.run_module(src, "rand.ts").unwrap();
    assert_eq!(as_num(&export(&ex, "a")), 0.25);
    assert_eq!(as_num(&export(&ex, "b")), 0.5);
}

// =====================================================================
// T-16 & T-20: モジュール export 分離・Pending キャンセル・二重報告防止
// =====================================================================

#[test]
fn t20_sync_top_level_throw_does_not_double_report_unhandled() {
    let mut interp = Interpreter::new();
    let unhandled = Rc::new(RefCell::new(0usize));
    let u2 = unhandled.clone();
    interp.set_unhandled_rejection_hook(move |_| {
        *u2.borrow_mut() += 1;
    });
    let err = interp
        .run_module(r#"throw new Error("boom");"#, "sync_throw.ts")
        .unwrap_err();
    assert!(err.to_string().contains("boom"));
    assert_eq!(*unhandled.borrow(), 0);
}

#[test]
fn t16_pending_module_later_rejection_reports_unhandled_and_run_module_cancels() {
    // 1. start_module_task で Pending → 後で非同期に throw → unhandled hook が発火
    {
        let pending: Rc<RefCell<Option<HostPromise>>> = Rc::new(RefCell::new(None));
        let p2 = pending.clone();
        let mut interp = Interpreter::new();
        interp.register_native_async("gate", 0, move |_vm, _this, _args, hp| {
            *p2.borrow_mut() = Some(hp);
        });
        let unhandled = Rc::new(RefCell::new(Vec::<String>::new()));
        let u2 = unhandled.clone();
        interp.set_unhandled_rejection_hook(move |v| {
            u2.borrow_mut().push(tsint::vm::to_string_basic(v));
        });
        let m = parse_module(
            r#"await gate(); throw new Error("async-boom");"#,
            "pending.ts",
        )
        .unwrap();
        let chunk = tsint::compiler::compile_module(&m).unwrap();
        let st = interp.vm().start_module_task(chunk).unwrap();
        assert!(matches!(st, ModuleTaskState::Pending));
        assert!(unhandled.borrow().is_empty());

        let hp = pending.borrow_mut().take().unwrap();
        hp.resolve(interp.vm(), Value::Undefined);
        assert_eq!(interp.run_event_loop(), PumpState::Idle);
        assert_eq!(unhandled.borrow().len(), 1);
        assert!(unhandled.borrow()[0].contains("async-boom"));
    }

    // 2. run_module が Pending で Err を返した後、HostPromise を resolve しても副作用が起きない
    {
        let pending: Rc<RefCell<Option<HostPromise>>> = Rc::new(RefCell::new(None));
        let p2 = pending.clone();
        let logs = Rc::new(RefCell::new(Vec::<String>::new()));
        let l2 = logs.clone();
        let mut interp = Interpreter::new();
        interp.set_console_hook(move |s| l2.borrow_mut().push(s.to_string()));
        interp.register_native_async("gate", 0, move |_vm, _this, _args, hp| {
            *p2.borrow_mut() = Some(hp);
        });
        assert!(interp
            .run_module(r#"await gate(); console.log("should-not-run");"#, "sync.ts")
            .is_err());
        let hp = pending.borrow_mut().take().unwrap();
        hp.resolve(interp.vm(), Value::Undefined);
        interp.run_event_loop();
        assert!(logs.borrow().is_empty(), "got logs: {:?}", *logs.borrow());
    }

    // 3. 並走モジュールの export が混線しない
    {
        let pending: Rc<RefCell<Option<HostPromise>>> = Rc::new(RefCell::new(None));
        let p2 = pending.clone();
        let mut interp = Interpreter::new();
        interp.register_native_async("gate", 0, move |_vm, _this, _args, hp| {
            *p2.borrow_mut() = Some(hp);
        });
        let ma = parse_module("await gate(); export const who = 'A';", "a.ts").unwrap();
        let ca = tsint::compiler::compile_module(&ma).unwrap();
        assert!(matches!(
            interp.vm().start_module_task(ca).unwrap(),
            ModuleTaskState::Pending
        ));

        // B の実行中に A を解決させる
        let hp = pending.borrow_mut().take().unwrap();
        hp.resolve(interp.vm(), Value::Undefined);
        let mb = parse_module("export const who = 'B';", "b.ts").unwrap();
        let cb = tsint::compiler::compile_module(&mb).unwrap();
        match interp.vm().start_module_task(cb).unwrap() {
            ModuleTaskState::Settled(exports) => {
                assert_eq!(as_str(&export(&exports, "who")), "B");
            }
            ModuleTaskState::Pending => panic!("B should settle immediately"),
        }
    }
}

// =====================================================================
// T-17: Error.stack のチャンク名・行番号とメソッド呼び出し失敗メッセージ
// =====================================================================

#[test]
fn t17_error_stack_has_line_and_method_error_has_name() {
    let src = r#"
function inner() {
    return new Error("boom");
}
export const st = inner().stack;
    "#;
    let ex = run_ok(src);
    let st = as_str(&export(&ex, "st"));
    assert!(st.contains("test.ts"), "stack missing chunk name: {st}");
    assert!(st.contains(":3"), "stack missing line number: {st}");

    let err = run_err("(1.5).toFixed(1);");
    assert!(err.contains("toFixed"), "missing method name: {err}");
    assert!(err.contains("number"), "missing receiver type: {err}");
}

// =====================================================================
// T-18: regex ステップ予算の入力長スケーリング & 単一アトム貪欲繰り返し
// =====================================================================

#[test]
fn t18_regex_large_linear_inputs_succeed_and_redos_still_fails() {
    let src = r#"
        const s1 = "a".repeat(700000).replace(/\r\n|\r/g, "\n");
        const m2 = "a".repeat(60000).match(/^.*$/);
        export const len1 = s1.length;
        export const len2 = m2 ? m2[0].length : -1;
    "#;
    let ex = run_ok(src);
    assert_eq!(as_num(&export(&ex, "len1")), 700_000.0);
    assert_eq!(as_num(&export(&ex, "len2")), 60_000.0);

    let err = run_err(r#"/(a+)+b/.test("a".repeat(35));"#);
    assert!(
        err.contains("RangeError") || err.contains("ステップ数"),
        "got: {err}"
    );
}

// =====================================================================
// T-19: エントリ自己循環 import の二重評価防止 & 連続 run_module_graph 名前空間分離
// =====================================================================

struct MemHost(HashMap<String, String>);

impl ModuleHost for MemHost {
    fn resolve(&self, specifier: &str, _referrer: &str) -> Result<String, String> {
        Ok(specifier.to_string())
    }
    fn load(&self, resolved: &str) -> Result<String, String> {
        self.0
            .get(resolved)
            .cloned()
            .ok_or_else(|| format!("not found: {resolved}"))
    }
}

#[test]
fn t19_entry_self_circular_import_and_repeated_graphs_isolated() {
    let mut files = HashMap::new();
    files.insert(
        "entry.ts".to_string(),
        r#"
            import { seen } from "dep.ts";
            countCalls();
            export const val = seen;
        "#
        .to_string(),
    );
    files.insert(
        "dep.ts".to_string(),
        r#"
            import * as entry from "entry.ts";
            export const seen = typeof entry;
        "#
        .to_string(),
    );
    let host = MemHost(files);
    let calls = Rc::new(RefCell::new(0usize));
    let calls2 = calls.clone();
    let mut interp = Interpreter::new();
    interp.register_native(
        "countCalls",
        0,
        Rc::new(move |_vm, _this, _args| {
            *calls2.borrow_mut() += 1;
            Ok(Value::Undefined)
        }),
    );
    let out = interp.run_module_graph("entry.ts", &host).unwrap();
    assert!(matches!(out, ModuleOutcome::Settled(_)));
    assert_eq!(*calls.borrow(), 1, "entry must only evaluate once");

    // 2 回目の run_module_graph が 1 回目の名前空間グローバルを上書きしない
    let mut files2 = HashMap::new();
    files2.insert("m1.ts".into(), "export const x = 1;".into());
    files2.insert(
        "g1.ts".into(),
        r#"import * as m from "m1.ts"; export const get = () => m.x;"#.into(),
    );
    files2.insert("m2.ts".into(), "export const x = 2;".into());
    files2.insert(
        "g2.ts".into(),
        r#"import * as m from "m2.ts"; export const get = () => m.x;"#.into(),
    );
    let host2 = MemHost(files2);
    let mut interp2 = Interpreter::new();
    let fn1 = match interp2.run_module_graph("g1.ts", &host2).unwrap() {
        ModuleOutcome::Settled(e) => export(&e, "get"),
        ModuleOutcome::Pending => panic!("pending"),
    };
    let _ = interp2.run_module_graph("g2.ts", &host2).unwrap();
    let v1 = interp2.vm().call_value(fn1, Value::Undefined, &[]).unwrap();
    assert_eq!(as_num(&v1), 1.0);
}

// =====================================================================
// T-21: 非 async 関数内の await はコンパイルエラー
// =====================================================================

#[test]
fn t21_await_in_non_async_function_is_compile_error() {
    let mut interp = Interpreter::new();
    match interp.run_module("function f() { await 1; return 2; }", "t21.ts") {
        Err(InterpError::Compile(e)) => assert!(e.message.contains("await")),
        other => panic!("CompileError を期待しましたが: {other:?}"),
    }
    // トップレベル await は引き続き動作
    let ex = run_ok("export const r = await Promise.resolve(7);");
    assert_eq!(as_num(&export(&ex, "r")), 7.0);
}

// =====================================================================
// T-22: HostPromise の drop と二重 resolve ガード
// =====================================================================

#[test]
fn t22_host_promise_drop_and_double_resolve() {
    // 1. 未解決のまま drop しても Waiting に張り付かない
    {
        let mut interp = Interpreter::new();
        let (_v, hp) = interp.vm().new_host_promise();
        assert_eq!(interp.run_event_loop(), PumpState::Waiting);
        drop(hp);
        assert_eq!(interp.run_event_loop(), PumpState::Idle);
    }

    // 2. resolve(pendingPromise) 後の 2 回目 resolve は無視され、カウンタも二重減算しない
    {
        let mut interp = Interpreter::new();
        let (p_val, hp1) = interp.vm().new_host_promise();
        let (inner_val, hp_inner) = interp.vm().new_host_promise();
        let (_other_val, _hp_other) = interp.vm().new_host_promise();

        hp1.resolve(interp.vm(), inner_val);
        hp1.resolve(interp.vm(), Value::Number(999.0)); // 無視されるべき
        hp_inner.resolve(interp.vm(), Value::Number(42.0));
        // _hp_other がまだ未解決なので Waiting のまま
        assert_eq!(interp.run_event_loop(), PumpState::Waiting);

        interp.define_global("p", p_val);
        let ex = interp
            .run_module("export const r = await p;", "t22.ts")
            .unwrap();
        assert_eq!(as_num(&export(&ex, "r")), 42.0);
        drop(_hp_other);
        assert_eq!(interp.run_event_loop(), PumpState::Idle);
    }
}

// =====================================================================
// T-23: トップレベル this / #private & %time 非列挙 / 正規配列添字 / var 巻き上げ
// =====================================================================

#[test]
fn t23_this_private_fields_array_index_and_var_hoisting() {
    let src = r#"
        export const topThis = typeof this;

        class Secret {
            #hidden = 123;
            pub = 456;
            getHidden() { return this.#hidden; }
        }
        const s = new Secret();
        const keys: string[] = [];
        for (const k in s) keys.push(k);
        export const objKeys = Object.keys(s).join(",");
        export const objVals = Object.values(s).join(",");
        export const objEnts = Object.entries(s).length;
        export const forInKeys = keys.join(",");
        export const jsonSecret = JSON.stringify(s);
        export const dateKeys = Object.keys(new Date(1000)).length;
        export const hiddenVal = s.getHidden();

        const arr: any = [10, 20];
        arr["01"] = 99;
        export const arr1 = arr[1];
        export const arr01 = arr["01"];
        export const arrPlus1 = arr["+1"];

        function testVar() {
            const before = typeof x;
            if (true) { var x = 5; }
            if (true) var y = 7;
            if (false) var z = 9;
            return `${before}:${x}:${y}:${typeof z}`;
        }
        export const varRes = testVar();
    "#;
    let ex = run_ok(src);
    assert_eq!(as_str(&export(&ex, "topThis")), "undefined");
    assert_eq!(as_str(&export(&ex, "objKeys")), "pub");
    assert_eq!(as_str(&export(&ex, "objVals")), "456");
    assert_eq!(as_num(&export(&ex, "objEnts")), 1.0);
    assert_eq!(as_str(&export(&ex, "forInKeys")), "pub");
    assert_eq!(as_str(&export(&ex, "jsonSecret")), r#"{"pub":456}"#);
    assert_eq!(as_num(&export(&ex, "dateKeys")), 0.0);
    assert_eq!(as_num(&export(&ex, "hiddenVal")), 123.0);
    assert_eq!(as_num(&export(&ex, "arr1")), 20.0);
    assert_eq!(as_num(&export(&ex, "arr01")), 99.0);
    assert!(matches!(export(&ex, "arrPlus1"), Value::Undefined));
    assert_eq!(as_str(&export(&ex, "varRes")), "undefined:5:7:undefined");
}

// =====================================================================
// T-24: ToInt32 / ToNumber / 16進リテラルオーバーフロー
// =====================================================================

#[test]
fn t24_numeric_edge_cases() {
    assert_eq!(eval_num("1e20 | 0"), 1661992960.0);
    assert!(eval_bool(r#"isNaN(Number("inf"))"#));
    assert!(eval_bool(r#"isNaN(Number("nan"))"#));
    assert!(eval_bool(r#"isNaN(Number("infinity"))"#));
    assert!(eval_bool(r#"isNaN(Number("0x-5"))"#));
    assert!(eval_bool(r#"Number("Infinity") === Infinity"#));
    assert!(eval_bool(r#"Number("+Infinity") === Infinity"#));
    assert!(eval_bool(r#"Number("-Infinity") === -Infinity"#));
    assert_eq!(eval_num(r#"Number("0b11")"#), 3.0);
    assert_eq!(eval_num(r#"Number("0o7")"#), 7.0);
    assert!(eval_num("0xFFFFFFFFFFFFFFFF") > 1e19);
}

// =====================================================================
// T-25: サロゲートペア結合 & 不正 \u エスケープ
// =====================================================================

#[test]
fn t25_surrogate_pairs_and_invalid_unicode_escapes() {
    assert_eq!(eval_str(r#""\ud83d\ude00""#), "😀");
    assert_eq!(eval_str(r#"JSON.parse('"\\ud83d\\ude00"')"#), "😀");
    assert_eq!(eval_str(r#""\ud83d""#), "\u{FFFD}");
    assert_eq!(eval_str(r#"JSON.parse('"\\ud83d"')"#), "\u{FFFD}");

    let err = parse_module(r#"const s = 'x\u12';"#, "bad_u.ts").unwrap_err();
    assert!(
        err.message.contains("エスケープ"),
        "expected escape error, got: {err}"
    );
}

// =====================================================================
// T-26: native_call_depth > 0 でも死んだ弱参照が刈り取られる
// =====================================================================

#[test]
fn t26_gc_prunes_dead_weaks_during_native_callback() {
    let mut interp = Interpreter::new();
    interp.vm().set_gc_threshold(64);
    let src = r#"
        [1].forEach(() => {
            for (let i = 0; i < 500; i++) {
                const o = { x: i };
            }
        });
        export const ok = true;
    "#;
    let ex = interp.run_module(src, "t26.ts").unwrap();
    assert!(as_bool(&export(&ex, "ok")));
    assert!(
        interp.vm().live_tracked_objects() < 200,
        "dead objects should not accumulate"
    );
}

// =====================================================================
// T-28: Promise の細部（resolve 後 reject 無視・proto/getter thenable・finally(非関数)）
// =====================================================================

#[test]
fn t28_promise_spec_details() {
    let src = r#"
        let resolveInner: any;
        const inner = new Promise((r) => { resolveInner = r; });
        const p1 = new Promise((res, rej) => {
            res(inner);
            rej("should-be-ignored");
        });
        resolveInner(123);

        class CustomThenable {
            v: number;
            constructor(v: number) { this.v = v; }
            then(res: any) { res(this.v * 2); }
        }
        const getterThenable = {
            get then() {
                return (res: any) => res(77);
            }
        };

        let syncNonFnErr = false;
        try { new Promise(123 as any); } catch (e) { syncNonFnErr = e instanceof TypeError; }

        export const r1 = await p1;
        export const r2 = await Promise.resolve(new CustomThenable(21));
        export const r3 = await Promise.resolve(getterThenable);
        export const r4 = await Promise.resolve(42).finally(123 as any);
        export const r5 = syncNonFnErr;
    "#;
    let ex = run_ok(src);
    assert_eq!(as_num(&export(&ex, "r1")), 123.0);
    assert_eq!(as_num(&export(&ex, "r2")), 42.0);
    assert_eq!(as_num(&export(&ex, "r3")), 77.0);
    assert_eq!(as_num(&export(&ex, "r4")), 42.0);
    assert!(as_bool(&export(&ex, "r5")));
}

// =====================================================================
// T-29: アロー関数の投機パースが深い括弧入れ子で線形時間
// =====================================================================

#[test]
fn t29_nested_parens_parse_in_linear_time() {
    let mut src = String::new();
    src.push_str("let a = 0;\n");
    for _ in 0..30 {
        src.push_str("(a = ");
    }
    src.push('1');
    for _ in 0..30 {
        src.push(')');
    }
    src.push_str(";\nexport const result = a;\n");
    let t0 = std::time::Instant::now();
    let ex = run_ok(&src);
    assert!(
        t0.elapsed().as_secs() < 2,
        "30-level nested parens took too long: {:?}",
        t0.elapsed()
    );
    assert_eq!(as_num(&export(&ex, "result")), 1.0);
}
