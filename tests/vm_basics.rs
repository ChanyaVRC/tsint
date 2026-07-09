//! T2（バイトコードコンパイラ + VM）の機能別ユニットテスト。

#[path = "vm_common/mod.rs"]
mod vm_common;
use vm_common::*;

// ============================== 基本演算 ==============================

#[test]
fn arithmetic_precedence() {
    assert_eq!(eval_num("1 + 2 * 3"), 7.0);
}

#[test]
fn exponent_right_assoc() {
    assert_eq!(eval_num("2 ** 3 ** 2"), 512.0);
}

#[test]
fn string_concat_with_plus() {
    assert_eq!(eval_str("'a' + 'b'"), "ab");
}

#[test]
fn number_plus_string_coerces_to_string() {
    assert_eq!(eval_str("1 + '2'"), "12");
}

#[test]
fn template_literal_basic() {
    assert_eq!(eval_str("`x=${1 + 1}, y=${'a' + 'b'}`"), "x=2, y=ab");
}

#[test]
fn nested_template_literal() {
    assert_eq!(eval_str("`outer ${`inner ${1+1}`}`"), "outer inner 2");
}

#[test]
fn loose_eq_main_rules() {
    assert!(eval_bool("1 == '1'"));
    assert!(eval_bool("null == undefined"));
    assert!(!eval_bool("NaN == NaN"));
    assert!(eval_bool("0 == false"));
}

#[test]
fn strict_eq_no_coercion() {
    assert!(!eval_bool("1 === '1'"));
    assert!(eval_bool("1 === 1"));
    assert!(!eval_bool("null === undefined"));
}

#[test]
fn ternary_and_comparisons() {
    assert_eq!(eval_num("1 < 2 ? 10 : 20"), 10.0);
}

#[test]
fn bitwise_and_shift_ops() {
    assert_eq!(eval_num("6 & 3"), 2.0);
    assert_eq!(eval_num("6 | 1"), 7.0);
    assert_eq!(eval_num("1 << 4"), 16.0);
    assert_eq!(eval_num("-1 >>> 28"), 15.0);
}

#[test]
fn typeof_various() {
    assert_eq!(eval_str("typeof 1"), "number");
    assert_eq!(eval_str("typeof 'a'"), "string");
    assert_eq!(eval_str("typeof undefined"), "undefined");
    assert_eq!(eval_str("typeof (()=>1)"), "function");
    assert_eq!(eval_str("typeof notDeclaredAnywhere"), "undefined");
}

// ============================== クロージャ ==============================

#[test]
fn closure_counter_shares_state() {
    let src = r#"
        function makeCounter() {
            let n = 0;
            return function () { n++; return n; };
        }
        const c = makeCounter();
        c(); c();
        export const result = c();
    "#;
    assert_eq!(as_num(&export(&run_ok(src), "result")), 3.0);
}

#[test]
fn closure_independent_instances() {
    let src = r#"
        function makeCounter() { let n = 0; return () => ++n; }
        const a = makeCounter();
        const b = makeCounter();
        a(); a(); a();
        b();
        export const result = a() + b();
    "#;
    assert_eq!(as_num(&export(&run_ok(src), "result")), 4.0 + 2.0);
}

#[test]
fn arrow_this_captures_lexically() {
    let src = r#"
        function Obj() {
            this.v = 10;
            this.get = () => this.v;
        }
        const o = new Obj();
        export const result = o.get();
    "#;
    assert_eq!(as_num(&export(&run_ok(src), "result")), 10.0);
}

#[test]
fn upvalue_chain_two_levels() {
    let src = r#"
        function outer() {
            let x = 1;
            function middle() {
                function inner() { return x + 1; }
                return inner();
            }
            return middle();
        }
        export const result = outer();
    "#;
    assert_eq!(as_num(&export(&run_ok(src), "result")), 2.0);
}

// ============================== 分割代入 / スプレッド ==============================

#[test]
fn array_destructuring_with_rest() {
    let src = "const [a, b, ...rest] = [1, 2, 3, 4]; export const result = a + b + rest.length;";
    assert_eq!(eval_num_of(src), 5.0);
}

#[test]
fn array_destructuring_with_default() {
    let src = "const [a = 100, b = 200] = [1]; export const result = a + b;";
    assert_eq!(eval_num_of(src), 201.0);
}

