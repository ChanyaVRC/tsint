//! Reproducible performance & allocation benchmark suite for `tsint`.
//!
//! Run with: `cargo bench --bench perf`

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tsint::compiler;
use tsint::parse_module;
use tsint::value::Value;
use tsint::vm::{HostPromise, PumpState};
use tsint::Interpreter;

// =====================================================================
// Zero-dependency allocation counter wrapping `System`
// =====================================================================

struct CountingAlloc;

static ALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);
static LIVE_BYTES: AtomicI64 = AtomicI64::new(0);
static PEAK_LIVE_BYTES: AtomicI64 = AtomicI64::new(0);

#[inline]
fn record_live_delta(delta: i64) {
    let cur = LIVE_BYTES.fetch_add(delta, Ordering::Relaxed) + delta;
    if delta > 0 {
        PEAK_LIVE_BYTES.fetch_max(cur, Ordering::Relaxed);
    }
}

// SAFETY: `CountingAlloc` delegates every allocation operation directly to `std::alloc::System`,
// which satisfies all `GlobalAlloc` layout, alignment, and thread-safety guarantees. The atomic
// counter updates (`Ordering::Relaxed`) are lock-free and never allocate, so re-entrancy cannot occur.
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        record_live_delta(layout.size() as i64);
        // SAFETY: Caller upholds `GlobalAlloc::alloc` preconditions (non-zero size layout).
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        record_live_delta(-(layout.size() as i64));
        // SAFETY: `ptr` was allocated by `System.alloc`/`System.realloc` via this allocator
        // with the same `layout`.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        record_live_delta(new_size as i64 - layout.size() as i64);
        // SAFETY: Caller upholds `GlobalAlloc::realloc` preconditions (`ptr` was allocated by
        // `System` with `layout`, and `new_size` is non-zero and valid for `layout.align()`).
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

fn reset_alloc_counters() {
    ALLOC_COUNT.store(0, Ordering::Relaxed);
    ALLOC_BYTES.store(0, Ordering::Relaxed);
    LIVE_BYTES.store(0, Ordering::Relaxed);
    PEAK_LIVE_BYTES.store(0, Ordering::Relaxed);
}

fn read_alloc_counters() -> (u64, u64, u64) {
    (
        ALLOC_COUNT.load(Ordering::Relaxed),
        ALLOC_BYTES.load(Ordering::Relaxed),
        PEAK_LIVE_BYTES.load(Ordering::Relaxed).max(0) as u64,
    )
}

// =====================================================================
// Benchmark harness
// =====================================================================

struct BenchResult {
    name: &'static str,
    median_us: f64,
    min_us: f64,
    allocs: u64,
    bytes: u64,
    peak_live: u64,
}

fn run_bench<F: FnMut()>(name: &'static str, warmup: usize, iters: usize, mut f: F) -> BenchResult {
    for _ in 0..warmup {
        f();
    }
    let mut times = Vec::with_capacity(iters);
    let mut last_allocs = 0u64;
    let mut last_bytes = 0u64;
    let mut last_peak = 0u64;
    for _ in 0..iters {
        reset_alloc_counters();
        let t0 = Instant::now();
        f();
        let dt = t0.elapsed();
        let (a, b, p) = read_alloc_counters();
        last_allocs = a;
        last_bytes = b;
        last_peak = p;
        times.push(dt);
    }
    times.sort();
    let median = times[iters / 2];
    let min = times[0];
    let to_us = |d: Duration| d.as_secs_f64() * 1_000_000.0;
    BenchResult {
        name,
        median_us: to_us(median),
        min_us: to_us(min),
        allocs: last_allocs,
        bytes: last_bytes,
        peak_live: last_peak,
    }
}

fn run_ts_compiled(src: &str) -> impl FnMut() {
    let module = parse_module(src, "<bench>").expect("parse");
    let chunk = compiler::compile_module(&module).expect("compile");
    move || {
        let mut interp = Interpreter::new();
        let res = interp.vm().run_module(chunk.clone()).expect("run");
        black_box(res);
    }
}

