//! Non-loop OUT-OF-RANGE conditional jumps in a flat lift range.
//!
//! `lift_instruction_range` turns a conditional jump into a `Stat::If` only
//! when its target lies inside the range being lifted (`target <= end`). The
//! range is whatever the parent carved: an if-arm, a Linear region of a
//! structured run, one contiguous run of an arm. OUTSIDE a loop a forward
//! conditional jump past `end` used to be DISCARDED with no trace, so the
//! guarded remainder of the range ran on both paths (the checker cannot see
//! a dropped guard). This module holds the two exact rewrites that recover
//! such a jump without re-carving anything:
//!
//! * [`range_exit_alias`] (S1): the jump target is provably the place the
//!   range itself exits to, so the jump is a jump to the range end and the
//!   ordinary inline-if path applies.
//! * [`terminal_return_at`] (S2): the jump target starts a terminal
//!   `RETURN` block, so the jump is exactly `if <fire> then return .. end`.
//!
//! Both are applied ONLY outside loops; in-loop out-of-range jumps (which
//! become `continue`/`break`) belong to the loop-control track and are left
//! untouched. Every site keeps today's behaviour unless its rule provably
//! holds.

use std::cell::RefCell;

use crate::ast::{Expr, Stat};
use crate::decompiler::{constant_to_expr, DecompileContext};
use crate::parser::opcodes::LuauOpcode;
use crate::parser::types::{insn_a, insn_b, insn_c, insn_d, insn_op, Proto};

use super::{is_pure_expr, reg_read_before_write, sanitize_leaked_global_string, RegVal};

thread_local! {
    /// Out-of-range forward jump targets that some ACTIVE `lift_instruction_range`
    /// call discarded (or resumed past), keyed by proto identity. A dropped jump
    /// stays dropped for the rest of the proto's lift: the output that follows
    /// it runs on that jump's path whichever sub-range emitted it (`fa3e7f27`
    /// proto@532: the dropped first link lived in a nested lift that had
    /// already returned when the second link was reached). Cleared per proto
    /// by `fired_reset`.
    static OOR_DROPPED: RefCell<Vec<(usize, usize)>> = RefCell::new(Vec::new());
}

thread_local! {
    /// Lone `JUMP` pcs (by proto identity) whose effect an in-loop guard
    /// already emitted as `if <cond> then continue|break end`. The JUMP is
    /// lifted later as its own span; it must not add an unconditional
    /// `continue`/`break` that dead-code elimination would truncate the loop
    /// body at.
    static CONSUMED_JUMPS: RefCell<Vec<(usize, usize)>> = RefCell::new(Vec::new());
}

/// Record that the lone `JUMP` at `jump_pc` is already represented by a guard.
pub(super) fn note_consumed_jump(proto: &Proto, jump_pc: usize) {
    let entry = (proto_key(proto), jump_pc);
    CONSUMED_JUMPS.with(|c| {
        let mut c = c.borrow_mut();
        if !c.contains(&entry) {
            c.push(entry);
        }
    });
}

/// Was the `JUMP` at `jump_pc` already represented by a guard? Consumes the note.
pub(super) fn take_consumed_jump(proto: &Proto, jump_pc: usize) -> bool {
    let entry = (proto_key(proto), jump_pc);
    CONSUMED_JUMPS.with(|c| {
        let mut c = c.borrow_mut();
        let before = c.len();
        c.retain(|e| *e != entry);
        c.len() != before
    })
}

fn proto_key(proto: &Proto) -> usize {
    proto as *const Proto as usize
}

/// Record that the flat walk runs PAST a forward jump to `target` without
/// modelling it: on that jump's path the bytecode never executes the code
/// that follows in the output, yet the output runs straight through it.
pub(super) fn note_dropped(proto: &Proto, target: usize) {
    OOR_DROPPED.with(|d| d.borrow_mut().push((proto_key(proto), target)));
}

/// Is `pc` on a FOREIGN path? True when an active lift of this proto dropped a
/// forward jump that lands past `pc`: the flat output then also runs `pc` on
/// that jump's path, so a guard emitted here would fire where the bytecode
/// never evaluates it (`fa3e7f27` proto@532: the first link of
/// `(a and b) or (not a and not c)` jumps to the body and is dropped; a
/// return-guard on the second link would return where the body must run).
fn on_foreign_path(proto: &Proto, pc: usize) -> bool {
    let key = proto_key(proto);
    OOR_DROPPED.with(|d| d.borrow().iter().any(|&(k, t)| k == key && t > pc))
}

