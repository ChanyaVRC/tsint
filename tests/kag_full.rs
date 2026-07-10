//! T5a 最重要テスト: Noiria の実ファイル（`../noiria/runtime/api.ts` /
//! `../noiria/runtime/kag/*.ts`）を tsint のモジュールシステム
//! (`tsint::module`) 経由でそのままロードし、`(globalThis).Deno.core.ops`
//! をモック ops テーブルとして注入して KAG プレイヤーを実際に走らせる。
//!
//! `../noiria` が存在しない環境では skip する。

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;

use tsint::module::{ModuleHost, ModuleOutcome};
use tsint::value::Value;
use tsint::vm::{number_to_js_string, HostPromise, PumpState, Vm};
use tsint::Interpreter;

fn noiria_root() -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("noiria")
}

// =====================================================================
// ModuleHost: "noiria:*" は ../noiria/runtime 以下の実ファイル、それ以外
// (テストのエントリモジュール) はインメモリの合成ソースを解決する。
// =====================================================================

struct NoiriaHost {
    root: PathBuf,
    synthetic: HashMap<String, String>,
}

impl ModuleHost for NoiriaHost {
    fn resolve(&self, specifier: &str, referrer: &str) -> Result<String, String> {
        if let Some(rest) = specifier.strip_prefix("noiria:") {
            let rel = if rest == "kag" {
                "kag/mod.ts".to_string()
            } else {
                format!("{rest}.ts")
            };
            Ok(format!("noiria:{rel}"))
        } else if self.synthetic.contains_key(specifier) {
            Ok(specifier.to_string())
        } else {
            Err(format!(
                "解決できないモジュール指定子です: '{specifier}' (from '{referrer}')"
            ))
        }
    }

    fn load(&self, resolved: &str) -> Result<String, String> {
        if let Some(rest) = resolved.strip_prefix("noiria:") {
            let path = self.root.join(rest);
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))
        } else {
            self.synthetic
                .get(resolved)
                .cloned()
                .ok_or_else(|| format!("合成モジュールが見つかりません: {resolved}"))
        }
    }
}

// =====================================================================
// モック ops テーブル
// =====================================================================

struct VirtualClock {
    now: f64,
    timers: Vec<(f64, HostPromise)>,
}

/// テスト用モック ops の共有状態。呼び出しログ・仮想クロック・入力イベント
/// キュー・保留中のホスト Promise 群をまとめて持つ。
struct Harness {
    log: RefCell<Vec<String>>,
    next_layer_id: Cell<i64>,
    quit: Cell<bool>,
    text_totals: RefCell<HashMap<i64, usize>>,
    clock: RefCell<VirtualClock>,
    input_queue: RefCell<VecDeque<Value>>,
    input_pending: RefCell<Option<HostPromise>>,
    trans_waiters: RefCell<Vec<HostPromise>>,
    audio_waiters: RefCell<Vec<HostPromise>>,
    files: HashMap<String, String>,
}

fn arg(args: &[Value], i: usize) -> Value {
    args.get(i).cloned().unwrap_or(Value::Undefined)
}

/// ネイティブ引数を人間可読な文字列に落とす（ログ・アサーション用。
/// プレーンオブジェクト/配列は個別に中身を読むのでここでは扱わない）。
fn js_like(v: &Value) -> String {
    match v {
        Value::Str(s) => s.to_string(),
        Value::Number(n) => number_to_js_string(*n),
        Value::Bool(b) => b.to_string(),
        Value::Undefined => "undefined".to_string(),
        Value::Null => "null".to_string(),
        Value::Object(_) => "[object]".to_string(),
        Value::Uninit => "<uninit>".to_string(),
    }
}

fn obj_from_pairs(pairs: &[(&str, Value)]) -> Value {
    let o = Value::new_plain_object();
    if let Value::Object(rc) = &o {
        let mut b = rc.borrow_mut();
        for (k, v) in pairs {
            b.props.set_value(Rc::from(*k), v.clone());
        }
    }
    o
}

fn sync_native(
    name: &'static str,
    arity: usize,
    f: impl Fn(&mut Vm, Value, &[Value]) -> Result<Value, Value> + 'static,
) -> Value {
    Value::native_fn(name, arity, Rc::new(f))
}