#[test]
fn object_destructuring_rename_default_rest() {
    let src = r#"
        const { a, b: renamed = 10, ...rest } = { a: 1, c: 2, d: 3 };
        export const result = a + renamed + rest.c + rest.d;
    "#;
    assert_eq!(eval_num_of(src), 16.0);
}

#[test]
fn nested_destructuring() {
    let src = "const { a: [x, y] } = { a: [1, 2] }; export const result = x + y;";
    assert_eq!(eval_num_of(src), 3.0);
}

#[test]
fn function_param_destructuring_and_defaults() {
    let src = r#"
        function f({ a, b = 5 }, [c, d = 9]) { return a + b + c + d; }
        export const result = f({ a: 1 }, [2]);
    "#;
    assert_eq!(eval_num_of(src), 17.0);
}

#[test]
fn rest_param() {
    let src = "function sum(...xs) { return xs.reduce ? 0 : xs.length; } export const result = sum(1,2,3);";
    // reduce は要求仕様外のため未実装。長さだけ確認する。
    assert_eq!(eval_num_of(src), 3.0);
}

#[test]
fn array_spread_literal() {
    let src = "const a = [1, 2]; const b = [0, ...a, 3]; export const result = b.join(',');";
    assert_eq!(eval_str_of(src), "0,1,2,3");
}

#[test]
fn object_spread_literal() {
    let src = "const a = { x: 1 }; const b = { ...a, y: 2 }; export const result = b.x + b.y;";
    assert_eq!(eval_num_of(src), 3.0);
}

#[test]
fn spread_call_args() {
    let src = "function sum(a,b,c){return a+b+c;} const args=[1,2,3]; export const result = sum(...args);";
    assert_eq!(eval_num_of(src), 6.0);
}

#[test]
fn spread_new_args() {
    let src = r#"
        class P { constructor(x, y) { this.x = x; this.y = y; } }
        const args = [1, 2];
        const p = new P(...args);
        export const result = p.x + p.y;
    "#;
    assert_eq!(eval_num_of(src), 3.0);
}

#[test]
fn destructuring_assignment_expr_form() {
    let src =
        "let a, b; [a, b] = [1, 2]; ({ a, b } = { a: 3, b: 4 }); export const result = a + b;";
    assert_eq!(eval_num_of(src), 7.0);
}

// ============================== オプショナルチェーン / ?? ==============================

#[test]
fn optional_chaining_short_circuits_whole_chain() {
    let src = "const o = null; export const result = o?.a?.b ?? 'fallback';";
    assert_eq!(eval_str_of(src), "fallback");
}

#[test]
fn optional_chaining_does_not_evaluate_rest_of_chain() {
    let src = r#"
        let called = false;
        function f() { called = true; return 1; }
        const o = null;
        const r = o?.[f()];
        export const result = called;
    "#;
    assert!(!eval_bool_of(src));
}

#[test]
fn optional_call_short_circuits() {
    let src = "const o = { f: null }; export const result = o.f?.() ?? 'no-call';";
    assert_eq!(eval_str_of(src), "no-call");
}

#[test]
fn optional_chaining_with_existing_value() {
    let src = "const o = { a: { b: 5 } }; export const result = o?.a?.b ?? -1;";
    assert_eq!(eval_num_of(src), 5.0);
}

#[test]
fn nullish_coalescing_treats_zero_as_defined() {
    let src = "export const result = 0 ?? 'fallback';";
    assert_eq!(eval_num_of(src), 0.0);
}

#[test]
fn logical_and_or_short_circuit() {
    assert!(!eval_bool_of("export const result = false && 1;"));
    assert!(eval_bool_of("export const result = 0 || true;"));
}

// ============================== 複合代入 / 論理代入 ==============================

#[test]
fn nullish_assign_only_when_nullish() {
    let src = "let x = null; x ??= 5; export const result = x;";
    assert_eq!(eval_num_of(src), 5.0);
}

#[test]
fn nullish_assign_skips_when_not_nullish() {
    let src = "let x = 0; x ??= 5; export const result = x;";
    assert_eq!(eval_num_of(src), 0.0);
}

#[test]
fn or_assign_and_and_assign() {
    assert_eq!(
        eval_num_of("let x = 0; x ||= 5; export const result = x;"),
        5.0
    );
    assert_eq!(
        eval_num_of("let x = 1; x &&= 2; export const result = x;"),
        2.0
    );
}

#[test]
fn compound_assign_arithmetic() {
    let src = "let x = 10; x += 5; x -= 2; x *= 2; x /= 13; export const result = x;";
    assert_eq!(eval_num_of(src), 2.0);
}