thread_local! {
    /// Protos (by identity) whose current lift runs WITHOUT these rewrites:
    /// the fallback lift of `lift_proto_inner`'s hazard check.
    static DISABLED: RefCell<Vec<usize>> = RefCell::new(Vec::new());
    /// How many rewrites fired per proto (by identity) in its current lift.
    static FIRED: RefCell<std::collections::HashMap<usize, usize>> =
        RefCell::new(std::collections::HashMap::new());
}

fn kill_switch() -> bool {
    std::env::var("LUAU_NO_OOR_NONLOOP").is_ok()
}

/// Are the rewrites switched off for this proto's current lift?
fn disabled(proto: &Proto) -> bool {
    kill_switch() || DISABLED.with(|d| d.borrow().contains(&proto_key(proto)))
}

/// May the loop-body gap join rewrite run for `proto`? (Same disable switches
/// as the other rewrites: the hazard fallback lifts the proto without it.)
pub(super) fn gap_join_allowed(proto: &Proto) -> bool {
    !disabled(proto)
}

/// Record that the loop-body gap join fired for `proto`, so the hazard check
/// compares it against a rewrite-free lift.
pub(super) fn note_gap_join(proto: &Proto) {
    note_fired(proto);
}

fn note_fired(proto: &Proto) {
    FIRED.with(|f| *f.borrow_mut().entry(proto_key(proto)).or_insert(0) += 1);
}

/// Reset, and later read, how many rewrites fired in a lift of `proto`.
pub(super) fn fired_reset(proto: &Proto) {
    FIRED.with(|f| {
        f.borrow_mut().remove(&proto_key(proto));
    });
    let key = proto_key(proto);
    OOR_DROPPED.with(|d| d.borrow_mut().retain(|&(k, _)| k != key));
    CONSUMED_JUMPS.with(|c| c.borrow_mut().retain(|&(k, _)| k != key));
}
pub(super) fn fired_count(proto: &Proto) -> usize {
    FIRED.with(|f| f.borrow().get(&proto_key(proto)).copied().unwrap_or(0))
}

/// Run `f` with the rewrites disabled for `proto` only (children keep theirs).
pub(super) fn with_disabled<T>(proto: &Proto, f: impl FnOnce() -> T) -> T {
    DISABLED.with(|d| d.borrow_mut().push(proto_key(proto)));
    let out = f();
    DISABLED.with(|d| {
        let mut d = d.borrow_mut();
        if let Some(i) = d.iter().rposition(|&k| k == proto_key(proto)) {
            d.remove(i);
        }
    });
    out
}

/// Could any rewrite fire in `proto`? A cheap bytecode prescan so the hazard
/// fallback only snapshots the context for protos that can need it: some
/// conditional jump must target a terminal-block opcode (S2) or a pc an
/// unconditional forward JUMP also targets, or two JUMPs share a target (S1),
/// or the proto holds a loop (the loop-body gap join).
pub(super) fn proto_has_candidates(proto: &Proto) -> bool {
    if kill_switch() {
        return false;
    }
    let code = &proto.code;
    let mut cond_targets: Vec<usize> = Vec::new();
    let mut jump_targets: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut w = 0usize;
    while w < code.len() {
        let op = LuauOpcode::from_u8(insn_op(code[w]));
        let t = (w as i64 + insn_d(code[w]) as i64 + 1) as usize;
        match op {
            LuauOpcode::JumpIf
            | LuauOpcode::JumpIfNot
            | LuauOpcode::JumpIfEq
            | LuauOpcode::JumpIfNotEq
            | LuauOpcode::JumpIfLE
            | LuauOpcode::JumpIfNotLE
            | LuauOpcode::JumpIfLT
            | LuauOpcode::JumpIfNotLT
            | LuauOpcode::JumpXEqKNil
            | LuauOpcode::JumpXEqKB
            | LuauOpcode::JumpXEqKN
            | LuauOpcode::JumpXEqKS => {
                if t > w {
                    cond_targets.push(t);
                }
            }
            LuauOpcode::Jump => {
                if t > w && !jump_targets.insert(t) {
                    return true;
                }
                // A backward jump closes a loop: the loop-body gap join may
                // fire there, and its result is hazard-checked like the rest.
                if t <= w {
                    return true;
                }
            }
            LuauOpcode::JumpBack | LuauOpcode::ForNLoop | LuauOpcode::ForGLoop => return true,
            _ => {}
        }
        w += insn_size(op);
    }
    cond_targets.iter().any(|t| {
        jump_targets.contains(t)
            || matches!(
                code.get(*t).map(|&i| LuauOpcode::from_u8(insn_op(i))),
                Some(
                    LuauOpcode::Return
                        | LuauOpcode::LoadNil
                        | LuauOpcode::LoadB
                        | LuauOpcode::LoadN
                        | LuauOpcode::LoadK
                )
            )
    })
}