fn async_native(
    name: &'static str,
    arity: usize,
    handler: impl Fn(&mut Vm, Value, &[Value], HostPromise) + 'static,
) -> Value {
    Value::native_fn(
        name,
        arity,
        Rc::new(move |vm, this, args| {
            let (promise_val, handle) = vm.new_host_promise();
            handler(vm, this, args, handle);
            Ok(promise_val)
        }),
    )
}

/// `(globalThis as any).Deno.core.ops` にモック ops を全部生やす。
fn install_mock_ops(interp: &mut Interpreter, h: &Rc<Harness>) {
    let ops_obj = Value::new_plain_object();
    macro_rules! op {
        ($name:literal, $value:expr) => {
            if let Value::Object(o) = &ops_obj {
                o.borrow_mut().props.set_value(Rc::from($name), $value);
            }
        };
    }

    // ---- レイヤ ----
    {
        let h = h.clone();
        op!(
            "op_layer_new",
            sync_native("op_layer_new", 1, move |_vm, _this, args| {
                let kind = js_like(&arg(args, 0));
                let id = h.next_layer_id.get();
                h.next_layer_id.set(id + 1);
                h.log.borrow_mut().push(format!("layer_new {kind} -> {id}"));
                Ok(Value::Number(id as f64))
            })
        );
    }
    {
        let h = h.clone();
        op!(
            "op_layer_free",
            sync_native("op_layer_free", 1, move |_vm, _this, args| {
                let id = js_like(&arg(args, 0));
                h.log.borrow_mut().push(format!("layer_free {id}"));
                Ok(Value::Undefined)
            })
        );
    }
    {
        let h = h.clone();
        op!(
            "op_layer_load_image",
            sync_native("op_layer_load_image", 3, move |_vm, _this, args| {
                let id = js_like(&arg(args, 0));
                let page = js_like(&arg(args, 1));
                let path = js_like(&arg(args, 2));
                h.log
                    .borrow_mut()
                    .push(format!("layer_load_image {id} {page} {path}"));
                Ok(Value::new_array(vec![
                    Value::Number(100.0),
                    Value::Number(100.0),
                ]))
            })
        );
    }
    {
        let h = h.clone();
        op!(
            "op_layer_clear_image",
            sync_native("op_layer_clear_image", 2, move |_vm, _this, args| {
                let id = js_like(&arg(args, 0));
                let page = js_like(&arg(args, 1));
                h.log
                    .borrow_mut()
                    .push(format!("layer_clear_image {id} {page}"));
                Ok(Value::Undefined)
            })
        );
    }
    {
        let h = h.clone();
        op!(
            "op_layer_fill",
            sync_native("op_layer_fill", 8, move |_vm, _this, args| {
                let id = js_like(&arg(args, 0));
                h.log.borrow_mut().push(format!("layer_fill {id}"));
                Ok(Value::Undefined)
            })
        );
    }
    {
        let h = h.clone();
        op!(
            "op_layer_msgbox",
            sync_native("op_layer_msgbox", 9, move |_vm, _this, args| {
                let id = js_like(&arg(args, 0));
                let page = js_like(&arg(args, 1));
                let w = js_like(&arg(args, 2));
                let hh = js_like(&arg(args, 3));
                h.log
                    .borrow_mut()
                    .push(format!("layer_msgbox {id} {page} {w}x{hh}"));
                Ok(Value::Undefined)
            })
        );
    }
    {
        let h = h.clone();
        op!(
            "op_layer_set_props",
            sync_native("op_layer_set_props", 3, move |vm, _this, args| {
                let id = js_like(&arg(args, 0));
                let page = js_like(&arg(args, 1));
                let props = arg(args, 2);
                let mut parts = Vec::new();
                for key in ["x", "y", "z", "opacity", "visible"] {
                    let v = vm.get_prop(&props, key)?;
                    if !matches!(v, Value::Undefined) {
                        parts.push(format!("{key}={}", js_like(&v)));
                    }
                }
                h.log.borrow_mut().push(format!(
                    "layer_set_props {id} {page} {{{}}}",
                    parts.join(",")
                ));
                Ok(Value::Undefined)
            })
        );
    }
    {
        let h = h.clone();
        op!(
            "op_backlay",
            sync_native("op_backlay", 1, move |_vm, _this, args| {
                let id = js_like(&arg(args, 0));
                h.log.borrow_mut().push(format!("backlay {id}"));
                Ok(Value::Undefined)
            })
        );
    }

    // ---- テキスト ----
    {
        let h = h.clone();
        op!(
            "op_text_style",
            sync_native("op_text_style", 2, move |_vm, _this, args| {
                let id = js_like(&arg(args, 0));
                h.log.borrow_mut().push(format!("text_style {id}"));
                Ok(Value::Undefined)
            })
        );
    }
    {
        let h = h.clone();
        op!(
            "op_text_print",
            sync_native("op_text_print", 4, move |vm, _this, args| {
                let id_num = match arg(args, 0) {
                    Value::Number(n) => n as i64,
                    _ => 0,
                };
                let segments = arg(args, 1);
                let append = matches!(arg(args, 2), Value::Bool(true));
                let cps = js_like(&arg(args, 3));
                let mut text = String::new();
                for seg in tsint::vm::array_values(&segments) {
                    let t = vm.get_prop(&seg, "text")?;
                    text.push_str(&js_like(&t));
                }
                let added = text.chars().count();
                let total = {
                    let mut totals = h.text_totals.borrow_mut();
                    if append {
                        let e = totals.entry(id_num).or_insert(0);
                        *e += added;
                        *e
                    } else {
                        totals.insert(id_num, added);
                        added
                    }
                };
                h.log.borrow_mut().push(format!(
                    "text_print {id_num} append={append} cps={cps} text={text:?}"
                ));
                Ok(obj_from_pairs(&[
                    ("total", Value::Number(total as f64)),
                    ("added", Value::Number(added as f64)),
                ]))
            })
        );
    }
    {
        let h = h.clone();
        op!(
            "op_text_complete",
            sync_native("op_text_complete", 1, move |_vm, _this, args| {
                let id = js_like(&arg(args, 0));
                h.log.borrow_mut().push(format!("text_complete {id}"));
                Ok(Value::Undefined)
            })
        );
    }
    {
        let h = h.clone();
        op!(
            "op_text_is_done",
            sync_native("op_text_is_done", 1, move |_vm, _this, args| {
                let id = js_like(&arg(args, 0));
                h.log.borrow_mut().push(format!("text_is_done {id}"));
                // モックは文字送りアニメーションを持たない: 常に「表示完了済み」。
                Ok(Value::Bool(true))
            })
        );
    }
    {
        let h = h.clone();
        op!(
            "op_text_clear",
            sync_native("op_text_clear", 1, move |_vm, _this, args| {
                let id = js_like(&arg(args, 0));
                h.log.borrow_mut().push(format!("text_clear {id}"));
                Ok(Value::Undefined)
            })
        );
    }
    {
        let h = h.clone();
        op!(
            "op_text_rects",
            sync_native("op_text_rects", 1, move |_vm, _this, args| {
                let id = js_like(&arg(args, 0));
                h.log.borrow_mut().push(format!("text_rects {id}"));
                // [link]...[endlink] で送った「はい」の UTF-8 バイト範囲
                // ([30, 36)) に一致する固定の矩形を 1 つ返す。
                let rect = obj_from_pairs(&[
                    ("x", Value::Number(100.0)),
                    ("y", Value::Number(20.0)),
                    ("w", Value::Number(60.0)),
                    ("h", Value::Number(30.0)),
                    ("byte_start", Value::Number(30.0)),
                    ("byte_end", Value::Number(36.0)),
                    ("line_i", Value::Number(0.0)),
                ]);
                Ok(Value::new_array(vec![rect]))
            })
        );
    }

    // ---- トランジション ----
    {
        let h = h.clone();
        op!(
            "op_trans_begin",
            sync_native("op_trans_begin", 1, move |vm, _this, args| {
                let a = arg(args, 0);
                let method = js_like(&vm.get_prop(&a, "method")?);
                let time_ms = js_like(&vm.get_prop(&a, "timeMs")?);
                h.log
                    .borrow_mut()
                    .push(format!("trans_begin {method} {time_ms}"));
                Ok(Value::Undefined)
            })
        );
    }
    {
        let h = h.clone();
        op!(
            "op_trans_skip",
            sync_native("op_trans_skip", 0, move |_vm, _this, _args| {
                h.log.borrow_mut().push("trans_skip".to_string());
                Ok(Value::Undefined)
            })
        );
    }
    {
        let h = h.clone();
        op!(
            "op_trans_wait",
            async_native("op_trans_wait", 0, move |_vm, _this, _args, handle| {
                h.log.borrow_mut().push("trans_wait".to_string());
                h.trans_waiters.borrow_mut().push(handle);
            })
        );
    }

    // ---- オーディオ ----
    {
        let h = h.clone();
        op!(
            "op_audio_play",
            sync_native("op_audio_play", 5, move |_vm, _this, args| {
                let ch = js_like(&arg(args, 0));
                let path = js_like(&arg(args, 1));
                let loop_ = js_like(&arg(args, 2));
                let vol = js_like(&arg(args, 3));
                let fade = js_like(&arg(args, 4));
                h.log.borrow_mut().push(format!(
                    "audio_play {ch} {path} loop={loop_} vol={vol} fade={fade}"
                ));
                Ok(Value::Undefined)
            })
        );
    }
    {
        let h = h.clone();
        op!(
            "op_audio_stop",
            sync_native("op_audio_stop", 2, move |_vm, _this, args| {
                let ch = js_like(&arg(args, 0));
                h.log.borrow_mut().push(format!("audio_stop {ch}"));
                Ok(Value::Undefined)
            })
        );
    }
    {
        let h = h.clone();
        op!(
            "op_audio_volume",
            sync_native("op_audio_volume", 2, move |_vm, _this, args| {
                let ch = js_like(&arg(args, 0));
                h.log.borrow_mut().push(format!("audio_volume {ch}"));
                Ok(Value::Undefined)
            })
        );
    }
    {
        let h = h.clone();
        op!(
            "op_audio_is_playing",
            sync_native("op_audio_is_playing", 1, move |_vm, _this, args| {
                let ch = js_like(&arg(args, 0));
                h.log.borrow_mut().push(format!("audio_is_playing {ch}"));
                Ok(Value::Bool(false))
            })
        );
    }
    {
        let h = h.clone();
        op!(
            "op_audio_wait_end",
            async_native("op_audio_wait_end", 1, move |_vm, _this, args, handle| {
                let ch = js_like(&arg(args, 0));
                h.log.borrow_mut().push(format!("audio_wait_end {ch}"));
                h.audio_waiters.borrow_mut().push(handle);
            })
        );
    }

    // ---- 動画 ----
    {
        let h = h.clone();
        op!(
            "op_video_play",
            sync_native("op_video_play", 3, move |_vm, _this, _args| {
                h.log.borrow_mut().push("video_play".to_string());
                Ok(Value::Undefined)
            })
        );
    }
    {
        let h = h.clone();
        op!(
            "op_video_stop",
            sync_native("op_video_stop", 0, move |_vm, _this, _args| {
                h.log.borrow_mut().push("video_stop".to_string());
                Ok(Value::Undefined)
            })
        );
    }
    {
        let h = h.clone();
        op!(
            "op_video_is_playing",
            sync_native("op_video_is_playing", 0, move |_vm, _this, _args| {
                h.log.borrow_mut().push("video_is_playing".to_string());
                Ok(Value::Bool(false))
            })
        );
    }

    // ---- システム ----
    {
        let h = h.clone();
        op!(
            "op_log",
            sync_native("op_log", 2, move |_vm, _this, args| {
                let level = js_like(&arg(args, 0));
                let msg = js_like(&arg(args, 1));
                h.log.borrow_mut().push(format!("log {level} {msg}"));
                Ok(Value::Undefined)
            })
        );
    }
    {
        let h = h.clone();
        op!(
            "op_now",
            sync_native("op_now", 0, move |_vm, _this, _args| {
                h.log.borrow_mut().push("now".to_string());
                Ok(Value::Number(0.0))
            })
        );
    }
    {
        let h = h.clone();
        op!(
            "op_wait",
            async_native("op_wait", 1, move |_vm, _this, args, handle| {
                let ms = match arg(args, 0) {
                    Value::Number(n) => n,
                    _ => 0.0,
                };
                h.log.borrow_mut().push(format!("wait {ms}"));
                let deadline = h.clock.borrow().now + ms;
                h.clock.borrow_mut().timers.push((deadline, handle));
            })
        );
    }
    {
        let h = h.clone();
        op!(
            "op_quit",
            sync_native("op_quit", 0, move |_vm, _this, _args| {
                h.log.borrow_mut().push("quit".to_string());
                h.quit.set(true);
                Ok(Value::Undefined)
            })
        );
    }
    {
        let h = h.clone();
        op!(
            "op_set_title",
            sync_native("op_set_title", 1, move |_vm, _this, args| {
                let t = js_like(&arg(args, 0));
                h.log.borrow_mut().push(format!("set_title {t}"));
                Ok(Value::Undefined)
            })
        );
    }
    {
        let h = h.clone();
        op!(
            "op_read_text",
            sync_native("op_read_text", 1, move |vm, _this, args| {
                let path = js_like(&arg(args, 0));
                h.log.borrow_mut().push(format!("read_text {path}"));
                match h.files.get(&path) {
                    Some(content) => Ok(Value::str(content.clone())),
                    None => {
                        Err(vm.make_error("Error", format!("ファイルが見つかりません: {path}")))
                    }
                }
            })
        );
    }
    {
        let h = h.clone();
        op!(
            "op_file_exists",
            sync_native("op_file_exists", 1, move |_vm, _this, args| {
                let path = js_like(&arg(args, 0));
                let exists = h.files.contains_key(&path);
                h.log.borrow_mut().push(format!("file_exists {path}"));
                Ok(Value::Bool(exists))
            })
        );
    }
    {
        let h = h.clone();
        op!(
            "op_save_write",
            sync_native("op_save_write", 2, move |_vm, _this, args| {
                let slot = js_like(&arg(args, 0));
                h.log.borrow_mut().push(format!("save_write {slot}"));
                Ok(Value::Undefined)
            })
        );
    }
    {
        let h = h.clone();
        op!(
            "op_save_read",
            sync_native("op_save_read", 1, move |_vm, _this, args| {
                let slot = js_like(&arg(args, 0));
                h.log.borrow_mut().push(format!("save_read {slot}"));
                // spec: op_save_read("sys") は null（そもそもテストでは常に null）。
                Ok(Value::Null)
            })
        );
    }
    {
        let h = h.clone();
        op!(
            "op_save_list",
            sync_native("op_save_list", 0, move |_vm, _this, _args| {
                h.log.borrow_mut().push("save_list".to_string());
                Ok(Value::new_array(vec![]))
            })
        );
    }
    {
        let h = h.clone();
        op!(
            "op_screenshot",
            sync_native("op_screenshot", 1, move |_vm, _this, args| {
                let slot = js_like(&arg(args, 0));
                h.log.borrow_mut().push(format!("screenshot {slot}"));
                Ok(Value::Undefined)
            })
        );
    }
    {
        let h = h.clone();
        op!(
            "op_quake",
            sync_native("op_quake", 3, move |_vm, _this, _args| {
                h.log.borrow_mut().push("quake".to_string());
                Ok(Value::Undefined)
            })
        );
    }
    {
        let h = h.clone();
        op!(
            "op_input_next_event",
            async_native("op_input_next_event", 0, move |vm, _this, _args, handle| {
                let popped = h.input_queue.borrow_mut().pop_front();
                match popped {
                    Some(ev) => handle.resolve(vm, ev),
                    None => *h.input_pending.borrow_mut() = Some(handle),
                }
            })
        );
    }

    let core_obj = Value::new_plain_object();
    if let Value::Object(o) = &core_obj {
        o.borrow_mut().props.set_value(Rc::from("ops"), ops_obj);
    }
    let deno_obj = Value::new_plain_object();
    if let Value::Object(o) = &deno_obj {
        o.borrow_mut().props.set_value(Rc::from("core"), core_obj);
    }
    let global_this = interp
        .vm()
        .globals
        .get("globalThis")
        .expect("globalThis がビルトインとして登録されているはず")
        .borrow()
        .clone();
    if let Value::Object(o) = &global_this {
        o.borrow_mut().props.set_value(Rc::from("Deno"), deno_obj);
    } else {
        panic!("globalThis がオブジェクトではありません");
    }
}

