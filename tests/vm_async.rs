//! T3（Promise / マイクロタスクキュー / async・await / イベントループ）の
//! テスト。
//!
//! - 「順序テスト」: sync 部 → マイクロタスク → 次のタスク、という
//!   観測可能な実行順序を `console.log` の出力列で検証する。
//! - 「Noiria パターン」: `player.ts` 等で使われる形を縮約したフィクスチャ
//!   （`Promise.race([wait(ms), waitClick()])` 相当・割り込み・複数 async
//!   ループの並走・`waitFor(pred)` 型のハンドラ登録）を、
//!   `Interpreter::register_native_async` + 仮想タイマーで再現する。
//! - 「フレーム分離」: 並走する async 関数の一方が例外を投げても、
//!   他方の実行やモジュール全体が壊れないことを確認する。

use std::cell::RefCell;
use std::rc::Rc;

use tsint::value::Value;
use tsint::vm::{HostPromise, PumpState};
use tsint::Interpreter;

#[path = "vm_common/mod.rs"]
mod vm_common;
use vm_common::*;

// =====================================================================
// 仮想タイマー（テスト用ホスト非同期 API のデモ実装）
// =====================================================================

/// `wait(ms)` を提供する仮想タイマー。`advance` で時刻を進めるまで
/// 実際には解決しない（実時間には一切依存しない決定的なテストのため）。
struct VirtualClock {
    now: f64,
    timers: Vec<(f64, HostPromise)>,
}

fn install_virtual_wait(interp: &mut Interpreter) -> Rc<RefCell<VirtualClock>> {
    let clock = Rc::new(RefCell::new(VirtualClock {
        now: 0.0,
        timers: Vec::new(),
    }));
    let clock2 = clock.clone();
    interp.register_native_async("wait", 1, move |_vm, _this, args, handle| {
        let ms = match args.first() {
            Some(Value::Number(n)) => *n,
            _ => 0.0,
        };
        let deadline = clock2.borrow().now + ms;
        clock2.borrow_mut().timers.push((deadline, handle));
    });
    clock
}

/// 仮想時刻を進め、期限が来たタイマーを resolve してからイベントループを
/// 汲み尽くす。
fn advance_time(
    interp: &mut Interpreter,
    clock: &Rc<RefCell<VirtualClock>>,
    ms: impl Into<f64>,
) -> PumpState {
    let new_now = clock.borrow().now + ms.into();
    clock.borrow_mut().now = new_now;
    let fired = {
        let mut c = clock.borrow_mut();
        let (mut fired, remaining): (Vec<_>, Vec<_>) = std::mem::take(&mut c.timers)
            .into_iter()
            .partition(|(deadline, _)| *deadline <= new_now);
        c.timers = remaining;
        // 同時に複数のタイマーが満期になった場合は、期限が早い順に発火させる。
        fired.sort_by(|(a, _), (b, _)| a.partial_cmp(b).unwrap());
        fired
    };
    for (_, handle) in fired {
        handle.resolve(interp.vm(), Value::Undefined);
    }
    interp.run_event_loop()
}

fn capture_lines(interp: &mut Interpreter) -> Rc<RefCell<Vec<String>>> {
    let lines: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let lines2 = lines.clone();
    interp.set_console_hook(move |s: &str| lines2.borrow_mut().push(s.to_string()));
    lines
}

// =====================================================================
// A. Promise 基礎（状態・thenable 吸収・自己解決）
// =====================================================================

#[test]
fn promise_resolve_then_fires_as_microtask() {
    let lines = run_capture(
        r#"
        console.log('a');
        Promise.resolve(1).then(v => console.log('b', v));
        console.log('c');
    "#,
    );
    assert_eq!(lines, vec!["a", "c", "b 1"]);
}

#[test]
fn promise_new_executor_runs_synchronously() {
    let lines = run_capture(
        r#"
        console.log('before');
        new Promise((resolve) => { console.log('executor'); resolve(1); });
        console.log('after');
    "#,
    );
    assert_eq!(lines, vec!["before", "executor", "after"]);
}

#[test]
fn promise_then_chain_passes_return_value() {
    let lines = run_capture(
        r#"
        Promise.resolve(1)
            .then(v => v + 1)
            .then(v => v * 10)
            .then(v => console.log(v));
    "#,
    );
    assert_eq!(lines, vec!["20"]);
}

#[test]
fn promise_reject_propagates_to_catch() {
    let lines = run_capture(
        r#"
        Promise.reject('boom').catch(e => console.log('caught', e));
    "#,
    );
    assert_eq!(lines, vec!["caught boom"]);
}

#[test]
fn promise_then_without_reject_handler_passes_rejection_through() {
    let lines = run_capture(
        r#"
        Promise.reject('x')
            .then(v => console.log('should not run', v))
            .catch(e => console.log('caught', e));
    "#,
    );
    assert_eq!(lines, vec!["caught x"]);
}