fn trace(kind: &str, pc: usize, target: usize, end: usize, what: &str) {
    if std::env::var("LUAU_OOR_FIX_TRACE").is_ok() {
        eprintln!(
            "OORFIX\t{}\t{}\tpc={}\ttarget={}\tend={}\t{}",
            kind,
            crate::decompiler::lifter::current_dce_proto(),
            pc,
            target,
            end,
            what
        );
    }
}

fn insn_size(op: LuauOpcode) -> usize {
    if op.has_aux() { 2 } else { 1 }
}

/// Start pc of the last instruction of `[start, end)`, found by walking
/// instruction-aligned from `start` (never `end - 1`, which can be an AUX word).
fn last_insn_start(code: &[u32], start: usize, end: usize) -> Option<usize> {
    let mut w = start;
    let mut last = None;
    while w < end && w < code.len() {
        last = Some(w);
        w += insn_size(LuauOpcode::from_u8(insn_op(code[w])));
    }
    // A walk that overshoots `end` means the range does not end on an
    // instruction boundary; refuse rather than guess.
    if w == end { last } else { None }
}

/// Target of the unconditional forward `JUMP` at `pc`, or None. JUMPX is not
/// followed (the compiler never folds long jumps).
fn fwd_jump_target(code: &[u32], pc: usize) -> Option<usize> {
    let insn = *code.get(pc)?;
    if LuauOpcode::from_u8(insn_op(insn)) != LuauOpcode::Jump {
        return None;
    }
    let d = insn_d(insn) as i64;
    if d < 0 {
        return None;
    }
    Some((pc as i64 + d + 1) as usize)
}

/// Does a conditional jump in `[from, to)` target a pc in `(lo, hi]`?
///
/// An alias SHRINKS the window an inner jump is lifted in: a jump that was in
/// range of the old window (`<= hi`) but lands past the new one (`> lo`)
/// would turn from an inline if into an out-of-range drop. `5348f2ef
/// UpdateColor`: `if Grid and Grid.Highlighted then .. else .. end` - the
/// `Highlighted` link targets the else start, which is in range of the old
/// then-arm `[9, 31)` but past the aliased one `[9, 30)`, so the link was
/// dropped. Such an alias is refused.
fn jumps_into(code: &[u32], from: usize, to: usize, lo: usize, hi: usize) -> bool {
    let mut w = from;
    while w < to && w < code.len() {
        let op = LuauOpcode::from_u8(insn_op(code[w]));
        if matches!(
            op,
            LuauOpcode::JumpIf
                | LuauOpcode::JumpIfNot
                | LuauOpcode::JumpIfEq
                | LuauOpcode::JumpIfNotEq
                | LuauOpcode::JumpIfLE
                | LuauOpcode::JumpIfNotLE
                | LuauOpcode::JumpIfLT
                | LuauOpcode::JumpIfNotLT
                | LuauOpcode::JumpXEqKNil
                | LuauOpcode::JumpXEqKB
                | LuauOpcode::JumpXEqKN
                | LuauOpcode::JumpXEqKS
        ) {
            let t = (w as i64 + insn_d(code[w]) as i64 + 1) as usize;
            if t > lo && t <= hi {
                return true;
            }
        }
        w += insn_size(op);
    }
    false
}

fn is_unconditional_exit(op: LuauOpcode) -> bool {
    matches!(
        op,
        LuauOpcode::Return | LuauOpcode::Jump | LuauOpcode::JumpBack | LuauOpcode::JumpX
    )
}