// =====================================================================
// 駆動ヘルパー
// =====================================================================

fn input_event(kind: &str, x: f64, y: f64, button: Option<&str>) -> Value {
    let mut pairs = vec![
        ("kind", Value::str(kind)),
        ("x", Value::Number(x)),
        ("y", Value::Number(y)),
        ("t", Value::Number(0.0)),
    ];
    if let Some(b) = button {
        pairs.push(("button", Value::str(b)));
    }
    obj_from_pairs(&pairs)
}

fn push_input_event(interp: &mut Interpreter, h: &Rc<Harness>, ev: Value) -> PumpState {
    let pending = h.input_pending.borrow_mut().take();
    match pending {
        Some(handle) => handle.resolve(interp.vm(), ev),
        None => h.input_queue.borrow_mut().push_back(ev),
    }
    interp.run_event_loop()
}

fn click(interp: &mut Interpreter, h: &Rc<Harness>, x: f64, y: f64) -> PumpState {
    push_input_event(interp, h, input_event("click", x, y, Some("left")))
}

fn advance(interp: &mut Interpreter, h: &Rc<Harness>, ms: f64) -> PumpState {
    let new_now = {
        let mut c = h.clock.borrow_mut();
        c.now += ms;
        c.now
    };
    let mut fired = {
        let mut c = h.clock.borrow_mut();
        let (fired, remaining): (Vec<_>, Vec<_>) = std::mem::take(&mut c.timers)
            .into_iter()
            .partition(|(deadline, _)| *deadline <= new_now);
        c.timers = remaining;
        fired
    };
    fired.sort_by(|(a, _), (b, _)| a.partial_cmp(b).unwrap());
    for (_, handle) in fired {
        handle.resolve(interp.vm(), Value::Undefined);
    }
    interp.run_event_loop()
}

