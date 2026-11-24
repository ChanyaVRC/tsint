//! マーク＋厳密外部参照カウント検査（trial-deletion）による循環参照回収 GC。

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use crate::hash::{FastHashMap, FastHashSet};
use crate::value::*;

use super::{Frame, LocalSlot, MicrotaskJob, Vm, GC_MIN_THRESHOLD};

impl Vm {
    /// オブジェクトを GC 追跡リストに登録する。
    pub fn track_object(&self, obj: &ObjRef) {
        self.gc_objects.borrow_mut().push(Rc::downgrade(obj));
        self.gc_allocs_since_last
            .set(self.gc_allocs_since_last.get() + 1);
    }

    /// 未追跡（`Rc::weak_count == 0`）のオブジェクトとその配下の未追跡オブジェクトを
    /// GC 追跡リストへ登録する。既に追跡済みのノードは O(1) でスキップする。
    #[inline]
    pub fn track_value_tree(&self, v: &Value) {
        let Value::Object(root) = v else {
            return;
        };
        if Rc::weak_count(root) > 0 {
            return;
        }
        self.track_value_tree_slow(root);
    }

    fn track_value_tree_slow(&self, root: &ObjRef) {
        let mut stack = vec![root.clone()];
        while let Some(obj) = stack.pop() {
            if Rc::weak_count(&obj) > 0 {
                continue;
            }
            self.track_object(&obj);
            if let Ok(b) = obj.try_borrow() {
                gc_visit_obj_data(&b, None, &mut |child| {
                    if Rc::weak_count(child) == 0 {
                        stack.push(child.clone());
                    }
                });
            }
        }
    }

    /// 現在 GC 追跡リスト上で生存しているオブジェクト数を返す（テスト・診断用）。
    pub fn live_tracked_objects(&self) -> usize {
        let mut seen = FastHashSet::default();
        self.gc_objects
            .borrow()
            .iter()
            .filter(|w| w.strong_count() > 0 && seen.insert(w.as_ptr() as usize))
            .count()
    }

    /// 自動 GC のトリガー閾値を設定する（テスト・チューニング用）。
    pub fn set_gc_threshold(&mut self, threshold: usize) {
        self.gc_threshold = threshold.max(1);
    }

    #[inline]
    pub(super) fn maybe_collect_garbage(&mut self) {
        if self.gc_allocs_since_last.get() >= self.gc_threshold {
            self.collect_garbage();
        }
    }

    #[inline]
    pub(super) fn maybe_gc_if_top_level(&mut self) {
        if self.native_call_depth == 0 {
            self.maybe_collect_garbage();
        }
    }

