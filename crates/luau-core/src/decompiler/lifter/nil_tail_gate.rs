//! Per-proto switch and hazard measure for the `c and f(..) or nil` binding
//! rewrites (`bind_call_arm_nil_tail`, `lift_nil_tail_call_select`).
//!
//! Binding the joined register is exact, but it also makes code that used to
//! read an unbound name liftable at all. That code then meets the lifter's
//! other heuristics: an out-of-range jump inside a loop turns into
//! `continue`/`break` (RideAPet Onboarding Handler: a Show path whose early
//! exits jump to the loop's `task.wait`, which is mid-body and not a latch).
//! `lift_proto_with_nil_tail_fallback` therefore lifts a proto in which a
//! rewrite fired a second time with the rewrites off, and keeps the rewrite-free
//! lift when the rewritten one has more loop exits or more binding hazards.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::ast::{Expr, Stat};
use super::naming::RegVal;
use crate::parser::opcodes::LuauOpcode;
use crate::parser::types::{insn_a, insn_d, insn_op, Proto};

thread_local! {
    /// Protos (by identity) whose current lift runs WITHOUT the rewrites.
    static DISABLED: RefCell<Vec<usize>> = RefCell::new(Vec::new());
    /// How many rewrites fired per proto (by identity) in its current lift.
    static FIRED: RefCell<HashMap<usize, usize>> = RefCell::new(HashMap::new());
}

fn key(proto: &Proto) -> usize {
    proto as *const Proto as usize
}

fn kill_switch() -> bool {
    std::env::var("LUAU_NO_NIL_TAIL_BIND").is_ok()
}

/// May a nil-tail rewrite replace what register state `val` holds?
///
/// The rewrite starts a fresh binding and resets the register, which is sound
/// for a dead local, a dead parked expression (`src.Event` read once and never
/// used again) or nothing. A parked string/number/boolean LITERAL is different:
/// it is a constant default that a later MOVE already copied (`x or "Normal"`,
/// the `LOADK` arm of the diamond), and the overwrite pass names it with a
/// declaration of its own; resetting the register here drops the constant
/// (Slayers2 TextPlusUtility `fontface` lost its "Normal" default).
pub(super) fn register_value_is_replaceable(val: &RegVal) -> bool {
    !matches!(val, RegVal::Expr(Expr::String(_) | Expr::Number(_) | Expr::Bool(_)))
}

/// May the nil-tail binding rewrites run for `proto` right now?
pub(super) fn enabled(proto: &Proto) -> bool {
    !kill_switch() && !DISABLED.with(|d| d.borrow().contains(&key(proto)))
}

/// Record that a rewrite fired in the current lift of `proto`.
pub(super) fn note_fired(proto: &Proto) {
    FIRED.with(|f| *f.borrow_mut().entry(key(proto)).or_insert(0) += 1);
}

pub(super) fn fired_reset(proto: &Proto) {
    FIRED.with(|f| {
        f.borrow_mut().remove(&key(proto));
    });
}

pub(super) fn fired_count(proto: &Proto) -> usize {
    FIRED.with(|f| f.borrow().get(&key(proto)).copied().unwrap_or(0))
}

/// Run `f` with the rewrites off for `proto` only (children keep theirs).
pub(super) fn with_disabled<T>(proto: &Proto, f: impl FnOnce() -> T) -> T {
    DISABLED.with(|d| d.borrow_mut().push(key(proto)));
    let out = f();
    DISABLED.with(|d| {
        let mut d = d.borrow_mut();
        if let Some(i) = d.iter().rposition(|&k| k == key(proto)) {
            d.remove(i);
        }
    });
    out
}

/// Cheap prescan: a `LOADNIL Rr` that a `JUMPIF Rr` skips (`-> pc + 1`) or that
/// follows a `JUMP +1`. Only such protos can fire a rewrite.
pub(super) fn proto_has_candidates(proto: &Proto) -> bool {
    if kill_switch() {
        return false;
    }
    let code = &proto.code;
    let mut skipped: Vec<(usize, usize)> = Vec::new(); // (JUMPIF target, reg)
    let mut after_jump1: Vec<usize> = Vec::new();
    let mut w = 0usize;
    while w < code.len() {
        let insn = code[w];
        let op = LuauOpcode::from_u8(insn_op(insn));
        match op {
            LuauOpcode::JumpIf => {
                let t = w as i64 + insn_d(insn) as i64 + 1;
                if t > 0 {
                    skipped.push((t as usize, insn_a(insn) as usize));
                }
            }
            LuauOpcode::Jump if insn_d(insn) == 1 => after_jump1.push(w + 1),
            LuauOpcode::LoadNil => {
                let r = insn_a(insn) as usize;
                if after_jump1.contains(&w) || skipped.contains(&(w + 1, r)) {
                    return true;
                }
            }
            _ => {}
        }
        w += if op.has_aux() { 2 } else { 1 };
    }
    false
}

/// `break` / `continue` statements in a lifted body (closure bodies are
/// separate protos and are not part of `Stat`).
pub(super) fn loop_exits(stats: &[Stat]) -> usize {
    stats
        .iter()
        .map(|s| match s {
            Stat::Break | Stat::Continue => 1,
            Stat::If { then_body, elseif_clauses, else_body, .. } => {
                loop_exits(then_body)
                    + elseif_clauses.iter().map(|(_, b)| loop_exits(b)).sum::<usize>()
                    + else_body.as_deref().map_or(0, loop_exits)
            }
            Stat::While { body, .. }
            | Stat::Repeat { body, .. }
            | Stat::NumericFor { body, .. }
            | Stat::GenericFor { body, .. }
            | Stat::DoBlock { body } => loop_exits(body),
            _ => 0,
        })
        .sum()
}