#[test]
fn promise_resolve_with_promise_absorbs_it() {
    let lines = run_capture(
        r#"
        const inner = Promise.resolve(42);
        const outer = new Promise((resolve) => resolve(inner));
        outer.then(v => console.log(v));
    "#,
    );
    assert_eq!(lines, vec!["42"]);
}

#[test]
fn promise_resolve_with_thenable_object_absorbs_it() {
    let lines = run_capture(
        r#"
        const thenable = { then(res, rej) { res(7); } };
        Promise.resolve(thenable).then(v => console.log(v));
    "#,
    );
    assert_eq!(lines, vec!["7"]);
}

#[test]
fn promise_resolve_with_rejecting_thenable() {
    let lines = run_capture(
        r#"
        const thenable = { then(res, rej) { rej('nope'); } };
        Promise.resolve(thenable).catch(e => console.log('caught', e));
    "#,
    );
    assert_eq!(lines, vec!["caught nope"]);
}

#[test]
fn promise_self_resolution_rejects_with_type_error() {
    let lines = run_capture(
        r#"
        let resolveFn;
        const p = new Promise((resolve) => { resolveFn = resolve; });
        resolveFn(p);
        p.then(() => console.log('should not resolve')).catch(e => console.log('rejected', e.name));
    "#,
    );
    assert_eq!(lines, vec!["rejected TypeError"]);
}

#[test]
fn promise_resolve_on_already_native_promise_returns_same_identity() {
    assert!(eval_bool(
        "(() => { const p = Promise.resolve(1); return Promise.resolve(p) === p; })()"
    ));
}

// =====================================================================
// B. then/catch/finally の連鎖・順序
// =====================================================================

#[test]
fn promise_finally_runs_on_fulfillment_and_passes_value_through() {
    let lines = run_capture(
        r#"
        Promise.resolve(5)
            .finally(() => console.log('cleanup'))
            .then(v => console.log('value', v));
    "#,
    );
    assert_eq!(lines, vec!["cleanup", "value 5"]);
}

#[test]
fn promise_finally_runs_on_rejection_and_rethrows() {
    let lines = run_capture(
        r#"
        Promise.reject('e')
            .finally(() => console.log('cleanup'))
            .catch(e => console.log('caught', e));
    "#,
    );
    assert_eq!(lines, vec!["cleanup", "caught e"]);
}

#[test]
fn promise_finally_callback_throw_overrides_result() {
    let lines = run_capture(
        r#"
        Promise.resolve(1)
            .finally(() => { throw 'oops'; })
            .catch(e => console.log('caught', e));
    "#,
    );
    assert_eq!(lines, vec!["caught oops"]);
}

#[test]
fn promise_then_handler_throw_rejects_result_promise() {
    let lines = run_capture(
        r#"
        Promise.resolve(1)
            .then(() => { throw 'thrown-in-then'; })
            .catch(e => console.log('caught', e));
    "#,
    );
    assert_eq!(lines, vec!["caught thrown-in-then"]);
}

#[test]
fn promise_multiple_then_on_same_promise_all_run() {
    let lines = run_capture(
        r#"
        const p = Promise.resolve('x');
        p.then(v => console.log('one', v));
        p.then(v => console.log('two', v));
    "#,
    );
    assert_eq!(lines, vec!["one x", "two x"]);
}

#[test]
fn promise_then_added_after_settlement_still_runs_as_microtask() {
    let lines = run_capture(
        r#"
        const p = Promise.resolve('late');
        console.log('sync');
        p.then(v => console.log('then', v));
        console.log('sync2');
    "#,
    );
    assert_eq!(lines, vec!["sync", "sync2", "then late"]);
}

#[test]
fn promise_chain_ordering_interleaved_with_two_chains() {
    let lines = run_capture(
        r#"
        Promise.resolve().then(() => console.log('A1')).then(() => console.log('A2'));
        Promise.resolve().then(() => console.log('B1')).then(() => console.log('B2'));
    "#,
    );
    // 各チェーンの then はキューに積まれた順（FIFO）でマイクロタスクとして
    // 実行されるため、A1,B1,A2,B2 の順になる（レベルごとの横並び実行）。
    assert_eq!(lines, vec!["A1", "B1", "A2", "B2"]);
}

#[test]
fn promise_catch_is_sugar_for_then_undefined_reject() {
    let lines = run_capture(
        r#"
        Promise.reject('e').catch(function(err) { console.log('handled', err); });
    "#,
    );
    assert_eq!(lines, vec!["handled e"]);
}

#[test]
fn promise_finally_no_args_still_chains() {
    let lines = run_capture(
        r#"
        Promise.resolve(9).finally(() => {}).then(v => console.log(v));
    "#,
    );
    assert_eq!(lines, vec!["9"]);
}