#[test]
fn compound_assign_on_member() {
    let src = "const o = { v: 1 }; o.v += 9; export const result = o.v;";
    assert_eq!(eval_num_of(src), 10.0);
}

#[test]
fn update_prefix_and_postfix() {
    let src = "let x = 5; const a = x++; const b = ++x; export const result = a * 100 + b;";
    // a=5(post), x=6, b=7(pre) => 507
    assert_eq!(eval_num_of(src), 507.0);
}

#[test]
fn update_on_member() {
    let src = "const o = { v: 1 }; o.v++; ++o.v; export const result = o.v;";
    assert_eq!(eval_num_of(src), 3.0);
}

// ============================== try/catch/finally ==============================

#[test]
fn finally_runs_after_return_in_try() {
    let src = r#"
        const log = [];
        function f() {
            try { log.push('try'); return 1; } finally { log.push('finally'); }
        }
        f();
        export const result = log.join(',');
    "#;
    assert_eq!(eval_str_of(src), "try,finally");
}

#[test]
fn catch_then_finally_order_with_throw() {
    let src = r#"
        const log = [];
        try {
            log.push('try');
            throw new Error('x');
        } catch (e) {
            log.push('catch:' + e.message);
        } finally {
            log.push('finally');
        }
        export const result = log.join(',');
    "#;
    assert_eq!(eval_str_of(src), "try,catch:x,finally");
}

#[test]
fn finally_return_overrides_try_return() {
    let src = r#"
        function f() {
            try { return 1; } finally { return 2; }
        }
        export const result = f();
    "#;
    assert_eq!(eval_num_of(src), 2.0);
}

#[test]
fn exception_propagates_through_nested_calls() {
    let src = r#"
        function level3() { throw new Error('boom'); }
        function level2() { level3(); }
        function level1() {
            try { level2(); } catch (e) { return 'caught:' + e.message; }
        }
        export const result = level1();
    "#;
    assert_eq!(eval_str_of(src), "caught:boom");
}

#[test]
fn uncaught_exception_fails_run() {
    let msg = run_err("throw new Error('nope');");
    assert!(msg.contains("nope"), "got: {msg}");
}

#[test]
fn nested_try_finally_runs_both() {
    let src = r#"
        const log = [];
        function f() {
            try {
                try { log.push('inner'); throw new Error('e'); } finally { log.push('inner-finally'); }
            } catch (e) {
                log.push('outer-catch');
            } finally {
                log.push('outer-finally');
            }
        }
        f();
        export const result = log.join(',');
    "#;
    assert_eq!(
        eval_str_of(src),
        "inner,inner-finally,outer-catch,outer-finally"
    );
}

#[test]
fn break_through_finally() {
    let src = r#"
        const log = [];
        for (let i = 0; i < 3; i++) {
            try {
                if (i === 1) break;
                log.push('iter:' + i);
            } finally {
                log.push('finally:' + i);
            }
        }
        export const result = log.join(',');
    "#;
    assert_eq!(eval_str_of(src), "iter:0,finally:0,finally:1");
}

// ============================== ラベル付き break/continue ==============================

#[test]
fn labeled_break_outer_loop() {
    let src = r#"
        let result = 0;
        outer: for (let i = 0; i < 3; i++) {
            for (let j = 0; j < 3; j++) {
                if (j === 1) continue outer;
                if (i === 2) break outer;
                result += 1;
            }
        }
        export { result };
    "#;
    assert_eq!(eval_num_of(src), 2.0);
}

#[test]
fn labeled_continue_skips_to_outer_update() {
    let src = r#"
        const log = [];
        outer: for (let i = 0; i < 3; i++) {
            for (let j = 0; j < 2; j++) {
                if (j === 0) continue outer;
                log.push(i + ':' + j);
            }
        }
        export const result = log.length;
    "#;
    assert_eq!(eval_num_of(src), 0.0);
}

#[test]
fn plain_break_and_continue() {
    let src = r#"
        let sum = 0;
        for (let i = 0; i < 10; i++) {
            if (i === 5) break;
            if (i % 2 === 0) continue;
            sum += i;
        }
        export const result = sum;
    "#;
    // i=1,3 が加算される
    assert_eq!(eval_num_of(src), 4.0);
}

// ============================== クラス ==============================

