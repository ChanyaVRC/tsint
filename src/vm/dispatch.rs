//! バイトコードディスパッチループ (`run_until`) と例外巻き戻し (`unwind`)。

use std::cell::RefCell;
use std::rc::Rc;

use crate::bytecode::*;
use crate::value::*;

use super::ops::{
    array_values, js_add, js_compare, loose_eq, to_int32, to_number, to_property_key_string,
    to_string_basic, to_string_into, to_uint32,
};
use super::{ascii_char_rc, LocalSlot, RunOutcome, Vm};

/// `IncLocal`/`DecLocal` 共通のインプレース加減算。未初期化（TDZ）なら `false` を返す。
#[inline]
fn adjust_local_slot(slot: &mut LocalSlot, delta: f64) -> bool {
    match slot {
        LocalSlot::Direct(Value::Number(n)) => {
            *n += delta;
            true
        }
        LocalSlot::Direct(Value::Uninit) => false,
        LocalSlot::Direct(v) => {
            *v = Value::Number(to_number(v) + delta);
            true
        }
        LocalSlot::Boxed(cell) => {
            let mut b = cell.borrow_mut();
            match &mut *b {
                Value::Number(n) => {
                    *n += delta;
                    true
                }
                Value::Uninit => false,
                other => {
                    *other = Value::Number(to_number(other) + delta);
                    true
                }
            }
        }
    }
}

#[inline]
fn is_valid_array_index(num: f64) -> bool {
    num >= 0.0 && num.fract() == 0.0 && num < (usize::MAX as f64)
}