// =====================================================================
// Workload sources
// =====================================================================

const SRC_LOOP_ARITH: &str = r#"
function run(): number {
    let acc = 0;
    for (let i = 0; i < 100000; i++) {
        acc += (i * 3 - (i >> 1)) ^ (i & 255);
    }
    return acc;
}
export const result = run();
"#;

const SRC_FIB_CALLS: &str = r#"
function fib(n: number): number {
    if (n <= 1) return n;
    return fib(n - 1) + fib(n - 2);
}
export const result = fib(24);
"#;

const SRC_CLOSURE_CALLS: &str = r#"
function makeAdder(base: number) {
    let count = base;
    return (delta: number): number => {
        count += delta;
        return count;
    };
}
function run(): number {
    const add = makeAdder(10);
    let sum = 0;
    for (let i = 0; i < 40000; i++) {
        sum += add(i & 7);
    }
    return sum;
}
export const result = run();
"#;

const SRC_OBJ_PROPS: &str = r#"
class Entity {
    x: number = 0;
    y: number = 0;
    vx: number = 1;
    vy: number = 2;
    hp: number = 100;
    constructor(x: number, y: number) {
        this.x = x;
        this.y = y;
    }
    step(dt: number): number {
        this.x += this.vx * dt;
        this.y += this.vy * dt;
        this.hp -= 0.1;
        return this.x + this.y;
    }
}
function run(): number {
    const entities: Entity[] = [];
    for (let i = 0; i < 200; i++) {
        entities.push(new Entity(i, i * 2));
    }
    let total = 0;
    for (let frame = 0; frame < 100; frame++) {
        for (let i = 0; i < entities.length; i++) {
            total += entities[i].step(0.016);
        }
    }
    // Large object (> 16 props) lookup/update
    const dict: any = {};
    for (let i = 0; i < 64; i++) {
        dict["prop_" + i] = i;
    }
    for (let r = 0; r < 200; r++) {
        for (let i = 0; i < 64; i++) {
            const k = "prop_" + i;
            dict[k] = dict[k] + 1;
            total += dict[k];
        }
    }
    return total;
}
export const result = run();
"#;

const SRC_ARRAY_OPS: &str = r#"
function run(): number {
    const arr: number[] = [];
    for (let i = 0; i < 5000; i++) {
        arr.push(i);
    }
    let sum = 0;
    for (let r = 0; r < 10; r++) {
        for (let i = 0; i < arr.length; i++) {
            arr[i] = arr[i] + 1;
            sum += arr[i];
        }
    }
    const mapped = arr.map((x: number) => x * 2);
    const filtered = mapped.filter((x: number) => (x & 3) === 0);
    const sliced = filtered.slice(100, 900);
    const idx = sliced.indexOf(sliced[400]);
    const joined = sliced.slice(0, 100).join(",");
    return sum + idx + joined.length;
}
export const result = run();
"#;

const SRC_STRING_OPS: &str = r#"
function run(): number {
    let total = 0;
    const base = "  [chara name=\"alice\" pos=center expr=smile]こんにちは、Noiriaの世界へ！  ";
    for (let i = 0; i < 3000; i++) {
        const t = base.trim();
        if (t.startsWith("[chara") && t.endsWith("！")) {
            const eq = t.indexOf("=");
            const part = t.slice(eq + 1, eq + 12);
            const parts = t.split(" ");
            const rep = t.replace("alice", "bob").replaceAll("=", ":");
            const msg = `${part}:${parts.length}:${rep.length}:${i}`;
            total += msg.length + t.charCodeAt(1);
        }
    }
    return total;
}
export const result = run();
"#;