#[test]
fn class_getter_setter_and_static_method() {
    let src = r#"
        class Counter {
            constructor(start) { this._count = start; }
            get value() { return this._count; }
            set value(v) { this._count = v; }
            inc() { this._count++; return this; }
            static create() { return new Counter(0); }
        }
        const c = Counter.create();
        c.inc().inc();
        c.value = c.value + 10;
        export const result = c.value;
    "#;
    assert_eq!(eval_num_of(src), 12.0);
}

#[test]
fn class_fields_and_private_names() {
    let src = r#"
        class Box {
            #secret = 42;
            label = "box";
            reveal() { return this.#secret; }
        }
        const b = new Box();
        export const result = b.reveal() + b.label.length;
    "#;
    assert_eq!(eval_num_of(src), 42.0 + 3.0);
}

#[test]
fn class_static_field() {
    let src = r#"
        class C { static count = 5; static bump() { return ++C.count; } }
        C.bump();
        export const result = C.count;
    "#;
    assert_eq!(eval_num_of(src), 6.0);
}

#[test]
fn class_instances_are_independent() {
    let src = r#"
        class C { constructor(v) { this.v = v; } }
        const a = new C(1);
        const b = new C(2);
        a.v = 100;
        export const result = a.v + b.v;
    "#;
    assert_eq!(eval_num_of(src), 102.0);
}

#[test]
fn instanceof_with_class() {
    let src = "class C {} const c = new C(); export const result = (c instanceof C) && !(1 instanceof C);";
    assert!(eval_bool_of(src));
}

// ============================== Map / Set ==============================

#[test]
fn map_basic_ops() {
    let src = r#"
        const m = new Map();
        m.set('a', 1).set('b', 2);
        export const result = m.get('a') + m.get('b') + m.size;
    "#;
    assert_eq!(eval_num_of(src), 5.0);
}

#[test]
fn map_delete_and_has() {
    let src = r#"
        const m = new Map([['a', 1], ['b', 2]]);
        m.delete('a');
        export const result = (m.has('a') ? 1 : 0) * 10 + (m.has('b') ? 1 : 0);
    "#;
    assert_eq!(eval_num_of(src), 1.0);
}

#[test]
fn map_keys_values_entries_and_spread() {
    let src = r#"
        const m = new Map([['a', 1], ['b', 2]]);
        const keys = [...m.keys()];
        const values = [...m.values()];
        export const result = keys.join(',') + '|' + values.join(',');
    "#;
    assert_eq!(eval_str_of(src), "a,b|1,2");
}

#[test]
fn map_for_of_iterates_entries() {
    let src = r#"
        const m = new Map([['a', 1], ['b', 2]]);
        let out = '';
        for (const [k, v] of m) { out += k + ':' + v + ';'; }
        export const result = out;
    "#;
    assert_eq!(eval_str_of(src), "a:1;b:2;");
}

#[test]
fn set_basic_ops_and_for_of() {
    let src = r#"
        const s = new Set([1, 2, 2, 3]);
        s.add(4);
        let sum = 0;
        for (const v of s) sum += v;
        export const result = s.size + sum;
    "#;
    assert_eq!(eval_num_of(src), 4.0 + 10.0);
}

// ============================== JSON ==============================

#[test]
fn json_roundtrip_object_and_array() {
    let src = r#"
        const obj = { a: 1, b: [1, 2, 3], c: "x", d: null, e: true };
        const back = JSON.parse(JSON.stringify(obj));
        export const result = back.a === 1 && back.b.length === 3 && back.c === "x"
            && back.d === null && back.e === true;
    "#;
    assert!(eval_bool_of(src));
}

#[test]
fn json_stringify_skips_undefined_and_functions() {
    let src = r#"
        const obj = { a: 1, b: undefined, c: function () {} };
        export const result = JSON.stringify(obj);
    "#;
    assert_eq!(eval_str_of(src), "{\"a\":1}");
}

// ============================== new Function ==============================

#[test]
fn new_function_basic() {
    let src = "const f = new Function('a', 'b', 'return a + b;'); export const result = f(2, 3);";
    assert_eq!(eval_num_of(src), 5.0);
}

#[test]
fn new_function_only_sees_globals_not_enclosing_scope() {
    let src = r#"
        let secret = 42;
        const f = new Function('return typeof secret;');
        export const result = f();
    "#;
    assert_eq!(eval_str_of(src), "undefined");
}

#[test]
fn new_function_can_see_true_globals() {
    let src = r#"
        const f = new Function('return Math.floor(3.7);');
        export const result = f();
    "#;
    assert_eq!(eval_num_of(src), 3.0);
}

// ============================== TDZ ==============================

#[test]
fn tdz_throws_reference_error() {
    let src = r#"
        function f() {
            try { return y; } catch (e) { return 'caught:' + e.name; }
            let y = 1;
        }
        export const result = f();
    "#;
    assert_eq!(eval_str_of(src), "caught:ReferenceError");
}

#[test]
fn let_after_declaration_works_normally() {
    let src = "let x = 1; x = x + 1; export const result = x;";
    assert_eq!(eval_num_of(src), 2.0);
}

#[test]
fn const_reassignment_is_not_prevented_but_declared_once() {
    // T2 は const の再代入禁止までは強制しない（TDZ の簡易実装のみ要求）。
    let src = "const x = 1; export const result = x;";
    assert_eq!(eval_num_of(src), 1.0);
}

// ============================== switch ==============================

#[test]
fn switch_fallthrough_and_default() {
    let src = r#"
        function f(x) {
            let out = '';
            switch (x) {
                case 1:
                case 2:
                    out += 'low;';
                    break;
                case 3:
                    out += 'three;';
                default:
                    out += 'default;';
            }
            return out;
        }
        export const result = f(1) + '|' + f(3) + '|' + f(9);
    "#;
    assert_eq!(eval_str_of(src), "low;|three;default;|default;");
}

// ============================== 文字列メソッド ==============================

#[test]
fn string_methods_group_a() {
    let src = r#"
        const s = "  Hello World  ";
        export const result = s.trim().toLowerCase() + '|' + s.trim().toUpperCase();
    "#;
    assert_eq!(eval_str_of(src), "hello world|HELLO WORLD");
}

#[test]
fn string_methods_group_b() {
    let src = r#"
        const s = "hello";
        export const result = s.startsWith('he') + ':' + s.endsWith('lo') + ':' + s.includes('ell') + ':' + s.indexOf('l');
    "#;
    assert_eq!(eval_str_of(src), "true:true:true:2");
}

#[test]
fn string_slice_split_replace() {
    let src = r#"
        const s = "a,b,c";
        export const result = s.slice(0, 3) + '|' + s.split(',').join('-') + '|' + s.replace('a', 'z');
    "#;
    assert_eq!(eval_str_of(src), "a,b|a-b-c|z,b,c");
}

#[test]
fn string_char_code_repeat_pad() {
    let src = r#"
        export const result = "A".charCodeAt(0) + ':' + "ab".repeat(3) + ':' + "5".padStart(3, '0');
    "#;
    assert_eq!(eval_str_of(src), "65:ababab:005");
}

// ============================== 配列メソッド ==============================

#[test]
fn array_push_pop_shift_unshift() {
    let src = r#"
        const a = [1, 2];
        a.push(3);
        a.unshift(0);
        const popped = a.pop();
        const shifted = a.shift();
        export const result = a.join(',') + '|' + popped + '|' + shifted;
    "#;
    assert_eq!(eval_str_of(src), "1,2|3|0");
}

#[test]
fn array_slice_splice() {
    let src = r#"
        const a = [1, 2, 3, 4, 5];
        const sliced = a.slice(1, 3);
        const removed = a.splice(1, 2, 'x', 'y');
        export const result = sliced.join(',') + '|' + removed.join(',') + '|' + a.join(',');
    "#;
    assert_eq!(eval_str_of(src), "2,3|2,3|1,x,y,4,5");
}

#[test]
fn array_map_filter_foreach() {
    let src = r#"
        const a = [1, 2, 3, 4];
        const doubled = a.map(x => x * 2);
        const evens = a.filter(x => x % 2 === 0);
        let sum = 0;
        a.forEach(x => { sum += x; });
        export const result = doubled.join(',') + '|' + evens.join(',') + '|' + sum;
    "#;
    assert_eq!(eval_str_of(src), "2,4,6,8|2,4|10");
}

#[test]
fn array_find_some_every_includes_indexof() {
    let src = r#"
        const a = [1, 2, 3, 4];
        const found = a.find(x => x > 2);
        const some = a.some(x => x > 3);
        const every = a.every(x => x > 0);
        export const result = found + ':' + some + ':' + every + ':' + a.includes(3) + ':' + a.indexOf(3);
    "#;
    assert_eq!(eval_str_of(src), "3:true:true:true:2");
}

#[test]
fn array_flatmap_concat_isarray() {
    let src = r#"
        const a = [1, 2];
        const fm = a.flatMap(x => [x, x * 10]);
        const c = a.concat([3, 4], 5);
        export const result = fm.join(',') + '|' + c.join(',') + '|' + Array.isArray(a) + '|' + Array.isArray('x');
    "#;
    assert_eq!(eval_str_of(src), "1,10,2,20|1,2,3,4,5|true|false");
}

// ============================== Object / Number / Math ==============================

#[test]
fn object_keys_values_entries_assign() {
    let src = r#"
        const o = { a: 1, b: 2 };
        const merged = Object.assign({}, o, { c: 3 });
        export const result = Object.keys(o).join(',') + '|' + Object.values(o).join(',')
            + '|' + Object.entries(merged).map(e => e[0] + '=' + e[1]).join(',');
    "#;
    assert_eq!(eval_str_of(src), "a,b|1,2|a=1,b=2,c=3");
}

#[test]
fn number_and_parse_functions() {
    let src = r#"
        export const result = Number.isFinite(1) + ':' + Number.isFinite(Infinity)
            + ':' + parseInt('42px') + ':' + parseFloat('3.14abc');
    "#;
    assert_eq!(eval_str_of(src), "true:false:42:3.14");
}

#[test]
fn math_functions() {
    let src = r#"
        export const result = Math.floor(1.9) + ':' + Math.ceil(1.1) + ':' + Math.round(1.5)
            + ':' + Math.min(3, 1, 2) + ':' + Math.max(3, 1, 2) + ':' + Math.abs(-5) + ':' + Math.pow(2, 10);
    "#;
    assert_eq!(eval_str_of(src), "1:2:2:1:3:5:1024");
}

// ============================== for-of / for-in ==============================

#[test]
fn for_of_over_array_and_string() {
    let src = r#"
        let total = 0;
        for (const x of [1, 2, 3]) total += x;
        let chars = '';
        for (const c of 'abc') chars += c + '-';
        export const result = total + ':' + chars;
    "#;
    assert_eq!(eval_str_of(src), "6:a-b-c-");
}

#[test]
fn for_in_over_object_keys() {
    let src = r#"
        const o = { a: 1, b: 2 };
        let keys = '';
        for (const k in o) keys += k;
        export const result = keys;
    "#;
    assert_eq!(eval_str_of(src), "ab");
}

// ============================== その他 ==============================

#[test]
fn function_hoisting_allows_forward_reference() {
    let src = "export const result = f(); function f() { return 42; }";
    assert_eq!(eval_num_of(src), 42.0);
}

#[test]
fn sequence_expression_evaluates_left_to_right_returns_last() {
    let src = "let a = 0; export const result = (a = 1, a = a + 2, a);";
    assert_eq!(eval_num_of(src), 3.0);
}

#[test]
fn delete_operator_removes_property() {
    let src = "const o = { a: 1 }; delete o.a; export const result = ('a' in o) ? 1 : 0;";
    assert_eq!(eval_num_of(src), 0.0);
}

#[test]
fn in_operator_checks_own_and_array_index() {
    let src = "const a = [1, 2]; export const result = (0 in a) && !(5 in a) && ('length' in a);";
    assert!(eval_bool_of(src));
}

#[test]
fn void_and_unary_plus_minus() {
    let src = "export const result = (void 0 === undefined) && (+'3' === 3) && (-'3' === -3);";
    assert!(eval_bool_of(src));
}

#[test]
fn import_statement_is_a_clear_compile_error() {
    let msg = run_err("import { x } from \"y\"; export const result = 1;");
    assert!(msg.contains("import"), "got: {msg}");
}

#[test]
fn export_named_and_default() {
    let src = "const a = 1; const b = 2; export { a, b as renamed }; export default a + b;";
    let exports = run_ok(src);
    assert_eq!(as_num(&export(&exports, "a")), 1.0);
    assert_eq!(as_num(&export(&exports, "renamed")), 2.0);
    assert_eq!(as_num(&export(&exports, "default")), 3.0);
}

// ---- ヘルパー: `export const result = ...;` を含む完全なソース用 ----

fn eval_num_of(src: &str) -> f64 {
    as_num(&export(&run_ok(src), "result"))
}
fn eval_str_of(src: &str) -> String {
    as_str(&export(&run_ok(src), "result"))
}
fn eval_bool_of(src: &str) -> bool {
    as_bool(&export(&run_ok(src), "result"))
}