    /// マーク＋厳密外部参照カウント検査（trial-deletion）による循環参照回収。
    pub fn collect_garbage(&mut self) -> usize {
        // ---- Phase 1: VM ルートからのマーク ----
        let tracked_hint = self.gc_objects.borrow().len();
        let mut marked: FastHashSet<usize> =
            FastHashSet::with_capacity_and_hasher(tracked_hint.min(4096), Default::default());
        let mut worklist: Vec<ObjRef> = Vec::with_capacity(64);

        for frame in &self.frames {
            gc_visit_frame_raw(frame, &mut |o| gc_mark_obj(o, &mut marked, &mut worklist));
        }
        for cell in self.globals.values() {
            if let Ok(v) = cell.try_borrow() {
                gc_mark_value(&v, &mut marked, &mut worklist);
            }
        }
        if let Some(exports) = &self.pending_module_exports {
            for (_, v) in exports {
                gc_mark_value(v, &mut marked, &mut worklist);
            }
        }
        for job in &self.microtasks {
            match job {
                MicrotaskJob::Then {
                    on_fulfilled,
                    on_rejected,
                    result_promise,
                    outcome,
                } => {
                    if let Some(v) = on_fulfilled {
                        gc_mark_value(v, &mut marked, &mut worklist);
                    }
                    if let Some(v) = on_rejected {
                        gc_mark_value(v, &mut marked, &mut worklist);
                    }
                    gc_mark_obj(result_promise, &mut marked, &mut worklist);
                    match outcome {
                        Ok(v) | Err(v) => gc_mark_value(v, &mut marked, &mut worklist),
                    }
                }
                MicrotaskJob::ResolveThenable {
                    promise,
                    thenable,
                    then_fn,
                } => {
                    gc_mark_obj(promise, &mut marked, &mut worklist);
                    gc_mark_value(thenable, &mut marked, &mut worklist);
                    gc_mark_value(then_fn, &mut marked, &mut worklist);
                }
                MicrotaskJob::ResumeAwait { cont, outcome } => {
                    for frame in &cont.frames {
                        gc_visit_frame_raw(frame, &mut |o| {
                            gc_mark_obj(o, &mut marked, &mut worklist)
                        });
                    }
                    gc_mark_obj(&cont.owner_promise, &mut marked, &mut worklist);
                    match outcome {
                        Ok(v) | Err(v) => gc_mark_value(v, &mut marked, &mut worklist),
                    }
                }
            }
        }
        for p in &self.maybe_unhandled {
            gc_mark_obj(p, &mut marked, &mut worklist);
        }
        if let Some(p) = &self.current_owner_promise {
            gc_mark_obj(p, &mut marked, &mut worklist);
        }

        while let Some(obj) = worklist.pop() {
            gc_visit_obj_outgoing(&obj, &mut |target| {
                gc_mark_obj(target, &mut marked, &mut worklist);
            });
        }

        // ---- Phase 2: 死亡済み弱参照の除去と未マーク候補の収集（in-place 圧縮） ----
        let mut live_weaks = std::mem::take(&mut *self.gc_objects.borrow_mut());
        let mut seen_weaks: FastHashSet<usize> = FastHashSet::default();
        let mut candidates: Vec<(usize, ObjRef)> = Vec::new();
        let mut candidate_idx: FastHashMap<usize, usize> = FastHashMap::default();

        live_weaks.retain(|w| {
            let Some(rc) = w.upgrade() else {
                return false;
            };
            let ptr = Rc::as_ptr(&rc) as usize;
            if w.weak_count() > 1 && !seen_weaks.insert(ptr) {
                return false;
            }
            if !marked.contains(&ptr) {
                let idx = candidates.len();
                candidate_idx.insert(ptr, idx);
                candidates.push((ptr, rc));
            }
            true
        });

        let finish_weaks = |weaks: &mut Vec<Weak<RefCell<ObjData>>>| -> usize {
            let live_count = weaks.len();
            let cap_limit = live_count.saturating_mul(4).max(GC_MIN_THRESHOLD);
            if weaks.capacity() > cap_limit {
                weaks.shrink_to(live_count.saturating_mul(2).max(GC_MIN_THRESHOLD));
            }
            live_count
        };

        if candidates.is_empty() {
            let live_count = finish_weaks(&mut live_weaks);
            *self.gc_objects.borrow_mut() = live_weaks;
            self.gc_allocs_since_last.set(0);
            self.gc_threshold = (live_count * 2).max(GC_MIN_THRESHOLD);
            return 0;
        }

        // ---- Phase 3: 候補集合内部の参照カウントを差し引く（trial-deletion） ----
        let mut external_refs: Vec<usize> = candidates
            .iter()
            .map(|(_, rc)| Rc::strong_count(rc).saturating_sub(1))
            .collect();

        let mut dedup = GcDedupState::default();

        for (i, (_, rc)) in candidates.iter().enumerate() {
            let Ok(b) = rc.try_borrow() else {
                external_refs[i] = external_refs[i].saturating_add(1);
                continue;
            };
            gc_visit_obj_data(&b, Some(&mut dedup), &mut |target| {
                let tptr = Rc::as_ptr(target) as usize;
                if let Some(&tidx) = candidate_idx.get(&tptr) {
                    debug_assert!(external_refs[tidx] > 0);
                    external_refs[tidx] = external_refs[tidx].saturating_sub(1);
                }
            });
        }

        // ---- Phase 4: 外部参照が残る候補から到達可能なものをすべて生存保護 ----
        let mut externally_alive = vec![false; candidates.len()];
        let mut ext_worklist: Vec<usize> = Vec::new();
        for (i, &ext) in external_refs.iter().enumerate() {
            if ext > 0 {
                externally_alive[i] = true;
                ext_worklist.push(i);
            }
        }
        while let Some(i) = ext_worklist.pop() {
            gc_visit_obj_outgoing(&candidates[i].1, &mut |target| {
                let tptr = Rc::as_ptr(target) as usize;
                if let Some(&j) = candidate_idx.get(&tptr) {
                    if !externally_alive[j] {
                        externally_alive[j] = true;
                        ext_worklist.push(j);
                    }
                }
            });
        }

        // ---- Phase 5: 完全に到達不能な循環ゴミの参照を切断して解放 ----
        let mut collected = 0usize;
        for (i, (_, rc)) in candidates.iter().enumerate() {
            if !externally_alive[i] {
                if let Ok(mut b) = rc.try_borrow_mut() {
                    b.props.clear();
                    b.proto = None;
                    b.kind = ObjKind::Plain;
                    collected += 1;
                }
            }
        }
        drop(candidates);
        live_weaks.retain(|w| w.strong_count() > 0);

        let live_count = finish_weaks(&mut live_weaks);
        *self.gc_objects.borrow_mut() = live_weaks;
        self.gc_allocs_since_last.set(0);
        self.gc_threshold = (live_count * 2).max(GC_MIN_THRESHOLD);
        collected
    }
}