fn resolve_trans(interp: &mut Interpreter, h: &Rc<Harness>) -> PumpState {
    let waiters: Vec<HostPromise> = std::mem::take(&mut *h.trans_waiters.borrow_mut());
    assert!(
        !waiters.is_empty(),
        "resolve_trans: 保留中の trans.wait() がありません"
    );
    for w in waiters {
        w.resolve(interp.vm(), Value::Undefined);
    }
    interp.run_event_loop()
}

/// `expected` が `log` の部分列（間に他のログが挟まっていてもよい・順序は
/// 保持される）として現れることを検証する。バックグラウンドの
/// `wait ...`/`text_is_done ...`/`text_rects ...` 等のノイズを気にせず、
/// 意味のある呼び出し列だけを検証するために部分列一致にしている。
///
/// 期待値の末尾が `*` の場合は前方一致（prefix）で比較する。メッセージ
/// ウィンドウの寸法など、将来の計算式変更で変わりうる具体的な数値を
/// アサートせずに「その呼び出しがこの位置で起きた」ことだけを検証したい
/// 場合に使う（例: `"layer_msgbox 3 fore *"`）。
fn assert_subsequence(log: &[String], expected: &[&str]) {
    fn matches(got: &str, want: &str) -> bool {
        match want.strip_suffix('*') {
            Some(prefix) => got.starts_with(prefix),
            None => got == want,
        }
    }

    let mut it = log.iter().enumerate();
    let mut last_idx = 0usize;
    for want in expected {
        loop {
            match it.next() {
                Some((idx, got)) if matches(got, want) => {
                    last_idx = idx;
                    break;
                }
                Some(_) => continue,
                None => panic!(
                    "op ログの部分列が一致しません。\n\
                     直前まで一致したインデックス: {last_idx}\n\
                     見つからなかった期待値: {want:?}\n\
                     期待列全体:\n{expected:#?}\n\
                     実ログ全体 ({} 件):\n{log:#?}",
                    log.len()
                ),
            }
        }
    }
}