// =====================================================================
// C. Promise.race / Promise.all
// =====================================================================

#[test]
fn promise_race_first_settled_wins_fulfilled() {
    let lines = run_capture(
        r#"
        const slow = new Promise(res => {});
        Promise.race([slow, Promise.resolve('fast')]).then(v => console.log(v));
    "#,
    );
    assert_eq!(lines, vec!["fast"]);
}

#[test]
fn promise_race_first_settled_wins_even_if_rejection() {
    let lines = run_capture(
        r#"
        const slow = new Promise(res => {});
        Promise.race([slow, Promise.reject('fail-fast')]).catch(e => console.log(e));
    "#,
    );
    assert_eq!(lines, vec!["fail-fast"]);
}

#[test]
fn promise_race_ignores_later_settlements() {
    let lines = run_capture(
        r#"
        let resolveA;
        const a = new Promise(res => { resolveA = res; });
        const b = Promise.resolve('b-wins');
        Promise.race([a, b]).then(v => console.log('winner', v));
        resolveA('a-late');
    "#,
    );
    assert_eq!(lines, vec!["winner b-wins"]);
}

#[test]
fn promise_all_resolves_with_values_in_order() {
    let lines = run_capture(
        r#"
        Promise.all([Promise.resolve(1), 2, Promise.resolve(3)]).then(vs => console.log(vs.join(',')));
    "#,
    );
    assert_eq!(lines, vec!["1,2,3"]);
}

#[test]
fn promise_all_rejects_if_any_rejects() {
    let lines = run_capture(
        r#"
        Promise.all([Promise.resolve(1), Promise.reject('bad'), Promise.resolve(3)])
            .catch(e => console.log('caught', e));
    "#,
    );
    assert_eq!(lines, vec!["caught bad"]);
}

#[test]
fn promise_all_empty_array_resolves_immediately_with_empty_array() {
    let lines = run_capture(
        r#"
        Promise.all([]).then(vs => console.log('len', vs.length));
    "#,
    );
    assert_eq!(lines, vec!["len 0"]);
}

#[test]
fn promise_all_preserves_order_regardless_of_completion_order() {
    let lines = run_capture(
        r#"
        let resolveFirst;
        const first = new Promise(res => { resolveFirst = res; });
        const second = Promise.resolve('fast');
        Promise.all([first, second]).then(vs => console.log(vs.join('|')));
        resolveFirst('slow');
    "#,
    );
    assert_eq!(lines, vec!["slow|fast"]);
}

// =====================================================================
// D. async / await の基本セマンティクス
// =====================================================================

#[test]
fn async_function_returns_promise() {
    assert!(eval_bool(
        r#"(() => { async function f() { return 1; } return typeof f().then === 'function'; })()"#
    ));
}

#[test]
fn async_function_runs_synchronously_until_first_await() {
    let lines = run_capture(
        r#"
        async function f() {
            console.log('f-start');
            await null;
            console.log('f-end');
        }
        console.log('main-start');
        f();
        console.log('main-end');
    "#,
    );
    assert_eq!(lines, vec!["main-start", "f-start", "main-end", "f-end"]);
}

#[test]
fn async_function_without_await_resolves_synchronously_but_then_is_still_a_microtask() {
    let lines = run_capture(
        r#"
        async function f() { return 'done'; }
        console.log('before');
        f().then(v => console.log('then', v));
        console.log('after');
    "#,
    );
    assert_eq!(lines, vec!["before", "after", "then done"]);
}

#[test]
fn await_non_promise_value_resolves_after_one_microtask() {
    let lines = run_capture(
        r#"
        async function f() {
            const v = await 42;
            console.log('got', v);
        }
        f();
        console.log('sync-after-call');
    "#,
    );
    assert_eq!(lines, vec!["sync-after-call", "got 42"]);
}

#[test]
fn await_rejected_promise_throws_catchable() {
    let lines = run_capture(
        r#"
        async function f() {
            try {
                await Promise.reject('bad');
            } catch (e) {
                console.log('caught', e);
            }
        }
        f();
    "#,
    );
    assert_eq!(lines, vec!["caught bad"]);
}

#[test]
fn await_rejected_promise_uncaught_rejects_returned_promise() {
    let lines = run_capture(
        r#"
        async function f() {
            await Promise.reject('bad');
        }
        f().catch(e => console.log('caught-outside', e));
    "#,
    );
    assert_eq!(lines, vec!["caught-outside bad"]);
}

#[test]
fn async_function_sync_throw_before_await_rejects_promise() {
    let lines = run_capture(
        r#"
        async function f() { throw new Error('immediate'); }
        f().catch(e => console.log('caught', e.message));
    "#,
    );
    assert_eq!(lines, vec!["caught immediate"]);
}