const SRC_REGEX_OPS: &str = r#"
function run(): number {
    const reTag = /^\[([a-zA-Z_][a-zA-Z0-9_]*)([^\]]*)\]/;
    const reAttr = /([a-zA-Z_][a-zA-Z0-9_]*)\s*=\s*(?:"([^"]*)"|([^\s]+))/g;
    const reLines = /\r\n|\r|\n/;
    const script = [
        "*start|Opening",
        "[bg storage=\"school_day.png\" time=500]",
        "[chara_show name=\"alice\" face=\"smile\" x=320 y=120]",
        "こんにちは！今日もいい天気ですね。[p]",
        "[eval exp=\"f.score = f.score + 10\"]",
        "@wait time=200",
        "[jump storage=\"scene2.ks\" target=\"*next\"]",
    ].join("\r\n");

    let count = 0;
    for (let iter = 0; iter < 500; iter++) {
        const lines = script.split(reLines);
        for (let i = 0; i < lines.length; i++) {
            const line = lines[i];
            const m = reTag.exec(line);
            if (m !== null) {
                count += m[1].length;
                const attrStr = m[2];
                const matches = attrStr.match(reAttr);
                if (matches !== null) {
                    count += matches.length;
                }
                const cleaned = attrStr.replace(reAttr, "$1:$2$3");
                count += cleaned.length;
            } else if (/^\*/.test(line)) {
                count += line.replace(/^\*/, "").length;
            }
        }
    }
    return count;
}
export const result = run();
"#;

const SRC_MAP_SET_OPS: &str = r#"
function run(): number {
    const m = new Map();
    const s = new Set();
    for (let i = 0; i < 600; i++) {
        const key = "key_" + i;
        m.set(key, i * 3);
        s.add(key);
    }
    let total = 0;
    for (let r = 0; r < 20; r++) {
        for (let i = 0; i < 600; i++) {
            const key = "key_" + i;
            if (s.has(key)) {
                const v = m.get(key);
                m.set(key, v + 1);
                total += v;
            }
        }
    }
    for (let i = 0; i < 300; i++) {
        const key = "key_" + i;
        m.delete(key);
        s.delete(key);
    }
    return total + m.size + s.size;
}
export const result = run();
"#;

const SRC_JSON_OPS: &str = r#"
function run(): number {
    const saveState = {
        version: 1,
        scene: "chapter01.ks",
        line: 428,
        flags: {
            visited: true,
            affection: 87,
            route: "true_end",
            name: "Alice \"Wonder\" \nLine2",
        },
        layers: [
            { id: 0, x: 0, y: 0, opacity: 255, visible: true, image: "bg_room.png" },
            { id: 1, x: 320, y: 120, opacity: 255, visible: true, image: "ch_alice.png" },
            { id: 2, x: 800, y: 140, opacity: 200, visible: false, image: "ch_bob.png" },
        ],
        history: [
            "おはようございます。",
            "今日は文化祭の準備ですね。",
            "教室に向かいましょう。",
        ],
    };
    let total = 0;
    for (let i = 0; i < 300; i++) {
        const text = JSON.stringify(saveState);
        const parsed = JSON.parse(text);
        total += text.length + parsed.layers.length + parsed.flags.affection;
    }
    return total;
}
export const result = run();
"#;

const SRC_ASYNC_AWAIT: &str = r#"
async function leaf(x: number): Promise<number> {
    return x + 1;
}
async function step(n: number): Promise<number> {
    const a = await leaf(n);
    const b = await Promise.resolve(a * 2);
    return b - 1;
}
async function run(): Promise<number> {
    let acc = 0;
    for (let i = 0; i < 500; i++) {
        acc += await step(i);
    }
    const batch = await Promise.all([step(1), step(2), step(3), step(4)]);
    return acc + batch[0] + batch[1] + batch[2] + batch[3];
}
export const result = await run();
"#;

const SRC_KAG_RUNTIME_SIM: &str = r#"
interface TagNode {
    type: "tag" | "text" | "label";
    name?: string;
    attrs?: Record<string, string>;
    text?: string;
}

class LayerState {
    id: number;
    x: number = 0;
    y: number = 0;
    opacity: number = 255;
    visible: boolean = true;
    image: string = "";
    constructor(id: number) {
        this.id = id;
    }
    apply(attrs: Record<string, string>) {
        if (attrs.x !== undefined) this.x = Number(attrs.x);
        if (attrs.y !== undefined) this.y = Number(attrs.y);
        if (attrs.opacity !== undefined) this.opacity = Number(attrs.opacity);
        if (attrs.storage !== undefined) this.image = attrs.storage;
        if (attrs.visible !== undefined) this.visible = attrs.visible !== "false";
    }
}

