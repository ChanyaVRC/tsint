use super::*;
use crate::{compiler, parse_module, Interpreter};

fn compile(src: &str) -> Rc<Chunk> {
    let m = parse_module(src, "<test>").unwrap();
    compiler::compile_module(&m).unwrap()
}

fn run_exports(interp: &mut Interpreter, src: &str) -> Vec<(Rc<str>, Value)> {
    interp.run_module(src, "<test>").unwrap()
}

fn export_val(exports: &[(Rc<str>, Value)], name: &str) -> Value {
    exports
        .iter()
        .find(|(k, _)| &**k == name)
        .map(|(_, v)| v.clone())
        .unwrap()
}

// =================================================================
// Part 1: 非キャプチャローカル変数の非ボックス化テスト
// =================================================================

#[test]
fn uncaptured_locals_stay_direct_and_only_captured_slots_are_boxed() {
    let src = r#"
        export function demo(a: number, b: number) {
            let uncaptured = a + 10;
            let captured = b + 20;
            let alsoUncaptured = uncaptured * 2;
            const fn1 = () => captured;
            captured += 5;
            return fn1() + alsoUncaptured;
        }
        export const result = demo(3, 7);
    "#;
    let chunk = compile(src);

    let demo_tpl = chunk
        .consts
        .iter()
        .find_map(|c| match c {
            Const::FunctionTemplate(t) if &*t.name == "demo" => Some(t.clone()),
            _ => None,
        })
        .expect("demo FunctionTemplate should exist");

    let boxed_count = demo_tpl
        .chunk
        .captured_locals
        .iter()
        .filter(|&&c| c)
        .count();
    let direct_count = demo_tpl
        .chunk
        .captured_locals
        .iter()
        .filter(|&&c| !c)
        .count();
    assert_eq!(
        boxed_count, 1,
        "captured_locals: {:?}",
        demo_tpl.chunk.captured_locals
    );
    assert!(direct_count >= 5, "non-captured locals should stay Direct");

    let frame = Frame::new(
        demo_tpl.chunk.clone(),
        Rc::new(Vec::new()),
        Rc::new(Vec::new()),
    );
    assert_eq!(frame.locals.iter().filter(|s| s.is_boxed()).count(), 1);
    assert_eq!(
        frame.locals.iter().filter(|s| s.is_direct()).count(),
        direct_count
    );

    let mut interp = Interpreter::new();
    let exports = run_exports(&mut interp, src);
    match export_val(&exports, "result") {
        Value::Number(n) => assert_eq!(n, 58.0),
        other => panic!("expected 58, got {other:?}"),
    }
}