fn gc_mark_obj(obj: &ObjRef, marked: &mut FastHashSet<usize>, worklist: &mut Vec<ObjRef>) {
    let ptr = Rc::as_ptr(obj) as usize;
    if marked.insert(ptr) {
        worklist.push(obj.clone());
    }
}

fn gc_mark_value(v: &Value, marked: &mut FastHashSet<usize>, worklist: &mut Vec<ObjRef>) {
    if let Value::Object(o) = v {
        gc_mark_obj(o, marked, worklist);
    }
}

fn gc_visit_value(v: &Value, f: &mut impl FnMut(&ObjRef)) {
    if let Value::Object(o) = v {
        f(o);
    }
}

#[derive(Default)]
struct GcDedupState {
    seen_fns: FastHashSet<usize>,
    seen_cells: FastHashSet<usize>,
    seen_raw_args: FastHashSet<usize>,
}

fn gc_visit_cell(
    cell: &Rc<RefCell<Value>>,
    dedup: Option<&mut GcDedupState>,
    f: &mut impl FnMut(&ObjRef),
) {
    if let Some(d) = dedup {
        if !d.seen_cells.insert(Rc::as_ptr(cell) as usize) {
            return;
        }
    }
    if let Ok(v) = cell.try_borrow() {
        gc_visit_value(&v, f);
    }
}

fn gc_visit_fn_upvalues(
    fo: &Rc<FunctionObj>,
    mut dedup: Option<&mut GcDedupState>,
    f: &mut impl FnMut(&ObjRef),
) {
    if let Some(d) = dedup.as_deref_mut() {
        if !d.seen_fns.insert(Rc::as_ptr(fo) as usize) {
            return;
        }
    }
    for cell in fo.upvalues.iter() {
        gc_visit_cell(cell, dedup.as_deref_mut(), f);
    }
}