#[test]
fn multiple_sequential_awaits_run_in_order() {
    let lines = run_capture(
        r#"
        async function step(n) { await null; console.log('step', n); return n; }
        async function main() {
            await step(1);
            await step(2);
            await step(3);
        }
        main();
    "#,
    );
    assert_eq!(lines, vec!["step 1", "step 2", "step 3"]);
}

#[test]
fn await_promise_all_aggregates_values() {
    let lines = run_capture(
        r#"
        async function main() {
            const [a, b] = await Promise.all([Promise.resolve(1), Promise.resolve(2)]);
            console.log(a + b);
        }
        main();
    "#,
    );
    assert_eq!(lines, vec!["3"]);
}

#[test]
fn async_arrow_function_supports_await() {
    let lines = run_capture(
        r#"
        const f = async () => { await null; console.log('arrow-done'); };
        f();
    "#,
    );
    assert_eq!(lines, vec!["arrow-done"]);
}

#[test]
fn async_method_on_object_literal_supports_await() {
    let lines = run_capture(
        r#"
        const obj = { async run() { await null; console.log('method-done', this === obj); } };
        obj.run();
    "#,
    );
    assert_eq!(lines, vec!["method-done true"]);
}

#[test]
fn async_class_method_supports_await() {
    let lines = run_capture(
        r#"
        class C {
            async go() { await null; console.log('class-method-done'); }
        }
        new C().go();
    "#,
    );
    assert_eq!(lines, vec!["class-method-done"]);
}

#[test]
fn async_function_try_finally_runs_around_await() {
    let lines = run_capture(
        r#"
        async function f() {
            try {
                await null;
                console.log('try');
            } finally {
                console.log('finally');
            }
        }
        f();
    "#,
    );
    assert_eq!(lines, vec!["try", "finally"]);
}

#[test]
fn await_inside_loop_runs_each_iteration_in_order() {
    let lines = run_capture(
        r#"
        async function main() {
            for (let i = 0; i < 3; i++) {
                await null;
                console.log('i', i);
            }
        }
        main();
    "#,
    );
    assert_eq!(lines, vec!["i 0", "i 1", "i 2"]);
}

#[test]
fn top_level_await_blocks_module_completion_until_resolved() {
    let exports = run_ok(
        r#"
        async function delayed() { await null; return 99; }
        export const result = await delayed();
    "#,
    );
    assert_eq!(as_num(&export(&exports, "result")), 99.0);
}

#[test]
fn top_level_await_of_rejected_promise_fails_run_module() {
    let msg = run_err(
        r#"
        await Promise.reject(new Error('top-level-fail'));
    "#,
    );
    assert!(msg.contains("top-level-fail"), "got: {msg}");
}

#[test]
fn await_in_non_async_function_called_via_native_callback_is_a_clear_error() {
    // await はネイティブコールバック（ここでは Array.prototype.forEach が
    // Rust 側から再帰的に呼び出すコールバック）の内側では中断できない、
    // という T3 の既知の制約を確認する（`await` はパーサレベルでは
    // async 関数の外でも許容されるため、実行時エラーとして検出する）。
    let msg = run_err(
        r#"
        function notAsyncButAwaits() {
            await null;
        }
        [1].forEach(notAsyncButAwaits);
    "#,
    );
    assert!(msg.contains("await"), "got: {msg}");
}

// =====================================================================
// E. 並走する async 関数のフレーム分離
// =====================================================================

#[test]
fn two_concurrent_async_functions_interleave_independently() {
    let lines = run_capture(
        r#"
        async function a() {
            console.log('a1');
            await null;
            console.log('a2');
            await null;
            console.log('a3');
        }
        async function b() {
            console.log('b1');
            await null;
            console.log('b2');
        }
        a();
        b();
    "#,
    );
    // 起動順に同期部分が実行され (a1, b1)、以後はマイクロタスクキューの
    // FIFO 順で交互に再開する。
    assert_eq!(lines, vec!["a1", "b1", "a2", "b2", "a3"]);
}

#[test]
fn one_async_task_throwing_does_not_affect_the_other() {
    let lines = run_capture(
        r#"
        async function failing() {
            await null;
            throw new Error('task-a-failed');
        }
        async function ok() {
            await null;
            await null;
            console.log('task-b-ok');
        }
        failing().catch(e => console.log('caught', e.message));
        ok();
    "#,
    );
    assert!(lines.contains(&"caught task-a-failed".to_string()));
    assert!(lines.contains(&"task-b-ok".to_string()));
}

#[test]
fn recursive_async_function_via_await_completes() {
    let lines = run_capture(
        r#"
        async function countdown(n) {
            if (n <= 0) { console.log('done'); return; }
            await null;
            await countdown(n - 1);
        }
        countdown(3);
    "#,
    );
    assert_eq!(lines, vec!["done"]);
}