#[test]
fn uncaptured_for_let_loop_is_direct_and_captured_for_let_preserves_per_iter_binding() {
    let src = r#"
        function sumLoop(n: number) {
            let s = 0;
            for (let i = 0; i < n; i++) {
                s += i;
            }
            return s;
        }
        function captureLoop() {
            const fns: Array<() => number> = [];
            for (let i = 0; i < 5; i++) {
                fns.push(() => i);
            }
            return fns.map(f => f()).join(",");
        }
        export const sum = sumLoop(100);
        export const captured = captureLoop();
    "#;
    let chunk = compile(src);
    let sum_tpl = chunk
        .consts
        .iter()
        .find_map(|c| match c {
            Const::FunctionTemplate(t) if &*t.name == "sumLoop" => Some(t.clone()),
            _ => None,
        })
        .unwrap();
    assert!(
        sum_tpl.chunk.captured_locals.iter().all(|&c| !c),
        "sumLoop の全ローカルは非キャプチャ（Direct）であるべき: {:?}",
        sum_tpl.chunk.captured_locals
    );

    let cap_tpl = chunk
        .consts
        .iter()
        .find_map(|c| match c {
            Const::FunctionTemplate(t) if &*t.name == "captureLoop" => Some(t.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        cap_tpl.chunk.captured_locals.iter().filter(|&&c| c).count(),
        1,
        "captureLoop はループ変数 i のみ Boxed であるべき"
    );

    let mut interp = Interpreter::new();
    let exports = run_exports(&mut interp, src);
    match export_val(&exports, "sum") {
        Value::Number(n) => assert_eq!(n, 4950.0),
        other => panic!("expected 4950, got {other:?}"),
    }
    match export_val(&exports, "captured") {
        Value::Str(s) => assert_eq!(&*s, "0,1,2,3,4"),
        other => panic!("expected '0,1,2,3,4', got {other:?}"),
    }
}

#[test]
fn nested_upvalues_and_arrow_captured_this_work_with_mixed_slots() {
    let src = r#"
        function makeCounter(start: number) {
            let uncap = 100;
            let count = start;
            function mid() {
                let midUncap = 1;
                return () => {
                    count += midUncap;
                    return count;
                };
            }
            const inc = mid();
            return [inc(), inc(), uncap];
        }
        const obj = {
            x: 42,
            getX() {
                let localOnly = 8;
                const arrow = () => this.x;
                return arrow() + localOnly;
            },
        };
        export const res = makeCounter(10).join(",");
        export const thisRes = obj.getX();
    "#;
    let mut interp = Interpreter::new();
    let exports = run_exports(&mut interp, src);
    match export_val(&exports, "res") {
        Value::Str(s) => assert_eq!(&*s, "11,12,100"),
        other => panic!("unexpected res: {other:?}"),
    }
    match export_val(&exports, "thisRes") {
        Value::Number(n) => assert_eq!(n, 50.0),
        other => panic!("unexpected thisRes: {other:?}"),
    }
}

// =================================================================
// Part 2: 循環参照回収 (GC) テスト
// =================================================================

#[test]
fn gc_reclaims_1000_circular_objects_and_pairs_back_to_baseline() {
    let mut interp = Interpreter::new();
    interp.collect_garbage();
    let baseline = interp.vm().live_tracked_objects();

    let src = r#"
        for (let i = 0; i < 1000; i++) {
            const selfRef: any = { id: i };
            selfRef.self = selfRef;

            const a: any = { name: "a" };
            const b: any = { name: "b" };
            a.b = b;
            b.a = a;
        }
        export const done = true;
    "#;
    interp.vm().set_gc_threshold(100_000);
    let _ = run_exports(&mut interp, src);

    let before = interp.vm().live_tracked_objects();
    assert!(
        before >= baseline + 3000,
        "GC 前は 3000 個以上の循環オブジェクトが残存しているはず: before={before}, baseline={baseline}"
    );

    let collected = interp.collect_garbage();
    let after = interp.vm().live_tracked_objects();
    assert!(
        collected >= 3000,
        "3000 個以上の循環オブジェクトが回収されるべき: collected={collected}"
    );
    assert_eq!(
        after, baseline,
        "GC 後はベースラインまで完全に回収されるべき: after={after}, baseline={baseline}"
    );
}

#[test]
fn gc_reclaims_self_referencing_and_mutually_recursive_closures() {
    let mut interp = Interpreter::new();
    interp.collect_garbage();
    let baseline = interp.vm().live_tracked_objects();

    let src = r#"
        function makeSelf() {
            function f() {
                return f;
            }
            return f() === f;
        }
        function makeMutual() {
            function a(n: number): number {
                return n <= 0 ? 0 : b(n - 1);
            }
            function b(n: number): number {
                return n <= 0 ? 1 : a(n - 1);
            }
            return a(4) + b(4);
        }
        let ok = 0;
        for (let i = 0; i < 200; i++) {
            if (makeSelf()) ok += 1;
            ok += makeMutual();
        }
        export const total = ok;
    "#;
    interp.vm().set_gc_threshold(100_000);
    let exports = run_exports(&mut interp, src);
    match export_val(&exports, "total") {
        Value::Number(n) => assert_eq!(n, 400.0),
        other => panic!("expected 400, got {other:?}"),
    }

    let before = interp.vm().live_tracked_objects();
    assert!(
        before >= baseline + 600,
        "before={before}, baseline={baseline}"
    );
    let collected = interp.collect_garbage();
    let after = interp.vm().live_tracked_objects();
    assert!(collected >= 600, "collected={collected}");
    assert_eq!(after, baseline, "after={after}, baseline={baseline}");
}

#[test]
fn gc_reclaims_circular_arrays_classes_and_promises() {
    let mut interp = Interpreter::new();
    interp.collect_garbage();
    let baseline = interp.vm().live_tracked_objects();

    let src = r#"
        for (let i = 0; i < 100; i++) {
            const arr: any = [];
            arr.push(arr);

            class LocalClass {
                v = i;
                get() { return this.v; }
            }
            const inst = new LocalClass();
            (inst as any).self = inst;

            const p1: any = new Promise(() => {});
            p1.then(() => p1);

            let res: any;
            const p2: any = new Promise((r: any) => { res = r; });
            p2.res = res;

            const box: any = {};
            const p3 = Promise.resolve(box);
            box.p = p3;
        }
        export const done = true;
    "#;
    interp.vm().set_gc_threshold(100_000);
    let _ = run_exports(&mut interp, src);

    let before = interp.vm().live_tracked_objects();
    assert!(
        before >= baseline + 500,
        "before={before}, baseline={baseline}"
    );

    let collected = interp.collect_garbage();
    let after = interp.vm().live_tracked_objects();
    assert!(collected >= 500, "collected={collected}");
    assert_eq!(after, baseline, "after={after}, baseline={baseline}");
}

#[test]
fn automatic_gc_threshold_reclaims_cycles_during_execution() {
    let mut interp = Interpreter::new();
    interp.collect_garbage();
    let baseline = interp.vm().live_tracked_objects();

    let src = r#"
        for (let i = 0; i < 6000; i++) {
            const a: any = {};
            a.self = a;
        }
        export const done = true;
    "#;
    let _ = run_exports(&mut interp, src);

    let live_after_run = interp.vm().live_tracked_objects();
    assert!(
        live_after_run < baseline + 4000,
        "自動 GC が発火して生存オブジェクト数が抑えられているべき: live={live_after_run}, baseline={baseline}"
    );
    interp.collect_garbage();
    assert_eq!(interp.vm().live_tracked_objects(), baseline);
}

#[test]
fn gc_preserves_cyclic_objects_held_by_external_rust_value() {
    let mut interp = Interpreter::new();
    interp.collect_garbage();
    let baseline = interp.vm().live_tracked_objects();

    let src = r#"
        const a: any = { tag: "keep-a" };
        const b: any = { tag: "keep-b" };
        a.b = b;
        b.a = a;

        {
            const trash: any = { tag: "trash" };
            trash.self = trash;
        }
        export const held = a;
    "#;
    let exports = run_exports(&mut interp, src);
    let held = export_val(&exports, "held");
    drop(exports);

    let collected = interp.collect_garbage();
    assert!(collected >= 1, "trash は回収されるべき");

    let tag_a = interp.vm().get_prop(&held, "tag").unwrap();
    let b_val = interp.vm().get_prop(&held, "b").unwrap();
    let tag_b = interp.vm().get_prop(&b_val, "tag").unwrap();
    let back_a = interp.vm().get_prop(&b_val, "a").unwrap();
    assert!(matches!(tag_a, Value::Str(ref s) if &**s == "keep-a"));
    assert!(matches!(tag_b, Value::Str(ref s) if &**s == "keep-b"));
    assert!(strict_eq(&back_a, &held));

    drop(back_a);
    drop(b_val);
    drop(held);
    let collected2 = interp.collect_garbage();
    assert_eq!(
        collected2, 2,
        "外部参照解放後は a と b の 2 個が回収されるべき"
    );
    assert_eq!(interp.vm().live_tracked_objects(), baseline);
}

#[test]
fn gc_preserves_suspended_async_task_waiting_on_external_host_promise() {
    let mut interp = Interpreter::new();
    let saved_handle: Rc<RefCell<Option<HostPromise>>> = Rc::new(RefCell::new(None));
    let h_clone = saved_handle.clone();
    interp.register_native_async("waitHost", 0, move |_vm, _this, _args, handle| {
        *h_clone.borrow_mut() = Some(handle);
    });

    let src = r#"
        export let observed = 0;
        async function worker() {
            const localObj: any = { bonus: 7 };
            localObj.self = localObj;
            const v = await waitHost();
            observed = v + localObj.bonus;
        }
        worker();
        for (let i = 0; i < 50; i++) {
            const g: any = {};
            g.self = g;
        }
    "#;
    let _ = run_exports(&mut interp, src);
    assert!(saved_handle.borrow().is_some());

    let collected = interp.collect_garbage();
    assert!(collected >= 50);

    let handle = saved_handle.borrow_mut().take().unwrap();
    handle.resolve(interp.vm(), Value::Number(35.0));
    assert_eq!(interp.run_event_loop(), PumpState::Idle);

    let mut interp2 = Interpreter::new();
    let saved2: Rc<RefCell<Option<HostPromise>>> = Rc::new(RefCell::new(None));
    let h2 = saved2.clone();
    interp2.register_native_async("waitHost", 0, move |_vm, _this, _args, handle| {
        *h2.borrow_mut() = Some(handle);
    });
    let box_obj = Value::new_plain_object();
    interp2.define_global("outBox", box_obj.clone());
    let _ = run_exports(
        &mut interp2,
        r#"
            async function worker() {
                const localObj: any = { bonus: 7 };
                localObj.self = localObj;
                const v = await waitHost();
                outBox.result = v + localObj.bonus;
            }
            worker();
        "#,
    );
    interp2.collect_garbage();
    saved2
        .borrow_mut()
        .take()
        .unwrap()
        .resolve(interp2.vm(), Value::Number(35.0));
    assert_eq!(interp2.run_event_loop(), PumpState::Idle);
    let res = interp2.vm().get_prop(&box_obj, "result").unwrap();
    assert!(matches!(res, Value::Number(n) if n == 42.0), "got {res:?}");
}