fn gc_visit_frame(
    frame: &Frame,
    mut dedup: Option<&mut GcDedupState>,
    f: &mut impl FnMut(&ObjRef),
) {
    for slot in &frame.locals {
        match slot {
            LocalSlot::Direct(v) => gc_visit_value(v, f),
            LocalSlot::Boxed(cell) => gc_visit_cell(cell, dedup.as_deref_mut(), f),
        }
    }
    for cell in frame.upvalues.iter() {
        gc_visit_cell(cell, dedup.as_deref_mut(), f);
    }
    for v in &frame.stack {
        gc_visit_value(v, f);
    }
    let visit_raw_args = match dedup {
        Some(d) => d.seen_raw_args.insert(Rc::as_ptr(&frame.raw_args) as usize),
        None => true,
    };
    if visit_raw_args {
        for v in frame.raw_args.iter() {
            gc_visit_value(v, f);
        }
    }
}

fn gc_visit_frame_raw(frame: &Frame, f: &mut impl FnMut(&ObjRef)) {
    gc_visit_frame(frame, None, f);
}

fn gc_visit_props_and_proto(b: &ObjData, f: &mut impl FnMut(&ObjRef)) {
    for (_, slot) in b.props.iter() {
        match slot {
            PropSlot::Value(v) => gc_visit_value(v, f),
            PropSlot::Accessor(acc) => {
                if let Some(v) = &acc.get {
                    gc_visit_value(v, f);
                }
                if let Some(v) = &acc.set {
                    gc_visit_value(v, f);
                }
            }
        }
    }
    if let Some(proto) = &b.proto {
        f(proto);
    }
}

fn gc_visit_obj_outgoing(obj: &ObjRef, f: &mut impl FnMut(&ObjRef)) {
    let Ok(b) = obj.try_borrow() else {
        return;
    };
    gc_visit_obj_data(&b, None, f);
}

/// `ObjData` の出エッジをすべて訪問する。`dedup` が `Some` の場合は共有される
/// `FunctionObj` / セル / `raw_args` をポインタ単位で重複排除して trial-deletion の
/// 二重減算を防ぐ。
fn gc_visit_obj_data(
    b: &ObjData,
    mut dedup: Option<&mut GcDedupState>,
    f: &mut impl FnMut(&ObjRef),
) {
    gc_visit_props_and_proto(b, f);
    match &b.kind {
        ObjKind::Plain | ObjKind::Error | ObjKind::Regex(_) => {}
        ObjKind::Array(items) => {
            for v in items {
                gc_visit_value(v, f);
            }
        }
        ObjKind::Map(m) => {
            for (k, v) in &m.entries {
                gc_visit_value(k, f);
                gc_visit_value(v, f);
            }
        }
        ObjKind::Set(s) => {
            for v in &s.values {
                gc_visit_value(v, f);
            }
        }
        ObjKind::Function(fo) => gc_visit_fn_upvalues(fo, dedup, f),
        ObjKind::Native(n) => {
            for cap in &n.captures {
                f(cap);
            }
        }
        ObjKind::Class(cd) => {
            f(&cd.prototype);
            if let Some(ctor) = &cd.ctor {
                gc_visit_fn_upvalues(ctor, dedup.as_deref_mut(), f);
            }
            for (_, init) in cd.instance_fields.iter() {
                if let Some(fo) = init {
                    gc_visit_fn_upvalues(fo, dedup.as_deref_mut(), f);
                }
            }
        }
        ObjKind::Promise(pd) => {
            match &pd.state {
                PromiseState::Pending => {}
                PromiseState::Fulfilled(v) | PromiseState::Rejected(v) => gc_visit_value(v, f),
            }
            for r in &pd.reactions {
                match r {
                    PromiseReaction::Then {
                        on_fulfilled,
                        on_rejected,
                        result_promise,
                    } => {
                        if let Some(v) = on_fulfilled {
                            gc_visit_value(v, f);
                        }
                        if let Some(v) = on_rejected {
                            gc_visit_value(v, f);
                        }
                        f(result_promise);
                    }
                    PromiseReaction::Await(cont) => {
                        for frame in &cont.frames {
                            gc_visit_frame(frame, dedup.as_deref_mut(), f);
                        }
                        f(&cont.owner_promise);
                    }
                }
            }
        }
    }
}