fn new_harness(files: HashMap<String, String>) -> Rc<Harness> {
    Rc::new(Harness {
        log: RefCell::new(Vec::new()),
        next_layer_id: Cell::new(1),
        quit: Cell::new(false),
        text_totals: RefCell::new(HashMap::new()),
        clock: RefCell::new(VirtualClock {
            now: 0.0,
            timers: Vec::new(),
        }),
        input_queue: RefCell::new(VecDeque::new()),
        input_pending: RefCell::new(None),
        trans_waiters: RefCell::new(Vec::new()),
        audio_waiters: RefCell::new(Vec::new()),
        files,
    })
}

// =====================================================================
// テスト 1: KAG プレイヤーのフル完走シナリオ
// =====================================================================

#[test]
fn kag_player_runs_full_scenario_to_close_via_module_graph() {
    let root = noiria_root();
    if !root.exists() {
        eprintln!("skip: ../noiria が見つかりません ({})", root.display());
        return;
    }

    let start_ks = "\
*start|はじまり\n\
こんにちは[l]\n\
[image storage=room.png]\n\
[backlay][trans time=300][wt]\n\
選択肢です[link target=*after]はい[endlink][s]\n\
*after\n\
[eval exp=\"f.x = 1\"]\n\
[emb exp=\"f.x + 1\"]\n\
[if exp=\"f.x == 2\"]\n\
これは表示されません[l]\n\
[else]\n\
値は1です[l]\n\
[endif]\n\
[jump storage=part2.ks target=*next]\n";

    let part2_ks = "\
*next|つぎ\n\
ここは2つ目のファイルです[l]\n\
[close]\n";

    let mut files = HashMap::new();
    files.insert(
        "game.json".to_string(),
        r#"{"width":1280,"height":720}"#.to_string(),
    );
    files.insert("scenario/start.ks".to_string(), start_ks.to_string());
    files.insert("scenario/part2.ks".to_string(), part2_ks.to_string());

    let h = new_harness(files);
    let mut interp = Interpreter::new();
    install_mock_ops(&mut interp, &h);

    let mut synthetic = HashMap::new();
    synthetic.insert(
        "test:entry".to_string(),
        r#"
        import { KagPlayer } from "noiria:kag";
        const player = new KagPlayer({ charaLayerCount: 1 });
        await player.run("start.ks");
        "#
        .to_string(),
    );
    let host = NoiriaHost {
        root: root.join("runtime"),
        synthetic,
    };

    let outcome = interp
        .run_module_graph("test:entry", &host)
        .unwrap_or_else(|e| panic!("モジュールグラフの評価に失敗しました: {e}"));
    assert!(
        matches!(outcome, ModuleOutcome::Pending),
        "player.run() はゲームループを抱えるので Pending のまま残るはず"
    );

    let dump = || h.log.borrow().clone();

    // ---- 初期化 -> ラベル -> 最初のテキスト表示までは、モジュール
    // グラフのロード自体（ホストからの駆動なし）で完了しているはず。
    assert_subsequence(
        &dump(),
        &[
            "read_text game.json",
            "layer_new image -> 1",
            "layer_set_props 1 fore {z=0,visible=false}",
            "layer_set_props 1 back {z=0,visible=false}",
            "layer_new image -> 2",
            "layer_set_props 2 fore {z=10,visible=false}",
            "layer_set_props 2 back {z=10,visible=false}",
            "layer_new text -> 3",
            // msgbox の具体的な寸法 (幅/高さ) はフォントメトリクスからの計算式
            // に依存するので、値そのものではなく「msgbox が呼ばれたこと」だけ
            // を検証する（将来の寸法計算式変更で壊れないように）。
            "layer_msgbox 3 fore *",
            // x は左マージンの既定値で固定だが、y はメッセージウィンドウの
            // 高さ計算に連動するので前方一致に留める。
            "layer_set_props 3 fore {x=40,y=*",
            "text_style 3",
            "layer_set_props 3 fore {z=100,visible=true}",
            "save_read sys",
            "save_write sys",
            "text_print 3 append=true cps=35 text=\"こんにちは\"",
        ],
    );
    assert_eq!(interp.run_event_loop(), PumpState::Waiting);
    assert!(!h.quit.get());

    // [l]: クリックで送り -> [image][backlay][trans][wt] まで進む。
    assert_eq!(click(&mut interp, &h, 10.0, 10.0), PumpState::Waiting);
    assert_subsequence(
        &dump(),
        &[
            "text_print 3 append=true cps=35 text=\"こんにちは\"",
            "layer_load_image 1 fore assets/bg/room.png",
            "layer_set_props 1 fore {visible=true}",
            "backlay -1",
            "trans_begin crossfade 300",
            "trans_wait",
        ],
    );

    // [wt]: op_trans_wait を resolve -> 選択肢のテキストが出る。
    assert_eq!(resolve_trans(&mut interp, &h), PumpState::Waiting);
    assert_subsequence(
        &dump(),
        &[
            "trans_wait",
            "text_print 3 append=true cps=35 text=\"選択肢です\"",
            "text_print 3 append=true cps=35 text=\"はい\"",
        ],
    );

    // [link][s]: op_text_rects が返す矩形に合わせた座標でクリック。
    assert_eq!(click(&mut interp, &h, 170.0, 558.0), PumpState::Waiting);
    assert_subsequence(
        &dump(),
        &[
            "text_rects 3",
            // seVol の既定値が 80% になったため、実効音量は 1 * 0.8 = 0.8。
            "audio_play se assets/se/click.wav loop=false vol=0.8 fade=0",
            "text_clear 3",
        ],
    );

    // idleUntilJump の wait(60) を進めてジャンプを成立させる。
    assert_eq!(advance(&mut interp, &h, 60.0), PumpState::Waiting);
    assert_subsequence(
        &dump(),
        &[
            "text_clear 3",
            "save_write sys",
            "text_print 3 append=true cps=35 text=\"2\"",
            "text_print 3 append=true cps=35 text=\"値は1です\"",
        ],
    );
    // if の偽分岐（これは表示されません）は実行されていないこと。
    assert!(
        !dump().iter().any(|l| l.contains("これは表示されません")),
        "if の偽分岐が実行されてしまっています: {:#?}",
        dump()
    );

    // 2 個目の [l]: [jump] で part2.ks へ。
    assert_eq!(click(&mut interp, &h, 10.0, 10.0), PumpState::Waiting);
    assert_subsequence(
        &dump(),
        &[
            "text_print 3 append=true cps=35 text=\"値は1です\"",
            "text_clear 3",
            "read_text scenario/part2.ks",
            "save_write sys",
            "text_print 3 append=true cps=35 text=\"ここは2つ目のファイルです\"",
        ],
    );
    assert!(!h.quit.get());

    // 3 個目の [l]: [close] -> op_quit。
    let _ = click(&mut interp, &h, 10.0, 10.0);
    assert!(h.quit.get(), "op_quit が呼ばれていません:\n{:#?}", dump());
    assert_subsequence(
        &dump(),
        &[
            "text_print 3 append=true cps=35 text=\"ここは2つ目のファイルです\"",
            "quit",
        ],
    );

    let final_log = dump();
    println!(
        "=== kag_player_runs_full_scenario_to_close_via_module_graph: {} 件の ops 呼び出し ===",
        final_log.len()
    );
    assert!(
        final_log.len() >= 25,
        "ops 呼び出しが想定より少ない: {} 件",
        final_log.len()
    );
}