function parseKagLine(line: string): TagNode[] {
    const nodes: TagNode[] = [];
    const trimmed = line.trim();
    if (trimmed.length === 0 || trimmed.startsWith(";")) return nodes;
    if (trimmed.startsWith("*")) {
        nodes.push({ type: "label", name: trimmed.slice(1) });
        return nodes;
    }
    let i = 0;
    let buf = "";
    while (i < trimmed.length) {
        const ch = trimmed[i];
        if (ch === "[") {
            if (buf.length > 0) {
                nodes.push({ type: "text", text: buf });
                buf = "";
            }
            const close = trimmed.indexOf("]");
            const inner = close >= 0 ? trimmed.slice(i + 1, close) : trimmed.slice(i + 1);
            const parts = inner.trim().split(" ");
            const name = parts[0];
            const attrs: Record<string, string> = {};
            for (let p = 1; p < parts.length; p++) {
                const tok = parts[p];
                const eq = tok.indexOf("=");
                if (eq > 0) {
                    attrs[tok.slice(0, eq)] = tok.slice(eq + 1);
                } else if (tok.length > 0) {
                    attrs[tok] = "true";
                }
            }
            nodes.push({ type: "tag", name, attrs });
            if (close < 0) break;
            i = close + 1;
        } else {
            buf += ch;
            i++;
        }
    }
    if (buf.length > 0) {
        nodes.push({ type: "text", text: buf });
    }
    return nodes;
}

async function runScenario(): Promise<number> {
    const layers = new Map();
    for (let i = 0; i < 8; i++) {
        layers.set("layer" + i, new LayerState(i));
    }
    const vars: Record<string, number> = { score: 0, step: 0, mood: 5 };
    const lines = [
        "*scene1",
        "[bg storage=classroom.png opacity=255]",
        "[chara layer=layer1 storage=alice.png x=320 y=120]",
        "おはよう！今日もいい天気だね。",
        "[wait time=16]",
        "[chara layer=layer1 x=360 opacity=220]",
        "放課後に屋上で話そう。",
        "[wait time=16]",
    ];

    let renderedChars = 0;
    for (let loop = 0; loop < 60; loop++) {
        for (let li = 0; li < lines.length; li++) {
            const nodes = parseKagLine(lines[li]);
            for (let ni = 0; ni < nodes.length; ni++) {
                const node = nodes[ni];
                if (node.type === "tag") {
                    if (node.name === "bg" && node.attrs) {
                        layers.get("layer0").apply(node.attrs);
                    } else if (node.name === "chara" && node.attrs) {
                        const lname = node.attrs.layer || "layer1";
                        layers.get(lname).apply(node.attrs);
                    } else if (node.name === "wait") {
                        await host_tick();
                        vars.step += 1;
                    }
                } else if (node.type === "text" && node.text) {
                    renderedChars += node.text.length;
                    vars.score += 1;
                }
            }
        }
    }
    return renderedChars + vars.score + vars.step + layers.get("layer1").x;
}

export const result = await runScenario();
"#;

const SRC_GC_ALLOC_CHURN: &str = r#"
function run(): number {
    let checksum = 0;
    for (let i = 0; i < 4000; i++) {
        const a: any = { id: i, tag: "a", payload: [i, i + 1, i + 2] };
        const b: any = { id: i, tag: "b", peer: a };
        a.peer = b;
        checksum += a.id + b.peer.payload[1];
    }
    return checksum;
}
export const result = run();
"#;