#[test]
fn nested_async_calls_return_correct_values() {
    let lines = run_capture(
        r#"
        async function inner(x) { await null; return x * 2; }
        async function outer(x) {
            const a = await inner(x);
            const b = await inner(a);
            console.log(b);
        }
        outer(3);
    "#,
    );
    assert_eq!(lines, vec!["12"]);
}

#[test]
fn many_concurrent_async_tasks_all_complete() {
    let lines = run_capture(
        r#"
        async function worker(id) {
            await null;
            console.log('worker', id);
        }
        for (let i = 0; i < 5; i++) {
            worker(i);
        }
    "#,
    );
    let mut sorted = lines.clone();
    sorted.sort();
    assert_eq!(
        sorted,
        vec!["worker 0", "worker 1", "worker 2", "worker 3", "worker 4"]
    );
}

// =====================================================================
// F. 未捕捉 reject の報告
// =====================================================================

#[test]
fn unhandled_rejection_hook_fires_when_nobody_catches() {
    let mut interp = Interpreter::new();
    let reported: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let reported2 = reported.clone();
    interp.set_unhandled_rejection_hook(move |v| {
        reported2.borrow_mut().push(format!("{v:?}"));
    });
    interp
        .run_module("Promise.reject('unhandled-here');", "test.ts")
        .unwrap();
    assert_eq!(reported.borrow().len(), 1);
}

#[test]
fn handled_rejection_does_not_fire_unhandled_hook() {
    let mut interp = Interpreter::new();
    let reported: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let reported2 = reported.clone();
    interp.set_unhandled_rejection_hook(move |v| {
        reported2.borrow_mut().push(format!("{v:?}"));
    });
    interp
        .run_module("Promise.reject('handled').catch(() => {});", "test.ts")
        .unwrap();
    assert_eq!(reported.borrow().len(), 0);
}

#[test]
fn late_catch_attached_before_drain_prevents_unhandled_report() {
    let mut interp = Interpreter::new();
    let reported: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let reported2 = reported.clone();
    interp.set_unhandled_rejection_hook(move |v| {
        reported2.borrow_mut().push(format!("{v:?}"));
    });
    interp
        .run_module(
            r#"
            const p = Promise.reject('e');
            // 同じマイクロタスク "tick" が終わる前に catch を付ける。
            p.catch(() => {});
            "#,
            "test.ts",
        )
        .unwrap();
    assert_eq!(reported.borrow().len(), 0);
}

// =====================================================================
// G. HostPromise / 仮想タイマー / run_event_loop
// =====================================================================

#[test]
fn host_promise_resolves_after_time_advances() {
    let mut interp = Interpreter::new();
    let clock = install_virtual_wait(&mut interp);
    let lines = capture_lines(&mut interp);
    interp
        .run_module(
            r#"
            async function main() {
                console.log('start');
                await wait(100);
                console.log('resumed');
            }
            main();
            "#,
            "test.ts",
        )
        .unwrap();
    assert_eq!(*lines.borrow(), vec!["start".to_string()]);
    let state = advance_time(&mut interp, &clock, 50);
    assert_eq!(state, PumpState::Waiting);
    assert_eq!(*lines.borrow(), vec!["start".to_string()]);
    let state = advance_time(&mut interp, &clock, 60);
    assert_eq!(state, PumpState::Idle);
    assert_eq!(*lines.borrow(), vec!["start", "resumed"]);
}

#[test]
fn run_module_returns_waiting_state_via_manual_pump_when_host_promise_pending() {
    let mut interp = Interpreter::new();
    let _clock = install_virtual_wait(&mut interp);
    interp.run_module("wait(1000);", "test.ts").unwrap();
    assert_eq!(interp.run_event_loop(), PumpState::Waiting);
}

#[test]
fn run_event_loop_idle_when_nothing_pending() {
    let mut interp = Interpreter::new();
    interp.run_module("1 + 1;", "test.ts").unwrap();
    assert_eq!(interp.run_event_loop(), PumpState::Idle);
}

#[test]
fn multiple_timers_fire_in_deadline_order() {
    let mut interp = Interpreter::new();
    let clock = install_virtual_wait(&mut interp);
    let lines = capture_lines(&mut interp);
    interp
        .run_module(
            r#"
            async function after(ms, label) { await wait(ms); console.log(label); }
            after(30, 'short');
            after(10, 'shorter');
            after(50, 'long');
            "#,
            "test.ts",
        )
        .unwrap();
    advance_time(&mut interp, &clock, 100);
    assert_eq!(*lines.borrow(), vec!["shorter", "short", "long"]);
}