// =====================================================================
// テスト 2: games/sample/main.ts を縮約した形での起動テスト
// =====================================================================

#[test]
fn main_ts_like_entry_boots_kag_player_and_reaches_close() {
    let root = noiria_root();
    if !root.exists() {
        eprintln!("skip: ../noiria が見つかりません ({})", root.display());
        return;
    }

    let title_ks = "*start|Title\nテストゲームへようこそ[l]\n[close]\n";

    let mut files = HashMap::new();
    files.insert(
        "game.json".to_string(),
        r#"{"width":1280,"height":720}"#.to_string(),
    );
    files.insert("scenario/title.ks".to_string(), title_ks.to_string());

    let h = new_harness(files);
    let mut interp = Interpreter::new();
    install_mock_ops(&mut interp, &h);

    // games/sample/main.ts と同じ形（KagPlayer + sys.log + await run）。
    let mut synthetic = HashMap::new();
    synthetic.insert(
        "test:main".to_string(),
        r#"
        import { KagPlayer } from "noiria:kag";
        import { sys } from "noiria:api";

        sys.log("Noiria サンプルを起動します");

        const player = new KagPlayer({
          charaLayerCount: 1,
          autoWaitMs: 1800,
        });

        await player.run("title.ks");
        "#
        .to_string(),
    );
    let host = NoiriaHost {
        root: root.join("runtime"),
        synthetic,
    };

    let outcome = interp
        .run_module_graph("test:main", &host)
        .unwrap_or_else(|e| panic!("モジュールグラフの評価に失敗しました: {e}"));
    assert!(matches!(outcome, ModuleOutcome::Pending));

    assert!(
        h.log
            .borrow()
            .iter()
            .any(|l| l == "log info Noiria サンプルを起動します"),
        "sys.log の呼び出しが記録されていません: {:#?}",
        h.log.borrow()
    );

    // title.ks は `*start` -> テキスト -> `[l]` -> `[close]` の 4 コマンドだけ
    // なので、1 回のクリックで [l] を抜けてそのまま [close] まで到達する。
    let _ = click(&mut interp, &h, 10.0, 10.0);
    assert!(
        h.quit.get(),
        "op_quit が呼ばれていません: {:#?}",
        h.log.borrow()
    );
    assert!(h
        .log
        .borrow()
        .iter()
        .any(|l| l.contains("テストゲームへようこそ")));
}