/// Does any register written by the skipped region `[from, to)` carry a VALUE
/// across the join `t`? An `a and b and c` chain, or an if/else that assigns
/// one variable, leaves its result in a register that is READ at the join
/// before any other write. Re-aiming the jump at the range end hands the
/// merge machinery an arm whose real continuation lies outside the range, so
/// it binds the arms' writes to a throwaway local instead of the variable the
/// join reads (`IceSkating`: `local Trail2 = a and a:FindFirstChild(..)` was
/// never assigned; `CogsPerRound`: `Op ..= ".."` after an `Add` arm read an
/// `Op` that no arm wrote). Such an alias is refused.
fn carries_value(code: &[u32], from: usize, to: usize, t: usize) -> bool {
    carries_value_except(code, from, to, t, &[])
}

/// [`carries_value`] ignoring `pre_init` registers: ones that already hold a
/// value from BEFORE the construct (see [`preinitialised`]). A write to such
/// a register is a plain assignment to an existing variable, not the creation
/// of a merge value, so the throwaway-local hazard above cannot arise.
fn carries_value_except(code: &[u32], from: usize, to: usize, t: usize, pre_init: &[usize]) -> bool {
    const JOIN_WINDOW: usize = 64;
    let hi = to.min(code.len());
    super::collect_body_writes(code, from, hi)
        .into_iter()
        .filter(|r| !pre_init.contains(r))
        .any(|r| super::reg_read_before_write_ex(code, t, (t + JOIN_WINDOW).min(code.len()), r, true))
}

/// Registers whose LAST write before the range `[start, ..)` is a literal load
/// (`LOADNIL`/`LOADB`/`LOADN`/`LOADK`) and that are not rewritten between
/// `start` and the jump at `pc`. Such a register is a variable the range only
/// updates conditionally (`local a, b = false, 0` then `if c then a = true end`);
/// an `a and b` / if-else merge value is instead created by the code leading to
/// the jump, never by a literal declaration. `PropsBillboardSlider` proto@190:
/// `LOADB R2 / LOADN R3` at pc 14-15, then the X/Y/else arms (each ending in
/// `JUMP -> 93`) assign R2/R3 under an inner `JUMPIFNOTLT -> 93`; that inner
/// guard was dropped as a "value chain" and its body ran unconditionally.
fn preinitialised(code: &[u32], start: usize, pc: usize) -> Vec<usize> {
    let mut last_literal: std::collections::BTreeMap<usize, bool> = std::collections::BTreeMap::new();
    let mut w = 0;
    while w < start && w < code.len() {
        let op = LuauOpcode::from_u8(insn_op(code[w]));
        let size = insn_size(op);
        let literal = matches!(
            op,
            LuauOpcode::LoadNil | LuauOpcode::LoadB | LuauOpcode::LoadN | LuauOpcode::LoadK
        );
        for r in super::collect_body_writes(code, w, w + size) {
            last_literal.insert(r, literal);
        }
        w += size;
    }
    let inside = super::collect_body_writes(code, start, pc);
    last_literal
        .into_iter()
        .filter(|(r, literal)| *literal && !inside.contains(r))
        .map(|(r, _)| r)
        .collect()
}