#[test]
fn host_promise_reject_propagates_to_await_catch() {
    let mut interp = Interpreter::new();
    let waiter: Rc<RefCell<Option<HostPromise>>> = Rc::new(RefCell::new(None));
    let waiter2 = waiter.clone();
    interp.register_native_async("failable", 0, move |_vm, _this, _args, handle| {
        *waiter2.borrow_mut() = Some(handle);
    });
    let lines = capture_lines(&mut interp);
    interp
        .run_module(
            r#"
            async function main() {
                try {
                    await failable();
                } catch (e) {
                    console.log('caught', e);
                }
            }
            main();
            "#,
            "test.ts",
        )
        .unwrap();
    let h = waiter.borrow_mut().take().unwrap();
    h.reject(interp.vm(), Value::str("host-error"));
    interp.run_event_loop();
    assert_eq!(*lines.borrow(), vec!["caught host-error"]);
}

// =====================================================================
// H. Noiria パターンの再現
// =====================================================================

/// `Promise.race([wait(ms), waitClick()])` 相当: タイムアウトとユーザー
/// 入力のどちらが先に来るかで結果が変わる、Noiria の待機パターン。
fn install_wait_and_click(
    interp: &mut Interpreter,
) -> (Rc<RefCell<VirtualClock>>, Rc<RefCell<Option<HostPromise>>>) {
    let clock = install_virtual_wait(interp);
    let click_slot: Rc<RefCell<Option<HostPromise>>> = Rc::new(RefCell::new(None));
    let click_slot2 = click_slot.clone();
    interp.register_native_async("waitClick", 0, move |_vm, _this, _args, handle| {
        *click_slot2.borrow_mut() = Some(handle);
    });
    (clock, click_slot)
}

#[test]
fn race_wait_vs_click_timeout_wins_when_click_never_happens() {
    let mut interp = Interpreter::new();
    let (clock, _click) = install_wait_and_click(&mut interp);
    let lines = capture_lines(&mut interp);
    interp
        .run_module(
            r#"
            async function main() {
                const r = await Promise.race([wait(100).then(() => 'timeout'), waitClick().then(() => 'click')]);
                console.log(r);
            }
            main();
            "#,
            "test.ts",
        )
        .unwrap();
    advance_time(&mut interp, &clock, 150);
    assert_eq!(*lines.borrow(), vec!["timeout"]);
}

#[test]
fn race_wait_vs_click_click_wins_when_it_happens_first() {
    let mut interp = Interpreter::new();
    let (clock, click) = install_wait_and_click(&mut interp);
    let lines = capture_lines(&mut interp);
    interp
        .run_module(
            r#"
            async function main() {
                const r = await Promise.race([wait(100).then(() => 'timeout'), waitClick().then(() => 'click')]);
                console.log(r);
            }
            main();
            "#,
            "test.ts",
        )
        .unwrap();
    // タイマーが満期になる前にクリックを発生させる。
    advance_time(&mut interp, &clock, 10);
    let h = click.borrow_mut().take().unwrap();
    h.resolve(interp.vm(), Value::Undefined);
    interp.run_event_loop();
    assert_eq!(*lines.borrow(), vec!["click"]);
    // その後タイマーが満期になっても、race は既に確定しているので
    // 追加の出力は起きない。
    advance_time(&mut interp, &clock, 200);
    assert_eq!(*lines.borrow(), vec!["click"]);
}

#[test]
fn interruptible_wait_resolved_externally_discards_pending_timer() {
    // 「割り込みパターン」: 外部から明示的に resolve すると、待機自体が
    // その場で確定し、後から発火するはずだったタイマーの結果は捨てられる
    // （race の他方が既にどちらでも確定しているため副作用は無い）。
    let mut interp = Interpreter::new();
    let waiter: Rc<RefCell<Option<HostPromise>>> = Rc::new(RefCell::new(None));
    let waiter2 = waiter.clone();
    interp.register_native_async("waitForInterrupt", 0, move |_vm, _this, _args, handle| {
        *waiter2.borrow_mut() = Some(handle);
    });
    let lines = capture_lines(&mut interp);
    interp
        .run_module(
            r#"
            async function main() {
                const v = await waitForInterrupt();
                console.log('interrupted-with', v);
            }
            main();
            "#,
            "test.ts",
        )
        .unwrap();
    assert_eq!(interp.run_event_loop(), PumpState::Waiting);
    let h = waiter.borrow_mut().take().unwrap();
    h.resolve(interp.vm(), Value::str("cancel-signal"));
    interp.run_event_loop();
    assert_eq!(*lines.borrow(), vec!["interrupted-with cancel-signal"]);
}