impl Vm {
    /// `self.frames.len()` が `stop_depth` 以下になるまでステップ実行する。
    pub fn run_until(&mut self, stop_depth: usize) -> Result<RunOutcome, Value> {
        let mut last_value = Value::Undefined;
        loop {
            if self.frames.len() <= stop_depth {
                return Ok(RunOutcome::Value(last_value));
            }
            // ホットパス内ループ: `self.frames.last_mut()` を毎回引き直さず、
            // アクティブフレームのローカル/スタック/IP を直接操作する。
            let slow_op: Option<(OpCode, usize)> = {
                let frame = self.frames.last_mut().unwrap();
                let code: &[OpCode] = &frame.chunk.code;
                let mut ip = frame.ip;
                let mut slow: Option<(OpCode, usize)> = None;
                let gc_due_ptr = &self.gc_allocs_since_last;
                let gc_thresh = self.gc_threshold;
                let native_depth = self.native_call_depth;

                loop {
                    let op_ip = ip;
                    let op = code[op_ip];
                    ip = op_ip + 1;

                    match op {
                        OpCode::Pop => {
                            frame.stack.pop().expect("オペランドスタックが空です");
                        }
                        OpCode::Dup => {
                            let v = frame
                                .stack
                                .last()
                                .expect("オペランドスタックが空です")
                                .clone();
                            frame.stack.push(v);
                        }
                        OpCode::Swap => {
                            let n = frame.stack.len();
                            frame.stack.swap(n - 1, n - 2);
                        }
                        OpCode::LoadSmallInt(n) => {
                            frame.stack.push(Value::Number(n as f64));
                        }
                        OpCode::LoadConst(idx) => {
                            let v = frame.chunk.const_value(idx);
                            frame.stack.push(v);
                        }
                        OpCode::LoadUndefined => frame.stack.push(Value::Undefined),
                        OpCode::LoadNull => frame.stack.push(Value::Null),
                        OpCode::LoadTrue => frame.stack.push(Value::Bool(true)),
                        OpCode::LoadFalse => frame.stack.push(Value::Bool(false)),
                        OpCode::LoadThis => {
                            unreachable!("This は Ident 解決経由でコンパイルされます")
                        }

                        OpCode::GetLocal(slot) => match &frame.locals[slot as usize] {
                            LocalSlot::Direct(Value::Number(n)) => {
                                frame.stack.push(Value::Number(*n));
                            }
                            LocalSlot::Direct(Value::Bool(b)) => {
                                frame.stack.push(Value::Bool(*b));
                            }
                            LocalSlot::Direct(Value::Undefined) => {
                                frame.stack.push(Value::Undefined);
                            }
                            LocalSlot::Direct(Value::Null) => {
                                frame.stack.push(Value::Null);
                            }
                            LocalSlot::Direct(Value::Uninit) => {
                                frame.ip = ip;
                                slow = Some((op, op_ip));
                                break;
                            }
                            LocalSlot::Direct(v) => {
                                frame.stack.push(v.clone());
                            }
                            LocalSlot::Boxed(cell) => {
                                let v = cell.borrow().clone();
                                if matches!(v, Value::Uninit) {
                                    frame.ip = ip;
                                    slow = Some((op, op_ip));
                                    break;
                                }
                                frame.stack.push(v);
                            }
                        },
                        OpCode::SetLocal(slot) => {
                            let v = frame
                                .stack
                                .last()
                                .expect("オペランドスタックが空です")
                                .clone();
                            frame.locals[slot as usize].set(v);
                        }
                        OpCode::BindLocal(slot) => {
                            let v = frame.stack.pop().expect("オペランドスタックが空です");
                            frame.locals[slot as usize].set(v);
                        }
                        OpCode::IncLocal(slot) => {
                            if !adjust_local_slot(&mut frame.locals[slot as usize], 1.0) {
                                frame.ip = ip;
                                slow = Some((op, op_ip));
                                break;
                            }
                        }
                        OpCode::DecLocal(slot) => {
                            if !adjust_local_slot(&mut frame.locals[slot as usize], -1.0) {
                                frame.ip = ip;
                                slow = Some((op, op_ip));
                                break;
                            }
                        }
                        OpCode::RebindLocal(slot) => {
                            if let LocalSlot::Boxed(cell) = &mut frame.locals[slot as usize] {
                                let cur = cell.borrow().clone();
                                *cell = Rc::new(RefCell::new(cur));
                            }
                        }
                        OpCode::GetUpvalue(idx) => {
                            let v = frame.upvalues[idx as usize].borrow().clone();
                            if matches!(v, Value::Uninit) {
                                frame.ip = ip;
                                slow = Some((op, op_ip));
                                break;
                            }
                            frame.stack.push(v);
                        }
                        OpCode::SetUpvalue(idx) => {
                            let v = frame
                                .stack
                                .last()
                                .expect("オペランドスタックが空です")
                                .clone();
                            *frame.upvalues[idx as usize].borrow_mut() = v;
                        }
                        OpCode::GetArg(idx) => {
                            let v = frame
                                .raw_args
                                .get(idx as usize)
                                .cloned()
                                .unwrap_or(Value::Undefined);
                            frame.stack.push(v);
                        }

                        // --- プロパティ/添字の高速インラインパス ---
                        OpCode::GetProp(cidx) => {
                            let key = frame.chunk.const_str_ref(cidx);
                            let mut hit: Option<Value> = None;
                            match frame.stack.last().expect("オペランドスタックが空です")
                            {
                                Value::Object(o) => {
                                    let hint = frame.chunk.ic_hint(op_ip);
                                    let b = o.borrow();
                                    if let Some((idx, slot)) = b.props.get_with_hint(key, hint) {
                                        frame.chunk.set_ic_hint(op_ip, idx);
                                        if let PropSlot::Value(v) = slot {
                                            hit = Some(v.clone());
                                        }
                                    } else {
                                        match &b.kind {
                                            ObjKind::Array(items) if &**key == "length" => {
                                                hit = Some(Value::Number(items.len() as f64));
                                            }
                                            ObjKind::Map(m) if &**key == "size" => {
                                                hit = Some(Value::Number(m.entries.len() as f64));
                                            }
                                            ObjKind::Set(s) if &**key == "size" => {
                                                hit = Some(Value::Number(s.values.len() as f64));
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                                Value::Str(s) if &**key == "length" => {
                                    let len = if s.is_ascii() {
                                        s.len()
                                    } else {
                                        s.chars().count()
                                    };
                                    hit = Some(Value::Number(len as f64));
                                }
                                Value::Number(_) | Value::Bool(_) => {
                                    hit = Some(Value::Undefined);
                                }
                                _ => {}
                            }
                            if let Some(v) = hit {
                                *frame.stack.last_mut().unwrap() = v;
                            } else {
                                frame.ip = ip;
                                slow = Some((op, op_ip));
                                break;
                            }
                        }
                        OpCode::SetProp(cidx) => {
                            let key = frame.chunk.const_str_ref(cidx);
                            let n = frame.stack.len();
                            let mut done = false;
                            if let Value::Object(o) = &frame.stack[n - 2] {
                                let hint = frame.chunk.ic_hint(op_ip);
                                let mut b = o.borrow_mut();
                                if let Some((idx, slot)) = b.props.get_mut_with_hint(key, hint) {
                                    frame.chunk.set_ic_hint(op_ip, idx);
                                    if let PropSlot::Value(slot_v) = slot {
                                        *slot_v = frame.stack[n - 1].clone();
                                        done = true;
                                    }
                                } else if b.proto.is_none() && !matches!(b.kind, ObjKind::Array(_))
                                {
                                    let idx = b.props.set_value_returning_idx(
                                        key.clone(),
                                        frame.stack[n - 1].clone(),
                                    );
                                    frame.chunk.set_ic_hint(op_ip, idx);
                                    done = true;
                                }
                            }
                            if done {
                                let val = frame.stack.pop().unwrap();
                                *frame.stack.last_mut().unwrap() = val;
                            } else {
                                frame.ip = ip;
                                slow = Some((op, op_ip));
                                break;
                            }
                        }
                        OpCode::GetIndex => {
                            let n = frame.stack.len();
                            let mut hit: Option<Value> = None;
                            match (&frame.stack[n - 2], &frame.stack[n - 1]) {
                                (Value::Object(o), Value::Number(num))
                                    if is_valid_array_index(*num) =>
                                {
                                    let b = o.borrow();
                                    if b.props.is_empty() {
                                        if let ObjKind::Array(items) = &b.kind {
                                            hit = Some(
                                                items
                                                    .get(*num as usize)
                                                    .cloned()
                                                    .unwrap_or(Value::Undefined),
                                            );
                                        }
                                    }
                                }
                                (Value::Str(s), Value::Number(num))
                                    if is_valid_array_index(*num) =>
                                {
                                    let idx = *num as usize;
                                    let v = if s.is_ascii() {
                                        s.as_bytes()
                                            .get(idx)
                                            .map(|&b| Value::Str(ascii_char_rc(b)))
                                            .unwrap_or(Value::Undefined)
                                    } else {
                                        s.chars()
                                            .nth(idx)
                                            .map(|c| Value::str(c.to_string()))
                                            .unwrap_or(Value::Undefined)
                                    };
                                    hit = Some(v);
                                }
                                (Value::Object(o), Value::Str(k)) => {
                                    let b = o.borrow();
                                    if let Some((_, PropSlot::Value(v))) =
                                        b.props.get_with_hint(k, 0)
                                    {
                                        hit = Some(v.clone());
                                    }
                                }
                                _ => {}
                            }
                            if let Some(v) = hit {
                                frame.stack.pop();
                                *frame.stack.last_mut().unwrap() = v;
                            } else {
                                frame.ip = ip;
                                slow = Some((op, op_ip));
                                break;
                            }
                        }
                        OpCode::SetIndex => {
                            let n = frame.stack.len();
                            let mut done = false;
                            match (&frame.stack[n - 3], &frame.stack[n - 2]) {
                                (Value::Object(o), Value::Number(num))
                                    if is_valid_array_index(*num) =>
                                {
                                    let mut b = o.borrow_mut();
                                    if b.props.is_empty() {
                                        if let ObjKind::Array(items) = &mut b.kind {
                                            let idx = *num as usize;
                                            if idx >= items.len() {
                                                items.resize(idx + 1, Value::Undefined);
                                            }
                                            items[idx] = frame.stack[n - 1].clone();
                                            done = true;
                                        }
                                    }
                                }
                                (Value::Object(o), Value::Str(k)) => {
                                    let mut b = o.borrow_mut();
                                    if let Some((_, slot)) = b.props.get_mut_with_hint(k, 0) {
                                        if let PropSlot::Value(slot_v) = slot {
                                            *slot_v = frame.stack[n - 1].clone();
                                            done = true;
                                        }
                                    } else if b.proto.is_none()
                                        && !matches!(b.kind, ObjKind::Array(_))
                                    {
                                        b.props.set_value(k.clone(), frame.stack[n - 1].clone());
                                        done = true;
                                    }
                                }
                                _ => {}
                            }
                            if done {
                                let val = frame.stack.pop().unwrap();
                                frame.stack.pop();
                                *frame.stack.last_mut().unwrap() = val;
                            } else {
                                frame.ip = ip;
                                slow = Some((op, op_ip));
                                break;
                            }
                        }

                        // --- 算術・比較・ビット演算の高速パス ---
                        OpCode::Add => {
                            let b = frame.stack.pop().expect("オペランドスタックが空です");
                            let a = frame.stack.last_mut().expect("オペランドスタックが空です");
                            match (&*a, &b) {
                                (Value::Number(x), Value::Number(y)) => *a = Value::Number(*x + *y),
                                _ => *a = js_add(a, &b),
                            }
                        }
                        OpCode::Sub => {
                            let b = frame.stack.pop().expect("オペランドスタックが空です");
                            let a = frame.stack.last_mut().expect("オペランドスタックが空です");
                            let r = match (&*a, &b) {
                                (Value::Number(x), Value::Number(y)) => *x - *y,
                                _ => to_number(a) - to_number(&b),
                            };
                            *a = Value::Number(r);
                        }
                        OpCode::Mul => {
                            let b = frame.stack.pop().expect("オペランドスタックが空です");
                            let a = frame.stack.last_mut().expect("オペランドスタックが空です");
                            let r = match (&*a, &b) {
                                (Value::Number(x), Value::Number(y)) => *x * *y,
                                _ => to_number(a) * to_number(&b),
                            };
                            *a = Value::Number(r);
                        }
                        OpCode::Div => {
                            let b = frame.stack.pop().expect("オペランドスタックが空です");
                            let a = frame.stack.last_mut().expect("オペランドスタックが空です");
                            let r = match (&*a, &b) {
                                (Value::Number(x), Value::Number(y)) => *x / *y,
                                _ => to_number(a) / to_number(&b),
                            };
                            *a = Value::Number(r);
                        }
                        OpCode::Mod => {
                            let b = frame.stack.pop().expect("オペランドスタックが空です");
                            let a = frame.stack.last_mut().expect("オペランドスタックが空です");
                            let r = match (&*a, &b) {
                                (Value::Number(x), Value::Number(y)) => *x % *y,
                                _ => to_number(a) % to_number(&b),
                            };
                            *a = Value::Number(r);
                        }
                        OpCode::Pow => {
                            let b = frame.stack.pop().expect("オペランドスタックが空です");
                            let a = frame.stack.last_mut().expect("オペランドスタックが空です");
                            let r = match (&*a, &b) {
                                (Value::Number(x), Value::Number(y)) => x.powf(*y),
                                _ => to_number(a).powf(to_number(&b)),
                            };
                            *a = Value::Number(r);
                        }
                        OpCode::Eq => {
                            let b = frame.stack.pop().expect("オペランドスタックが空です");
                            let a = frame.stack.last_mut().expect("オペランドスタックが空です");
                            *a = Value::Bool(loose_eq(a, &b));
                        }
                        OpCode::NotEq => {
                            let b = frame.stack.pop().expect("オペランドスタックが空です");
                            let a = frame.stack.last_mut().expect("オペランドスタックが空です");
                            *a = Value::Bool(!loose_eq(a, &b));
                        }
                        OpCode::StrictEq => {
                            let b = frame.stack.pop().expect("オペランドスタックが空です");
                            let a = frame.stack.last_mut().expect("オペランドスタックが空です");
                            *a = Value::Bool(strict_eq(a, &b));
                        }
                        OpCode::StrictNotEq => {
                            let b = frame.stack.pop().expect("オペランドスタックが空です");
                            let a = frame.stack.last_mut().expect("オペランドスタックが空です");
                            *a = Value::Bool(!strict_eq(a, &b));
                        }
                        OpCode::Lt => {
                            let b = frame.stack.pop().expect("オペランドスタックが空です");
                            let a = frame.stack.last_mut().expect("オペランドスタックが空です");
                            let r = match (&*a, &b) {
                                (Value::Number(x), Value::Number(y)) => *x < *y,
                                _ => js_compare(a, &b) == Some(std::cmp::Ordering::Less),
                            };
                            *a = Value::Bool(r);
                        }
                        OpCode::Gt => {
                            let b = frame.stack.pop().expect("オペランドスタックが空です");
                            let a = frame.stack.last_mut().expect("オペランドスタックが空です");
                            let r = match (&*a, &b) {
                                (Value::Number(x), Value::Number(y)) => *x > *y,
                                _ => js_compare(a, &b) == Some(std::cmp::Ordering::Greater),
                            };
                            *a = Value::Bool(r);
                        }
                        OpCode::LtEq => {
                            let b = frame.stack.pop().expect("オペランドスタックが空です");
                            let a = frame.stack.last_mut().expect("オペランドスタックが空です");
                            let r = match (&*a, &b) {
                                (Value::Number(x), Value::Number(y)) => *x <= *y,
                                _ => js_compare(a, &b)
                                    .is_some_and(|o| o != std::cmp::Ordering::Greater),
                            };
                            *a = Value::Bool(r);
                        }
                        OpCode::GtEq => {
                            let b = frame.stack.pop().expect("オペランドスタックが空です");
                            let a = frame.stack.last_mut().expect("オペランドスタックが空です");
                            let r = match (&*a, &b) {
                                (Value::Number(x), Value::Number(y)) => *x >= *y,
                                _ => {
                                    js_compare(a, &b).is_some_and(|o| o != std::cmp::Ordering::Less)
                                }
                            };
                            *a = Value::Bool(r);
                        }
                        OpCode::Shl => {
                            let b = frame.stack.pop().expect("オペランドスタックが空です");
                            let a = frame.stack.last_mut().expect("オペランドスタックが空です");
                            let r = to_int32(a).wrapping_shl(to_int32(&b) as u32 & 31);
                            *a = Value::Number(r as f64);
                        }
                        OpCode::Shr => {
                            let b = frame.stack.pop().expect("オペランドスタックが空です");
                            let a = frame.stack.last_mut().expect("オペランドスタックが空です");
                            let r = to_int32(a).wrapping_shr(to_int32(&b) as u32 & 31);
                            *a = Value::Number(r as f64);
                        }
                        OpCode::UShr => {
                            let b = frame.stack.pop().expect("オペランドスタックが空です");
                            let a = frame.stack.last_mut().expect("オペランドスタックが空です");
                            let r = to_uint32(a).wrapping_shr(to_int32(&b) as u32 & 31);
                            *a = Value::Number(r as f64);
                        }
                        OpCode::BitAnd => {
                            let b = frame.stack.pop().expect("オペランドスタックが空です");
                            let a = frame.stack.last_mut().expect("オペランドスタックが空です");
                            *a = Value::Number((to_int32(a) & to_int32(&b)) as f64);
                        }
                        OpCode::BitOr => {
                            let b = frame.stack.pop().expect("オペランドスタックが空です");
                            let a = frame.stack.last_mut().expect("オペランドスタックが空です");
                            *a = Value::Number((to_int32(a) | to_int32(&b)) as f64);
                        }
                        OpCode::BitXor => {
                            let b = frame.stack.pop().expect("オペランドスタックが空です");
                            let a = frame.stack.last_mut().expect("オペランドスタックが空です");
                            *a = Value::Number((to_int32(a) ^ to_int32(&b)) as f64);
                        }
                        OpCode::Neg => {
                            let a = frame.stack.last_mut().expect("オペランドスタックが空です");
                            *a = Value::Number(-to_number(a));
                        }
                        OpCode::Pos => {
                            let a = frame.stack.last_mut().expect("オペランドスタックが空です");
                            *a = Value::Number(to_number(a));
                        }
                        OpCode::Not => {
                            let a = frame.stack.last_mut().expect("オペランドスタックが空です");
                            *a = Value::Bool(!a.to_bool());
                        }
                        OpCode::BitNot => {
                            let a = frame.stack.last_mut().expect("オペランドスタックが空です");
                            *a = Value::Number(!to_int32(a) as f64);
                        }
                        OpCode::TypeOf => {
                            let a = frame.stack.last_mut().expect("オペランドスタックが空です");
                            *a = Value::str(a.type_of());
                        }
                        OpCode::ToStringOp => {
                            let a = frame.stack.last_mut().expect("オペランドスタックが空です");
                            if !matches!(a, Value::Str(_)) {
                                *a = Value::Str(to_string_basic(a).into());
                            }
                        }
                        OpCode::ConcatStrings(count) => {
                            let cnt = count as usize;
                            let start = frame.stack.len() - cnt;
                            let mut buf = String::new();
                            for v in &frame.stack[start..] {
                                to_string_into(v, &mut buf);
                            }
                            frame.stack.truncate(start);
                            frame.stack.push(Value::Str(buf.into()));
                        }

                        // --- 制御フロー・融合比較ジャンプ ---
                        OpCode::Jump(addr) => {
                            let target = addr as usize;
                            ip = target;
                            if target <= op_ip && native_depth == 0 && gc_due_ptr.get() >= gc_thresh
                            {
                                frame.ip = ip;
                                break;
                            }
                        }
                        OpCode::JumpIfFalse(addr) => {
                            let v = frame.stack.pop().expect("オペランドスタックが空です");
                            if !v.to_bool() {
                                ip = addr as usize;
                            }
                        }
                        OpCode::JumpIfTrue(addr) => {
                            let v = frame.stack.pop().expect("オペランドスタックが空です");
                            if v.to_bool() {
                                let target = addr as usize;
                                ip = target;
                                if target <= op_ip
                                    && native_depth == 0
                                    && gc_due_ptr.get() >= gc_thresh
                                {
                                    frame.ip = ip;
                                    break;
                                }
                            }
                        }
                        OpCode::LtJumpIfFalse(addr) => {
                            let b = frame.stack.pop().expect("オペランドスタックが空です");
                            let a = frame.stack.pop().expect("オペランドスタックが空です");
                            let cond = match (&a, &b) {
                                (Value::Number(x), Value::Number(y)) => *x < *y,
                                _ => js_compare(&a, &b) == Some(std::cmp::Ordering::Less),
                            };
                            if !cond {
                                ip = addr as usize;
                            }
                        }
                        OpCode::LtEqJumpIfFalse(addr) => {
                            let b = frame.stack.pop().expect("オペランドスタックが空です");
                            let a = frame.stack.pop().expect("オペランドスタックが空です");
                            let cond = match (&a, &b) {
                                (Value::Number(x), Value::Number(y)) => *x <= *y,
                                _ => js_compare(&a, &b)
                                    .is_some_and(|o| o != std::cmp::Ordering::Greater),
                            };
                            if !cond {
                                ip = addr as usize;
                            }
                        }
                        OpCode::GtJumpIfFalse(addr) => {
                            let b = frame.stack.pop().expect("オペランドスタックが空です");
                            let a = frame.stack.pop().expect("オペランドスタックが空です");
                            let cond = match (&a, &b) {
                                (Value::Number(x), Value::Number(y)) => *x > *y,
                                _ => js_compare(&a, &b) == Some(std::cmp::Ordering::Greater),
                            };
                            if !cond {
                                ip = addr as usize;
                            }
                        }
                        OpCode::GtEqJumpIfFalse(addr) => {
                            let b = frame.stack.pop().expect("オペランドスタックが空です");
                            let a = frame.stack.pop().expect("オペランドスタックが空です");
                            let cond = match (&a, &b) {
                                (Value::Number(x), Value::Number(y)) => *x >= *y,
                                _ => js_compare(&a, &b)
                                    .is_some_and(|o| o != std::cmp::Ordering::Less),
                            };
                            if !cond {
                                ip = addr as usize;
                            }
                        }
                        OpCode::StrictEqJumpIfFalse(addr) => {
                            let b = frame.stack.pop().expect("オペランドスタックが空です");
                            let a = frame.stack.pop().expect("オペランドスタックが空です");
                            if !strict_eq(&a, &b) {
                                ip = addr as usize;
                            }
                        }
                        OpCode::StrictNotEqJumpIfFalse(addr) => {
                            let b = frame.stack.pop().expect("オペランドスタックが空です");
                            let a = frame.stack.pop().expect("オペランドスタックが空です");
                            if strict_eq(&a, &b) {
                                ip = addr as usize;
                            }
                        }
                        OpCode::JumpIfNullish(addr) => {
                            if frame
                                .stack
                                .last()
                                .expect("オペランドスタックが空です")
                                .is_nullish()
                            {
                                *frame.stack.last_mut().unwrap() = Value::Undefined;
                                ip = addr as usize;
                            }
                        }
                        OpCode::JumpIfNullishCall(addr) => {
                            if frame
                                .stack
                                .last()
                                .expect("オペランドスタックが空です")
                                .is_nullish()
                            {
                                frame.stack.pop();
                                *frame.stack.last_mut().unwrap() = Value::Undefined;
                                ip = addr as usize;
                            }
                        }
                        OpCode::JumpIfFalsyKeep(addr) => {
                            if !frame
                                .stack
                                .last()
                                .expect("オペランドスタックが空です")
                                .to_bool()
                            {
                                ip = addr as usize;
                            }
                        }
                        OpCode::JumpIfTruthyKeep(addr) => {
                            if frame
                                .stack
                                .last()
                                .expect("オペランドスタックが空です")
                                .to_bool()
                            {
                                ip = addr as usize;
                            }
                        }
                        OpCode::JumpIfNotNullishKeep(addr) => {
                            if !frame
                                .stack
                                .last()
                                .expect("オペランドスタックが空です")
                                .is_nullish()
                            {
                                ip = addr as usize;
                            }
                        }
                        OpCode::JumpIfNotUndefinedKeep(addr) => {
                            if !matches!(
                                frame.stack.last().expect("オペランドスタックが空です"),
                                Value::Undefined
                            ) {
                                ip = addr as usize;
                            } else {
                                frame.stack.pop();
                            }
                        }
                        OpCode::ArrayPush => {
                            let v = frame.stack.pop().expect("オペランドスタックが空です");
                            if let Some(Value::Object(o)) = frame.stack.last() {
                                if let ObjKind::Array(items) = &mut o.borrow_mut().kind {
                                    items.push(v);
                                }
                            }
                        }
                        OpCode::ObjectSetProp(cidx) => {
                            let name = frame.chunk.const_str(cidx);
                            let v = frame.stack.pop().expect("オペランドスタックが空です");
                            if let Some(Value::Object(o)) = frame.stack.last() {
                                o.borrow_mut().props.set_value(name, v);
                            }
                        }
                        OpCode::ObjectSetComputed => {
                            let v = frame.stack.pop().expect("オペランドスタックが空です");
                            let key = frame.stack.pop().expect("オペランドスタックが空です");
                            if let Some(Value::Object(o)) = frame.stack.last() {
                                let name: Rc<str> = match &key {
                                    Value::Str(s) => s.clone(),
                                    other => Rc::from(to_property_key_string(other).as_str()),
                                };
                                o.borrow_mut().props.set_value(name, v);
                            }
                        }
                        OpCode::ObjectSetMethod(cidx) => {
                            let name = frame.chunk.const_str(cidx);
                            let f = frame.stack.pop().expect("オペランドスタックが空です");
                            if let Some(Value::Object(o)) = frame.stack.last() {
                                o.borrow_mut().props.set_value(name, f);
                            }
                        }
                        OpCode::ObjectDefineGetter(cidx) => {
                            let name = frame.chunk.const_str(cidx);
                            let f = frame.stack.pop().expect("オペランドスタックが空です");
                            if let Some(Value::Object(o)) = frame.stack.last() {
                                o.borrow_mut().props.define_accessor(name, true, f);
                            }
                        }
                        OpCode::ObjectDefineSetter(cidx) => {
                            let name = frame.chunk.const_str(cidx);
                            let f = frame.stack.pop().expect("オペランドスタックが空です");
                            if let Some(Value::Object(o)) = frame.stack.last() {
                                o.borrow_mut().props.define_accessor(name, false, f);
                            }
                        }

                        other => {
                            frame.ip = ip;
                            slow = Some((other, op_ip));
                            break;
                        }
                    }
                }
                slow
            };

            let Some((op, op_ip)) = slow_op else {
                if self.native_call_depth == 0 {
                    self.maybe_collect_garbage();
                }
                continue;
            };

            let res = self.exec_slow_op(op, op_ip, stop_depth, &mut last_value);
            match res {
                Ok(Some(outcome)) => return Ok(outcome),
                Ok(None) => {}
                Err(thrown) => self.unwind(thrown, stop_depth)?,
            }
        }
    }

    /// フレーム変更・VM 状態アクセス・低頻度命令を実行する外側ハンドラ。
    /// `run_until` のホットループから分離してコードサイズとレジスタ圧力を抑える。
    #[inline(never)]
    fn exec_slow_op(
        &mut self,
        op: OpCode,
        op_ip: usize,
        stop_depth: usize,
        last_value: &mut Value,
    ) -> Result<Option<RunOutcome>, Value> {
        match op {
            OpCode::Return => {
                let mut popped = self.frames.pop().unwrap();
                let v = popped.stack.pop().expect("オペランドスタックが空です");
                if self.frames.is_empty() && popped.chunk.is_module {
                    let exports = popped
                        .chunk
                        .exports
                        .iter()
                        .map(|(name, slot)| (name.clone(), popped.locals[*slot as usize].get()))
                        .collect();
                    self.pending_module_exports = Some(exports);
                }
                self.release_frame(popped);
                if self.frames.len() > stop_depth {
                    self.frames.last_mut().unwrap().stack.push(v);
                } else {
                    *last_value = v;
                }
                self.maybe_gc_if_top_level();
                Ok(None)
            }
            OpCode::Call(argc) => self.exec_call(argc as usize).map(|()| None),
            OpCode::CallMethod(cidx, argc) => self
                .exec_call_method(cidx as ConstIdx, argc as usize, op_ip)
                .map(|()| None),
            OpCode::GetProp(cidx) => {
                let name = self.frames.last().unwrap().chunk.const_str(cidx);
                let obj = self.pop();
                self.get_prop_ic(&obj, &name, op_ip).map(|v| {
                    self.push(v);
                    None
                })
            }
            OpCode::SetProp(cidx) => {
                let name = self.frames.last().unwrap().chunk.const_str(cidx);
                let value = self.pop();
                let obj = self.pop();
                self.set_prop_rc(&obj, &name, value.clone()).map(|()| {
                    self.push(value);
                    None
                })
            }
            OpCode::GetIndex => {
                let key = self.pop();
                let obj = self.pop();
                self.exec_get_index(&obj, &key).map(|v| {
                    self.push(v);
                    None
                })
            }
            OpCode::SetIndex => {
                let value = self.pop();
                let key = self.pop();
                let obj = self.pop();
                self.exec_set_index(&obj, &key, value.clone()).map(|()| {
                    self.push(value);
                    None
                })
            }
            OpCode::GetLocal(_)
            | OpCode::IncLocal(_)
            | OpCode::DecLocal(_)
            | OpCode::GetUpvalue(_) => {
                Err(self.reference_error("変数が初期化される前にアクセスされました（TDZ）"))
            }
            OpCode::GetGlobal(cidx) => {
                let name = self.frames.last().unwrap().chunk.const_str(cidx);
                match self.globals.get(&name) {
                    Some(cell) => {
                        let v = cell.borrow().clone();
                        if matches!(v, Value::Uninit) {
                            Err(self.reference_error(format!(
                                "'{name}' は初期化される前に参照されました"
                            )))
                        } else {
                            self.push(v);
                            Ok(None)
                        }
                    }
                    None => Err(self.reference_error(format!("'{name}' は定義されていません"))),
                }
            }
            OpCode::SetGlobal(cidx) => {
                let name = self.frames.last().unwrap().chunk.const_str(cidx);
                let v = self.top().clone();
                match self.globals.get(&name) {
                    Some(cell) => {
                        *cell.borrow_mut() = v;
                        Ok(None)
                    }
                    None => Err(self.reference_error(format!("'{name}' は定義されていません"))),
                }
            }
            OpCode::BindGlobal(cidx) => {
                let name = self.frames.last().unwrap().chunk.const_str(cidx);
                let v = self.pop();
                self.globals.insert(name, Rc::new(RefCell::new(v)));
                Ok(None)
            }
            OpCode::GetRestArgs(from) => {
                let args = &self.frames.last().unwrap().raw_args;
                let rest = if (from as usize) < args.len() {
                    args[from as usize..].to_vec()
                } else {
                    Vec::new()
                };
                let arr = self.alloc_array(rest);
                self.push(arr);
                Ok(None)
            }
            OpCode::DeleteProp(cidx) => {
                let name = self.frames.last().unwrap().chunk.const_str(cidx);
                let obj = self.pop();
                let ok = self.delete_prop(&obj, &name);
                self.push(Value::Bool(ok));
                Ok(None)
            }
            OpCode::DeleteIndex => {
                let key = self.pop();
                let obj = self.pop();
                let ok = match &key {
                    Value::Str(s) => self.delete_prop(&obj, s),
                    other => {
                        let name = to_property_key_string(other);
                        self.delete_prop(&obj, &name)
                    }
                };
                self.push(Value::Bool(ok));
                Ok(None)
            }
            OpCode::Instanceof => {
                let b = self.pop();
                let a = self.pop();
                self.op_instanceof(&a, &b).map(|r| {
                    self.push(r);
                    None
                })
            }
            OpCode::In => {
                let obj = self.pop();
                let key = self.pop();
                if let Value::Object(o) = &obj {
                    let has = match &key {
                        Value::Str(s) => self.has_prop(o, s),
                        other => {
                            let name = to_property_key_string(other);
                            self.has_prop(o, &name)
                        }
                    };
                    self.push(Value::Bool(has));
                    Ok(None)
                } else {
                    Err(self.type_error("in 演算子の右辺はオブジェクトである必要があります"))
                }
            }
            OpCode::TypeOfGlobal(cidx) => {
                let name = self.frames.last().unwrap().chunk.const_str(cidx);
                let s = match self.globals.get(&name) {
                    Some(cell) => {
                        let v = cell.borrow().clone();
                        if matches!(v, Value::Uninit) {
                            "undefined"
                        } else {
                            v.type_of()
                        }
                    }
                    None => "undefined",
                };
                self.push(Value::str(s));
                Ok(None)
            }
            OpCode::Throw => {
                let v = self.pop();
                Err(v)
            }
            OpCode::MakeClosure(cidx) => {
                let tpl = self
                    .frames
                    .last()
                    .unwrap()
                    .chunk
                    .const_function_template(cidx);
                let fo = self.build_closure(&tpl);
                let v = self.wrap_function(fo);
                self.push(v);
                self.maybe_gc_if_top_level();
                Ok(None)
            }
            OpCode::CallSpread => {
                let args_arr = self.pop();
                let callee = self.pop();
                let this = self.pop();
                let args = array_values(&args_arr);
                self.call_value(callee, this, &args).map(|r| {
                    self.push(r);
                    None
                })
            }
            OpCode::New(argc) => self.exec_new(argc as usize).map(|()| {
                self.maybe_gc_if_top_level();
                None
            }),
            OpCode::NewSpread => {
                let args_arr = self.pop();
                let callee = self.pop();
                let args = array_values(&args_arr);
                self.dispatch_new(callee, args).map(|r| {
                    self.push(r);
                    None
                })
            }
            OpCode::NewObject => {
                let o = self.alloc_plain_object();
                self.push(o);
                self.maybe_gc_if_top_level();
                Ok(None)
            }
            OpCode::NewArray => {
                let a = self.alloc_array(Vec::new());
                self.push(a);
                self.maybe_gc_if_top_level();
                Ok(None)
            }
            OpCode::SpreadAppendArray => {
                let src = self.pop();
                self.materialize_iter_array(&src).map(|items| {
                    let arr = self.top().clone();
                    if let Value::Object(o) = &arr {
                        if let ObjKind::Array(dst) = &mut o.borrow_mut().kind {
                            dst.extend(items);
                        }
                    }
                    None
                })
            }
            OpCode::ObjectSpread => {
                let src = self.pop();
                let obj = self.top().clone();
                if let (Value::Object(dst), Value::Object(src_o)) = (&obj, &src) {
                    let entries: Vec<(Rc<str>, Value)> = src_o
                        .borrow()
                        .props
                        .iter()
                        .filter_map(|(k, s)| match s {
                            PropSlot::Value(v) => Some((k.clone(), v.clone())),
                            PropSlot::Accessor { .. } => None,
                        })
                        .collect();
                    for (k, v) in entries {
                        dst.borrow_mut().props.set_value(k, v);
                    }
                    if let ObjKind::Array(items) = &src_o.borrow().kind {
                        for (i, it) in items.iter().enumerate() {
                            dst.borrow_mut()
                                .props
                                .set_value(Rc::from(i.to_string().as_str()), it.clone());
                        }
                    }
                }
                Ok(None)
            }
            OpCode::MakeIterArray => {
                let v = self.pop();
                self.materialize_iter_array(&v).map(|items| {
                    let arr = self.alloc_array(items);
                    self.push(arr);
                    None
                })
            }
            OpCode::ObjectKeysArray => {
                let v = self.pop();
                let keys = super::props::own_enumerable_keys(&v);
                let arr = self.alloc_array(keys);
                self.push(arr);
                Ok(None)
            }
            OpCode::ObjectRestExclude(n) => {
                let mut exclude = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    exclude.push(to_property_key_string(&self.pop()));
                }
                let src = self.pop();
                let mut out = ObjData::new(ObjKind::Plain);
                if let Value::Object(o) = &src {
                    for (k, s) in o.borrow().props.iter() {
                        if exclude.iter().any(|e| e == &**k) {
                            continue;
                        }
                        if let PropSlot::Value(v) = s {
                            out.props.set_value(k.clone(), v.clone());
                        }
                    }
                }
                let val = self.alloc_obj(out);
                self.push(val);
                Ok(None)
            }
            OpCode::ArraySliceFrom => {
                let start = self.pop();
                let arr = self.pop();
                let start_idx = to_number(&start) as usize;
                let items = array_values(&arr);
                let sliced = if start_idx < items.len() {
                    items[start_idx..].to_vec()
                } else {
                    Vec::new()
                };
                let val = self.alloc_array(sliced);
                self.push(val);
                Ok(None)
            }
            OpCode::MakeClass(cidx) => {
                let tpl = self.frames.last().unwrap().chunk.const_class_template(cidx);
                self.build_class(&tpl).map(|v| {
                    self.push(v);
                    None
                })
            }
            OpCode::LoadRegex(cidx) => {
                let re = self.frames.last().unwrap().chunk.const_regex(cidx);
                let v = self.alloc_regex_value(re);
                self.push(v);
                self.maybe_gc_if_top_level();
                Ok(None)
            }
            OpCode::Await => {
                if self.native_call_depth > 0 {
                    Err(self.type_error(
                        "await はネイティブコールバックの内側では中断できません（T3 の既知の制約）",
                    ))
                } else {
                    let v = self.pop();
                    let promise = self.coerce_to_promise(v);
                    let owner = self
                        .current_owner_promise
                        .clone()
                        .expect("await はタスク駆動ループの外では実行されないはず");
                    let frames = std::mem::take(&mut self.frames);
                    let cont = AwaitContinuation {
                        frames,
                        owner_promise: owner,
                    };
                    self.add_reaction(&promise, PromiseReaction::Await(cont));
                    Ok(Some(RunOutcome::Suspended))
                }
            }
            _ => unreachable!("ホットパスで処理済みの命令が外側へ到達しました"),
        }
    }

    /// 例外を現在のフレームから外側へ向かって処理できる場所を探す。
    pub(super) fn unwind(&mut self, thrown: Value, stop_depth: usize) -> Result<(), Value> {
        loop {
            if self.frames.len() <= stop_depth {
                return Err(thrown);
            }
            let frame = self.frames.last_mut().unwrap();
            let fault_ip = frame.ip.saturating_sub(1) as u32;
            let handler = frame
                .chunk
                .exception_table
                .iter()
                .filter(|e| e.try_start <= fault_ip && fault_ip < e.try_end)
                .min_by_key(|e| e.try_end - e.try_start)
                .map(|e| e.handler_addr);
            if let Some(addr) = handler {
                frame.stack.clear();
                frame.stack.push(thrown);
                frame.ip = addr as usize;
                return Ok(());
            }
            if let Some(popped) = self.frames.pop() {
                self.release_frame(popped);
            }
        }
    }
}