/// S1 EXIT ALIAS. Is the out-of-range target `t` the place the range
/// `[start, end)` itself exits to? Returns the in-range pc the jump may be
/// retargeted to without changing any execution path:
///
/// * the range's LAST instruction `lp` is `JUMP -> t`: jumping to `t` and
///   jumping to `lp` are the same path, so the target becomes `lp`;
/// * the range falls through its end (its last instruction is not an
///   unconditional exit) into `code[end]`, which is `JUMP -> t`: jumping to
///   `t` is falling off the range, so the target becomes `end`.
///
/// Evidence: `2b4fdb6e updateMenuToItem` pc397 `JUMPIFNOT Discount -> 419`
/// inside the arm 383..411 whose final `410 JUMP -> 419`: the guard was
/// dropped and ` (N% Off)` was concatenated onto every price, erroring on a
/// nil Discount. `b88d6cad CreateCamera` pc650 -> 654, where the then-arm
/// [647,652) is followed by its else-skip `652 JUMP -> 654`: `if TouchEnabled`
/// was lost.
pub(super) fn range_exit_alias(
    code: &[u32],
    proto: &Proto,
    pc: usize,
    start: usize,
    end: usize,
    t: usize,
    in_loop: bool,
) -> Option<usize> {
    if in_loop || t <= end || disabled(proto) {
        return None;
    }
    let lp = last_insn_start(code, start, end)?;
    let alias = if fwd_jump_target(code, lp) == Some(t) {
        Some(lp)
    } else {
        let lop = LuauOpcode::from_u8(insn_op(code[lp]));
        if !is_unconditional_exit(lop) && end < code.len() && fwd_jump_target(code, end) == Some(t) {
            Some(end)
        } else {
            None
        }
    };
    let alias = alias.filter(|&al| al > pc);
    if let Some(al) = alias {
        let jump_next = pc + insn_size(LuauOpcode::from_u8(insn_op(code[pc])));
        if jumps_into(code, jump_next, al, al, end) {
            trace("ALIAS", pc, t, end, "refused:shrinks_window");
            return None;
        }
        if on_foreign_path(proto, pc) {
            trace("ALIAS", pc, t, end, "refused:foreign_path");
            return None;
        }
        if carries_value_except(code, jump_next, al, t, &preinitialised(code, start, pc)) {
            trace("ALIAS", pc, t, end, "refused:value_chain");
            return None;
        }
        trace("ALIAS", pc, t, end, &format!("-> {}", al));
        note_fired(proto);
    }
    alias
}

/// S1 for `detect_else_skip`: the then-arm's else-skip `JUMP` at `last_pc`
/// goes past `end`. If that destination is where the range exits to (the same
/// two rules as [`range_exit_alias`], with the else arm running to `end`), the
/// diamond is `[then_start, last_pc)` / `[target, end)`. Without this the
/// elseif chain inside a sub-range is flattened into sequential ifs
/// (`a9295351 new@23`: `AssignInboxFetcher` ran for every `p4`).
pub(super) fn else_skip_exit_alias(
    code: &[u32],
    proto: &Proto,
    pc: usize,
    then_start: usize,
    target: usize,
    end: usize,
    last_pc: usize,
    jump_target: usize,
    in_loop: bool,
) -> bool {
    if in_loop || disabled(proto) || target >= end || jump_target <= end {
        return false;
    }
    // `last_pc` must be a real instruction boundary of the then-arm.
    if last_insn_start(code, then_start, target) != Some(last_pc) {
        return false;
    }
    // The else arm [target, end) must itself fall off its end into
    // `code[end] = JUMP -> jump_target`.
    let Some(elp) = last_insn_start(code, target, end) else { return false };
    let elop = LuauOpcode::from_u8(insn_op(code[elp]));
    let ok = (fwd_jump_target(code, elp) == Some(jump_target))
        || (!is_unconditional_exit(elop) && end < code.len() && fwd_jump_target(code, end) == Some(jump_target));
    if ok && jumps_into(code, then_start, last_pc, last_pc, target) {
        trace("ELSE_ALIAS", last_pc, jump_target, end, "refused:shrinks_window");
        return false;
    }
    if ok && on_foreign_path(proto, pc) {
        trace("ELSE_ALIAS", last_pc, jump_target, end, "refused:foreign_path");
        return false;
    }
    if ok && carries_value(code, then_start, end, jump_target) {
        trace("ELSE_ALIAS", last_pc, jump_target, end, "refused:value_chain");
        return false;
    }
    if ok {
        trace("ELSE_ALIAS", last_pc, jump_target, end, &format!("then=[{},{}) else=[{},{})", then_start, last_pc, target, end));
        note_fired(proto);
    }
    ok
}

/// Is `e` safe to evaluate a SECOND time as a returned value? The early return
/// re-states the value a register holds at the jump site: a call would run
/// again, a closure or table constructor would mint a fresh object whose
/// identity differs from the fall-through's, and varargs truncate differently.
fn is_stable_value(e: &Expr) -> bool {
    fn has_table_or_varargs(e: &Expr) -> bool {
        match e {
            Expr::Table { .. } | Expr::Varargs => true,
            Expr::Field { object, .. } => has_table_or_varargs(object),
            Expr::Index { object, key } => has_table_or_varargs(object) || has_table_or_varargs(key),
            Expr::BinOp { left, right, .. } => has_table_or_varargs(left) || has_table_or_varargs(right),
            Expr::UnOp { operand, .. } => has_table_or_varargs(operand),
            Expr::Ternary { cond, then_expr, else_expr } => {
                has_table_or_varargs(cond) || has_table_or_varargs(then_expr) || has_table_or_varargs(else_expr)
            }
            _ => false,
        }
    }
    is_pure_expr(e) && !has_table_or_varargs(e)
}