fn build_large_ts_source() -> String {
    let mut out = String::with_capacity(64 * 1024);
    out.push_str(
        r#"
export interface LayerConfig {
    id: number;
    x: number;
    y: number;
    z: number;
    opacity: number;
    visible: boolean;
    storage?: string;
}
export type TransitionKind = "crossfade" | "universal" | "scroll";
"#,
    );
    for idx in 0..45 {
        out.push_str(&format!(
            r#"
export class Component{idx} {{
    readonly name: string = "comp_{idx}";
    private value: number = {idx};
    private items: Array<LayerConfig> = [];

    constructor(init: number) {{
        this.value = init + {idx};
    }}

    public update(dt: number, flag: boolean = true): number {{
        let sum: number = this.value;
        for (let i: number = 0; i < 10; i++) {{
            if (flag && (i & 1) === 0) {{
                sum += (i * dt) + 1.5;
            }} else {{
                sum -= i * 0.25;
            }}
        }}
        const obj = {{ a: sum, b: `val_${{this.name}}_${{sum}}`, c: [1, 2, 3] }};
        const {{ a, b }} = obj;
        switch (this.value & 3) {{
            case 0: return a + b.length;
            case 1: return a * 2;
            case 2: return a - 1;
            default: return a;
        }}
    }}

    public async runAsync(n: number): Promise<number> {{
        try {{
            const re = /^[a-z0-9_]+$/i;
            if (re.test(this.name)) {{
                return await Promise.resolve(this.update(n, true));
            }}
            return 0;
        }} catch (e) {{
            return -1;
        }}
    }}
}}
"#
        ));
    }
    out
}

fn main() {
    let large_ts = build_large_ts_source();

    let mut results = vec![
        run_bench("loop_arith", 2, 7, run_ts_compiled(SRC_LOOP_ARITH)),
        run_bench("fib_calls", 2, 7, run_ts_compiled(SRC_FIB_CALLS)),
        run_bench("closure_calls", 2, 7, run_ts_compiled(SRC_CLOSURE_CALLS)),
        run_bench("obj_props", 2, 7, run_ts_compiled(SRC_OBJ_PROPS)),
        run_bench("array_ops", 2, 7, run_ts_compiled(SRC_ARRAY_OPS)),
        run_bench("string_ops", 2, 7, run_ts_compiled(SRC_STRING_OPS)),
        run_bench("regex_ops", 2, 7, run_ts_compiled(SRC_REGEX_OPS)),
        run_bench("map_set_ops", 2, 7, run_ts_compiled(SRC_MAP_SET_OPS)),
        run_bench("json_ops", 2, 7, run_ts_compiled(SRC_JSON_OPS)),
        run_bench("async_await", 2, 7, run_ts_compiled(SRC_ASYNC_AWAIT)),
    ];

    // KAG runtime simulation with async host ticks
    {
        let module = parse_module(SRC_KAG_RUNTIME_SIM, "<kag_sim>").expect("parse");
        let chunk = compiler::compile_module(&module).expect("compile");
        results.push(run_bench("kag_runtime_sim", 2, 7, move || {
            use std::cell::RefCell;
            use std::rc::Rc;
            let pending: Rc<RefCell<Vec<HostPromise>>> = Rc::new(RefCell::new(Vec::new()));
            let p_clone = pending.clone();
            let mut interp = Interpreter::new();
            interp.register_native_async("host_tick", 0, move |_vm, _this, _args, hp| {
                p_clone.borrow_mut().push(hp);
            });
            let _ = interp.vm().start_module_task(chunk.clone()).expect("start");
            loop {
                let batch = std::mem::take(&mut *pending.borrow_mut());
                if batch.is_empty() {
                    break;
                }
                for hp in batch {
                    hp.resolve(interp.vm(), Value::Undefined);
                }
                if interp.run_event_loop() == PumpState::Idle && pending.borrow().is_empty() {
                    break;
                }
            }
            black_box(());
        }));
    }

    results.push(run_bench(
        "gc_alloc_churn",
        2,
        7,
        run_ts_compiled(SRC_GC_ALLOC_CHURN),
    ));

    // Startup: lex + parse + compile large TS source (~1800 lines)
    results.push(run_bench("parse_large_ts", 2, 7, || {
        let m = parse_module(&large_ts, "large.ts").expect("parse");
        black_box(m);
    }));
    {
        let parsed = parse_module(&large_ts, "large.ts").expect("parse");
        results.push(run_bench("compile_large_ts", 2, 7, || {
            let c = compiler::compile_module(&parsed).expect("compile");
            black_box(c);
        }));
    }
    results.push(run_bench("parse_compile_large_ts", 2, 7, || {
        let m = parse_module(&large_ts, "large.ts").expect("parse");
        let c = compiler::compile_module(&m).expect("compile");
        black_box(c);
    }));

    println!(
        "{:<24} {:>12} {:>12} {:>12} {:>14} {:>14}",
        "benchmark", "median (ms)", "min (ms)", "allocs", "alloc bytes", "peak live"
    );
    println!("{}", "-".repeat(93));
    let mut total_ms = 0.0;
    let mut total_allocs = 0u64;
    let mut total_bytes = 0u64;
    let mut total_peak = 0u64;
    for r in &results {
        let med_ms = r.median_us / 1000.0;
        let min_ms = r.min_us / 1000.0;
        total_ms += med_ms;
        total_allocs += r.allocs;
        total_bytes += r.bytes;
        total_peak += r.peak_live;
        println!(
            "{:<24} {:>12.3} {:>12.3} {:>12} {:>14} {:>14}",
            r.name, med_ms, min_ms, r.allocs, r.bytes, r.peak_live
        );
    }
    println!("{}", "-".repeat(93));
    println!(
        "{:<24} {:>12.3} {:>12} {:>12} {:>14} {:>14}",
        "TOTAL (sum)", total_ms, "", total_allocs, total_bytes, total_peak
    );

    print_type_sizes();
}