#[test]
fn async_loop_with_wait_runs_multiple_iterations_across_time_advances() {
    let mut interp = Interpreter::new();
    let clock = install_virtual_wait(&mut interp);
    let lines = capture_lines(&mut interp);
    interp
        .run_module(
            r#"
            async function loop() {
                for (let i = 0; i < 3; i++) {
                    await wait(10);
                    console.log('tick', i);
                }
                console.log('loop-done');
            }
            loop();
            "#,
            "test.ts",
        )
        .unwrap();
    assert_eq!(*lines.borrow(), Vec::<String>::new());
    advance_time(&mut interp, &clock, 10);
    advance_time(&mut interp, &clock, 10);
    advance_time(&mut interp, &clock, 10);
    assert_eq!(
        *lines.borrow(),
        vec!["tick 0", "tick 1", "tick 2", "loop-done"]
    );
}

#[test]
fn two_async_loops_race_wait_run_concurrently_and_independently() {
    let mut interp = Interpreter::new();
    let clock = install_virtual_wait(&mut interp);
    let lines = capture_lines(&mut interp);
    interp
        .run_module(
            r#"
            async function loopA() {
                for (let i = 0; i < 2; i++) { await wait(10); console.log('A', i); }
            }
            async function loopB() {
                for (let i = 0; i < 2; i++) { await wait(15); console.log('B', i); }
            }
            loopA();
            loopB();
            "#,
            "test.ts",
        )
        .unwrap();
    advance_time(&mut interp, &clock, 10); // A0 が発火
    advance_time(&mut interp, &clock, 5); // B0 が発火 (合計 15)
    advance_time(&mut interp, &clock, 5); // A1 が発火 (合計 20)
    advance_time(&mut interp, &clock, 10); // B1 が発火 (合計 30)
    assert_eq!(*lines.borrow(), vec!["A 0", "B 0", "A 1", "B 1"]);
}

#[test]
fn wait_for_predicate_style_handler_registration() {
    // `waitFor(pred)` 型: 複数の待機者を登録しておき、条件成立時に
    // ホスト側が一括で resolve する（イベント配信パターン）。
    let mut interp = Interpreter::new();
    let waiters: Rc<RefCell<Vec<HostPromise>>> = Rc::new(RefCell::new(Vec::new()));
    let waiters2 = waiters.clone();
    interp.register_native_async("waitForFlag", 0, move |_vm, _this, _args, handle| {
        waiters2.borrow_mut().push(handle);
    });
    let lines = capture_lines(&mut interp);
    interp
        .run_module(
            r#"
            async function subscriber(id) {
                await waitForFlag();
                console.log('flagged', id);
            }
            subscriber(1);
            subscriber(2);
            subscriber(3);
            "#,
            "test.ts",
        )
        .unwrap();
    assert_eq!(*lines.borrow(), Vec::<String>::new());
    let pending: Vec<HostPromise> = std::mem::take(&mut *waiters.borrow_mut());
    assert_eq!(pending.len(), 3);
    for h in pending {
        h.resolve(interp.vm(), Value::Undefined);
    }
    interp.run_event_loop();
    let mut sorted = lines.borrow().clone();
    sorted.sort();
    assert_eq!(sorted, vec!["flagged 1", "flagged 2", "flagged 3"]);
}

#[test]
fn host_promise_pending_count_reflected_by_waiting_state_until_all_resolved() {
    let mut interp = Interpreter::new();
    let waiters: Rc<RefCell<Vec<HostPromise>>> = Rc::new(RefCell::new(Vec::new()));
    let waiters2 = waiters.clone();
    interp.register_native_async("hostOp", 0, move |_vm, _this, _args, handle| {
        waiters2.borrow_mut().push(handle);
    });
    interp.run_module("hostOp(); hostOp();", "test.ts").unwrap();
    assert_eq!(interp.run_event_loop(), PumpState::Waiting);
    let pending: Vec<HostPromise> = std::mem::take(&mut *waiters.borrow_mut());
    assert_eq!(pending.len(), 2);
    pending[0].resolve(interp.vm(), Value::Undefined);
    assert_eq!(interp.run_event_loop(), PumpState::Waiting);
    pending[1].resolve(interp.vm(), Value::Undefined);
    assert_eq!(interp.run_event_loop(), PumpState::Idle);
}

// =====================================================================
// I. T2 の妥協の解消: for-let の per-iteration キャプチャ
// =====================================================================

#[test]
fn for_let_closures_capture_per_iteration_binding() {
    let lines = run_capture(
        r#"
        const fns = [];
        for (let i = 0; i < 3; i++) {
            fns.push(() => console.log(i));
        }
        fns.forEach(f => f());
    "#,
    );
    assert_eq!(lines, vec!["0", "1", "2"]);
}

#[test]
fn for_var_closures_still_share_single_binding() {
    // 比較対象: `var` は従来通り共有セルのまま（本来の JS 仕様どおり）。
    let lines = run_capture(
        r#"
        const fns = [];
        for (var i = 0; i < 3; i++) {
            fns.push(() => console.log(i));
        }
        fns.forEach(f => f());
    "#,
    );
    assert_eq!(lines, vec!["3", "3", "3"]);
}