fn is_literal(e: &Expr) -> bool {
    matches!(e, Expr::Nil | Expr::Bool(_) | Expr::Number(_) | Expr::String(_) | Expr::Vector(..))
}

/// One value of a terminal return: already known, or the jump's own tested
/// register whose (call) value must be bound to a local first.
pub(super) enum RetVal {
    Known(Expr),
    Operand(usize),
}

/// S2 TERMINAL-RETURN GUARD. A forward conditional jump to `t`, where `t`
/// starts a TERMINAL block, behaves exactly like `if <fire> then return v1..vn
/// end` placed at the jump. A terminal block is 0-3 of {LOADNIL, LOADB with
/// C==0, LOADN, LOADK of a literal} followed by `RETURN A B` with 1 <= B <= 11
/// (never B==0: multret depends on `top`). The values are the RETURN operands
/// in the JUMP-SITE register state (the block's own literal loads override).
/// This holds however the parent carved the range.
///
/// Evidence (v12 oracle, read against disassembly and Potassium):
/// `29029f13 Close` pc73 `JUMPXEQKNIL Frame -> 116 RETURN` was dropped, so
/// Close dereferenced a nil Frame; `40368de6 strCommas` pc23
/// `JUMPIF R1 -> 38 RETURN R1` was dropped, so `A and B or C` always returned
/// C; `4a9b495f Give` lost `not p12.Mute` guards to its terminal `RETURN R4`.
///
/// Refusals (each keeps today's output): an Unknown tested register (the fire
/// condition would mint a `vN`); tested registers that ALL hold literals (a
/// real test is never constant-folded by the compiler, so the literal is a
/// stale value from another path - `StealABrainrot DuelsMachineController`
/// proto@655 pc137 would otherwise become an unconditional return that deletes
/// the rest of the function); a returned register not written by the block
/// that holds a literal (same staleness) or an Unknown / unstable value.
/// `operand_regs[0]`, when `allow_operand_bind`, may hold an unstable value:
/// the caller binds it to a local first and tests and returns that name.
pub(super) fn terminal_return_at(
    ctx: &DecompileContext,
    proto: &Proto,
    regs: &[RegVal],
    t: usize,
    operand_regs: &[usize],
    allow_operand_bind: bool,
) -> Result<Vec<RetVal>, &'static str> {
    for &r in operand_regs {
        if !matches!(regs.get(r), Some(RegVal::Expr(_)) | Some(RegVal::LoopVar(_))) {
            return Err("operand_unknown");
        }
    }
    if !operand_regs.is_empty()
        && operand_regs
            .iter()
            .all(|&r| matches!(regs.get(r), Some(RegVal::Expr(e)) if is_literal(e)))
    {
        return Err("operand_literal");
    }
    let code = &proto.code;
    let mut scratch: Vec<(usize, Expr)> = Vec::new();
    let mut pc = t;
    for _ in 0..=3 {
        let Some(&insn) = code.get(pc) else { return Err("oob") };
        let op = LuauOpcode::from_u8(insn_op(insn));
        let a = insn_a(insn) as usize;
        match op {
            LuauOpcode::Return => {
                let b = insn_b(insn) as usize;
                if b == 0 {
                    return Err("multret");
                }
                if b > 11 {
                    return Err("wide");
                }
                let mut values = Vec::with_capacity(b - 1);
                for r in a..a + b - 1 {
                    if let Some((_, e)) = scratch.iter().rev().find(|(sr, _)| *sr == r) {
                        values.push(RetVal::Known(e.clone()));
                        continue;
                    }
                    let e = match regs.get(r) {
                        Some(RegVal::Expr(e)) if is_literal(e) => return Err("value_reg_literal"),
                        Some(RegVal::Expr(e)) => e.clone(),
                        Some(RegVal::LoopVar(s)) => Expr::Name(s.clone()),
                        _ => return Err("value_unknown"),
                    };
                    if is_stable_value(&e) {
                        values.push(RetVal::Known(sanitize_leaked_global_string(e)));
                    } else if allow_operand_bind
                        && operand_regs.first() == Some(&r)
                        && !matches!(e, Expr::Function { .. } | Expr::Table { .. } | Expr::Varargs)
                    {
                        values.push(RetVal::Operand(r));
                    } else {
                        return Err("value_unstable");
                    }
                }
                return Ok(values);
            }
            LuauOpcode::LoadNil => scratch.push((a, Expr::Nil)),
            LuauOpcode::LoadB => {
                if insn_c(insn) != 0 {
                    return Err("loadb_skip");
                }
                scratch.push((a, Expr::Bool(insn_b(insn) != 0)));
            }
            LuauOpcode::LoadN => scratch.push((a, Expr::Number(insn_d(insn) as f64))),
            LuauOpcode::LoadK => {
                let k = insn_d(insn) as u16 as usize;
                let e = proto
                    .constants
                    .get(k)
                    .map(|c| constant_to_expr(c, &ctx.chunk.strings, &proto.constants))
                    .ok_or("k_oob")?;
                if !matches!(e, Expr::Nil | Expr::Bool(_) | Expr::Number(_) | Expr::String(_)) {
                    return Err("k_kind");
                }
                scratch.push((a, e));
            }
            _ => return Err("not_terminal"),
        }
        pc += insn_size(op);
    }
    Err("too_long")
}