fn print_type_sizes() {
    use std::mem::size_of;
    let sizes: &[(&str, usize)] = &[
        ("Value", size_of::<Value>()),
        ("PropSlot", size_of::<tsint::value::PropSlot>()),
        ("PropMap", size_of::<tsint::value::PropMap>()),
        ("ObjData", size_of::<tsint::value::ObjData>()),
        ("ObjKind", size_of::<tsint::value::ObjKind>()),
        ("FunctionObj", size_of::<tsint::value::FunctionObj>()),
        ("NativeObj", size_of::<tsint::value::NativeObj>()),
        ("MapData", size_of::<tsint::value::MapData>()),
        ("SetData", size_of::<tsint::value::SetData>()),
        ("PromiseData", size_of::<tsint::value::PromiseData>()),
        ("ClassData", size_of::<tsint::value::ClassData>()),
        ("Stmt", size_of::<tsint::ast::Stmt>()),
        ("StmtKind", size_of::<tsint::ast::StmtKind>()),
        ("Expr", size_of::<tsint::ast::Expr>()),
        ("ExprKind", size_of::<tsint::ast::ExprKind>()),
        ("Pattern", size_of::<tsint::ast::Pattern>()),
        ("Function", size_of::<tsint::ast::Function>()),
        ("Class", size_of::<tsint::ast::Class>()),
        ("ClassMember", size_of::<tsint::ast::ClassMember>()),
        ("PropertyKey", size_of::<tsint::ast::PropertyKey>()),
        ("OpCode", size_of::<tsint::bytecode::OpCode>()),
        ("Const", size_of::<tsint::bytecode::Const>()),
        ("Chunk", size_of::<tsint::bytecode::Chunk>()),
        (
            "FunctionTemplate",
            size_of::<tsint::bytecode::FunctionTemplate>(),
        ),
        ("ClassTemplate", size_of::<tsint::bytecode::ClassTemplate>()),
        ("Frame", size_of::<tsint::vm::Frame>()),
        ("LocalSlot", size_of::<tsint::vm::LocalSlot>()),
        ("MicrotaskJob", size_of::<tsint::vm::MicrotaskJob>()),
        ("Vm", size_of::<tsint::vm::Vm>()),
        ("Token", size_of::<tsint::lexer::Token>()),
        ("TokenKind", size_of::<tsint::lexer::TokenKind>()),
        ("Regex", size_of::<tsint::regex::Regex>()),
    ];
    println!("\n=== Type Sizes (bytes) ===");
    for (name, sz) in sizes {
        println!("{name:<24} {sz:>6}");
    }
}