#[test]
fn for_let_async_closure_captures_per_iteration_value() {
    let lines = run_capture(
        r#"
        async function main() {
            const results = [];
            for (let i = 0; i < 3; i++) {
                await null;
                results.push(i);
            }
            console.log(results.join(','));
        }
        main();
    "#,
    );
    assert_eq!(lines, vec!["0,1,2"]);
}

#[test]
fn for_let_update_expression_sees_rebound_value_each_iteration() {
    // per-iteration リバインドが update 式の直前で起きることを、蓄積した
    // 合計値で確認する（各イテレーションで正しく 0,1,2 と増える）。
    assert_eq!(
        eval_num(
            r#"(() => {
                let sum = 0;
                for (let i = 0; i < 4; i++) { sum += i; }
                return sum;
            })()"#
        ),
        6.0
    );
}

// =====================================================================
// J. T2 の妥協の解消: computed member への複合代入
// =====================================================================

#[test]
fn computed_member_add_assign() {
    assert_eq!(
        eval_num(r#"(() => { const o = { a: 1 }; const k = 'a'; o[k] += 10; return o[k]; })()"#),
        11.0
    );
}

#[test]
fn computed_member_or_assign_short_circuits() {
    assert_eq!(
        eval_num(r#"(() => { const o = { a: 5 }; o['a'] ||= 99; return o.a; })()"#),
        5.0
    );
}

#[test]
fn computed_member_nullish_assign_applies_when_missing() {
    assert_eq!(
        eval_num(r#"(() => { const o = {}; o['a'] ??= 3; return o.a; })()"#),
        3.0
    );
}

#[test]
fn computed_member_compound_assign_evaluates_key_expression_once() {
    // key を計算する式が副作用を持つ場合、複合代入で二重評価されない
    // ことを確認する（一時ローカルに退避しているため）。
    let lines = run_capture(
        r#"
        const o = { a: 1 };
        let calls = 0;
        function key() { calls++; return 'a'; }
        o[key()] += 1;
        console.log(o.a, calls);
    "#,
    );
    assert_eq!(lines, vec!["2 1"]);
}

#[test]
fn computed_member_compound_assign_result_value_usable() {
    assert_eq!(
        eval_num(r#"(() => { const o = { a: 1 }; const r = (o['a'] += 4); return r; })()"#),
        5.0
    );
}

// =====================================================================
// K. 追加の順序・エラー伝播テスト
// =====================================================================

#[test]
fn error_thrown_synchronously_outside_promise_still_fails_module() {
    let msg = run_err("throw new Error('plain-sync');");
    assert!(msg.contains("plain-sync"));
}

#[test]
fn promise_chain_ordering_relative_to_synchronous_code_after_call() {
    let lines = run_capture(
        r#"
        function sync() { console.log('sync-fn'); }
        Promise.resolve().then(() => console.log('microtask'));
        sync();
        console.log('end');
    "#,
    );
    assert_eq!(lines, vec!["sync-fn", "end", "microtask"]);
}

#[test]
fn await_in_try_catch_does_not_leak_across_iterations() {
    let lines = run_capture(
        r#"
        async function main() {
            for (let i = 0; i < 3; i++) {
                try {
                    if (i === 1) throw new Error('skip-1');
                    await null;
                    console.log('ok', i);
                } catch (e) {
                    console.log('err', i);
                }
            }
        }
        main();
    "#,
    );
    assert_eq!(lines, vec!["ok 0", "err 1", "ok 2"]);
}

#[test]
fn async_function_returning_promise_directly_flattens_once() {
    let lines = run_capture(
        r#"
        async function inner() { return 5; }
        async function outer() { return inner(); }
        outer().then(v => console.log(v));
    "#,
    );
    assert_eq!(lines, vec!["5"]);
}

#[test]
fn console_log_ordering_with_mixed_sync_microtask_and_host_timer() {
    let mut interp = Interpreter::new();
    let clock = install_virtual_wait(&mut interp);
    let lines = capture_lines(&mut interp);
    interp
        .run_module(
            r#"
            console.log('1-sync');
            Promise.resolve().then(() => console.log('2-microtask'));
            async function withTimer() {
                await wait(5);
                console.log('4-timer');
            }
            withTimer();
            console.log('3-sync');
            "#,
            "test.ts",
        )
        .unwrap();
    assert_eq!(*lines.borrow(), vec!["1-sync", "3-sync", "2-microtask"]);
    advance_time(&mut interp, &clock, 10);
    assert_eq!(
        *lines.borrow(),
        vec!["1-sync", "3-sync", "2-microtask", "4-timer"]
    );
}