/// Gate + trace wrapper for the three non-loop out-of-range arms. Returns the
/// return plan when S2 applies; otherwise records the jump as dropped (the
/// flat walk runs past it) and returns None.
pub(super) fn oor_terminal_return(
    ctx: &DecompileContext,
    proto: &Proto,
    regs: &[RegVal],
    pc: usize,
    target: usize,
    end: usize,
    operand_regs: &[usize],
    allow_operand_bind: bool,
) -> Option<Vec<RetVal>> {
    let r = if disabled(proto) {
        Err("killed")
    } else if on_foreign_path(proto, pc) {
        Err("foreign_path")
    } else {
        terminal_return_at(ctx, proto, regs, target, operand_regs, allow_operand_bind)
    };
    match r {
        Ok(v) => {
            trace("RET", pc, target, end, "FIRE");
            note_fired(proto);
            Some(v)
        }
        Err(why) => {
            if std::env::var("LUAU_OOR_TRACE").is_ok() {
                eprintln!("OORDROP proto={} pc={} target={} end={} top={:?} why={}", crate::decompiler::lifter::current_dce_proto(), pc, target, end, proto.code.get(target).map(|&i| LuauOpcode::from_u8(insn_op(i))), why);
            }
            trace("RET", pc, target, end, why);
            note_dropped(proto, target);
            None
        }
    }
}

/// Should the or/and-chain fold at `pc` yield to the S2 guard? True when the
/// chain's join lies past the range and is a terminal RETURN block: its later
/// segments are the OTHER path of `c and A or B`, which the parent lifts again
/// as fall-through, so folding stored `A or B` into a register the re-lift
/// overwrites and the function always returned B.
#[allow(clippy::too_many_arguments)]
pub(super) fn chain_yields_to_terminal_return(
    ctx: &DecompileContext,
    proto: &Proto,
    regs: &[RegVal],
    join: usize,
    end: usize,
    target: usize,
    tested_reg: usize,
    in_loop: bool,
) -> bool {
    !in_loop
        && join > end
        && target > end
        && !disabled(proto)
        && terminal_return_at(ctx, proto, regs, join, &[tested_reg], true).is_ok()
}

/// Build the `return` for a plan whose values are all known (the operand, if
/// any, has already been bound to `bound`).
pub(super) fn build_return(plan: Vec<RetVal>, bound: Option<&Expr>) -> Option<Stat> {
    let mut values = Vec::with_capacity(plan.len());
    for v in plan {
        match v {
            RetVal::Known(e) => values.push(e),
            RetVal::Operand(_) => values.push(bound?.clone()),
        }
    }
    Some(Stat::Return { values })
}

/// The register a plan needs bound to a local before the guard, if any.
pub(super) fn plan_operand(plan: &[RetVal]) -> Option<usize> {
    plan.iter().find_map(|v| match v {
        RetVal::Operand(r) => Some(*r),
        _ => None,
    })
}
