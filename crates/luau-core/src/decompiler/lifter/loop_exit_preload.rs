//! Loop-exit flag preload.
//!
//! Shape: a `for` loop whose normal exit runs ONE constant load (`LOADNIL R` or
//! `LOADB R b` with no skip) that a `break` arm jumps PAST, landing on the
//! instruction right after it, where `R` is then tested:
//!
//! ```text
//!   body:   ...  MOVE|LOADB|LOADN|LOADK|LOADNIL R ..;  JUMP -> X+1   (break arms)
//!   X:      LOADNIL R | LOADB R b                                   (normal exit)
//!   X+1:    JUMPIF R ...                                            (join)
//! ```
//!
//! The lifter walked X then X+1 linearly and folded the test against the
//! parked constant, which the break edge never passes through, so the join's
//! guard and everything after it was dropped. The faithful source is
//! `local R = <const>` BEFORE the loop, `R = <value>; break` in the arm, and no
//! load at X. This is only equivalent when `R` is touched in the loop by those
//! arm writes alone, which `detect` checks.

use super::*;
use super::opcode_handlers::{insn_reads_reg, insn_writes_regs, reg_read_before_overwrite};

/// What to preload: the load's pc, its register and the constant it stores.
pub(super) struct ExitPreload {
    pub load_pc: usize,
    pub reg: usize,
    pub value: Expr,
}

fn step(op: LuauOpcode) -> usize {
    if op.has_aux() { 2 } else { 1 }
}

fn jump_target(code: &[u32], pc: usize) -> Option<usize> {
    let insn = *code.get(pc)?;
    match LuauOpcode::from_u8(insn_op(insn)) {
        LuauOpcode::Jump => Some((pc as i64 + 1 + insn_d(insn) as i64) as usize),
        LuauOpcode::JumpX => Some((pc as i64 + 1 + insn_e(insn) as i64) as usize),
        _ => None,
    }
}

/// Does any branch other than a plain in-loop `JUMP` land on `join`? Such an
/// edge (an arm outside the loop, a conditional) is a path the preload cannot
/// model, so the shape is refused.
fn join_has_other_preds(code: &[u32], prep_pc: usize, loop_pc: usize, join: usize) -> bool {
    let mut pc = 0usize;
    while pc < code.len() {
        let op = LuauOpcode::from_u8(insn_op(code[pc]));
        let target = match op {
            LuauOpcode::Jump | LuauOpcode::JumpBack => Some(pc as i64 + 1 + insn_d(code[pc]) as i64),
            LuauOpcode::JumpX => Some(pc as i64 + 1 + insn_e(code[pc]) as i64),
            LuauOpcode::JumpIf | LuauOpcode::JumpIfNot | LuauOpcode::JumpIfEq
            | LuauOpcode::JumpIfNotEq | LuauOpcode::JumpIfLE | LuauOpcode::JumpIfNotLE
            | LuauOpcode::JumpIfLT | LuauOpcode::JumpIfNotLT | LuauOpcode::JumpXEqKNil
            | LuauOpcode::JumpXEqKB | LuauOpcode::JumpXEqKN | LuauOpcode::JumpXEqKS => {
                Some(pc as i64 + 1 + insn_d(code[pc]) as i64)
            }
            _ => None,
        };
        if target == Some(join as i64) {
            let in_loop_jump = op == LuauOpcode::Jump && pc > prep_pc && pc < loop_pc;
            if !in_loop_jump {
                return true;
            }
        }
        pc += step(op);
    }
    false
}

fn is_control_flow(op: LuauOpcode) -> bool {
    matches!(
        op,
        LuauOpcode::Jump | LuauOpcode::JumpBack | LuauOpcode::JumpX | LuauOpcode::JumpIf
            | LuauOpcode::JumpIfNot | LuauOpcode::JumpIfEq | LuauOpcode::JumpIfNotEq
            | LuauOpcode::JumpIfLE | LuauOpcode::JumpIfNotLE | LuauOpcode::JumpIfLT
            | LuauOpcode::JumpIfNotLT | LuauOpcode::JumpXEqKNil | LuauOpcode::JumpXEqKB
            | LuauOpcode::JumpXEqKN | LuauOpcode::JumpXEqKS | LuauOpcode::Return
            | LuauOpcode::ForNPrep | LuauOpcode::ForNLoop | LuauOpcode::ForGPrep
            | LuauOpcode::ForGLoop | LuauOpcode::ForGPrepNext | LuauOpcode::ForGPrepINext
    )
}

/// Find the exit load: the first instruction at or after `from` that is a
/// constant load, reached through straight-line code only. Other statements
/// may precede it (Luau emits an `error(...)` call there for an inlined search
/// helper); control flow before it means the exit run is not a single path.
fn find_exit_load(code: &[u32], from: usize) -> Option<usize> {
    let mut pc = from;
    while pc < code.len() {
        let op = LuauOpcode::from_u8(insn_op(code[pc]));
        if matches!(op, LuauOpcode::LoadNil | LuauOpcode::LoadB) {
            return Some(pc);
        }
        if is_control_flow(op) {
            return None;
        }
        pc += step(op);
    }
    None
}

fn is_loop_op(op: LuauOpcode) -> bool {
    matches!(
        op,
        LuauOpcode::ForNPrep | LuauOpcode::ForNLoop | LuauOpcode::ForGPrep
            | LuauOpcode::ForGLoop | LuauOpcode::ForGPrepNext | LuauOpcode::ForGPrepINext
    )
}

/// Does any instruction in `[start, end)` read or write `reg`? (Precise: uses
/// the per-opcode operand model, not a raw A/B/C scan, whose D-byte overlap
/// reports a LOADN's immediate as a register.)
fn exit_run_touches(code: &[u32], start: usize, end: usize, reg: usize) -> bool {
    let mut pc = start;
    while pc < end.min(code.len()) {
        if insn_reads_reg(code, pc, reg) || insn_writes_regs(code, pc).contains(&reg) {
            return true;
        }
        pc += step(LuauOpcode::from_u8(insn_op(code[pc])));
    }
    false
}

/// Does any plain jump land in `[start, end)`? Loop prep/latch edges are
/// ignored: the prep's skip edge enters the exit run legitimately.
fn any_jump_lands_in(code: &[u32], start: usize, end: usize) -> bool {
    let mut pc = 0usize;
    while pc < code.len() {
        let word = code[pc];
        let op = LuauOpcode::from_u8(insn_op(word));
        if is_control_flow(op) && !is_loop_op(op) && op != LuauOpcode::Return {
            let target = match op {
                LuauOpcode::JumpX => pc as i64 + 1 + insn_e(word) as i64,
                _ => pc as i64 + 1 + insn_d(word) as i64,
            };
            if target >= start as i64 && target < end as i64 {
                return true;
            }
        }
        pc += step(op);
    }
    false
}

pub(super) fn detect(code: &[u32], prep_pc: usize, loop_pc: usize) -> Option<ExitPreload> {
    if loop_pc >= code.len() || prep_pc >= loop_pc {
        return None;
    }
    let after_loop = loop_pc + step(LuauOpcode::from_u8(insn_op(code[loop_pc])));
    let load_pc = find_exit_load(code, after_loop)?;
    let insn = code[load_pc];
    let join = load_pc + 1;
    if join >= code.len() {
        return None;
    }
    let reg = insn_a(insn) as usize;
    let value = match LuauOpcode::from_u8(insn_op(insn)) {
        LuauOpcode::LoadNil => Expr::Nil,
        LuauOpcode::LoadB if insn_c(insn) == 0 => Expr::Bool(insn_b(insn) != 0),
        _ => return None,
    };
    // After the join R must be read before it is rewritten, either by the join
    // itself (a JUMPIF/JUMPIFNOT test) or by a later statement: otherwise the
    // break arms' writes are dead and R's value after the loop is not what is
    // read. (A join that only rewrites R, as in AdoptMe VehicleClient, fails.)
    let join_word = code[join];
    let join_tests_reg = matches!(
        LuauOpcode::from_u8(insn_op(join_word)),
        LuauOpcode::JumpIf | LuauOpcode::JumpIfNot
    ) && insn_a(join_word) as usize == reg;
    if !join_tests_reg && !reg_read_before_overwrite(code, join, reg) {
        return None;
    }
    // A non-empty exit run must be reachable only by falling out of the loop,
    // and must not touch R (the preload would reorder around it).
    if load_pc > after_loop
        && (any_jump_lands_in(code, after_loop, load_pc + 1)
            || exit_run_touches(code, after_loop, load_pc, reg))
    {
        return None;
    }
    if join_has_other_preds(code, prep_pc, loop_pc, join) {
        return None;
    }
    let mut arm_writes = 0usize;
    let mut pc = prep_pc + 1;
    while pc < loop_pc {
        let word = code[pc];
        let op = LuauOpcode::from_u8(insn_op(word));
        let n = step(op);
        let is_arm_write = matches!(
            op,
            LuauOpcode::Move | LuauOpcode::LoadB | LuauOpcode::LoadN
                | LuauOpcode::LoadK | LuauOpcode::LoadNil
        ) && insn_a(word) as usize == reg;
        if is_arm_write {
            let reads_self = op == LuauOpcode::Move && insn_b(word) as usize == reg;
            let skips = op == LuauOpcode::LoadB && insn_c(word) != 0;
            if reads_self || skips || jump_target(code, pc + n) != Some(join) {
                return None;
            }
            arm_writes += 1;
            pc += n;
            continue;
        }
        if reg_used_in_range(code, pc, pc + n, reg) {
            return None;
        }
        // A break to the join that no write precedes would carry the OLD value
        // of R, which the preload would replace.
        if jump_target(code, pc) == Some(join) {
            let prev_is_write = (prep_pc + 1..pc).rev().find(|&p| {
                p + step(LuauOpcode::from_u8(insn_op(code[p]))) == pc
            });
            let ok = prev_is_write.map_or(false, |p| {
                let pw = code[p];
                insn_a(pw) as usize == reg
                    && matches!(
                        LuauOpcode::from_u8(insn_op(pw)),
                        LuauOpcode::Move | LuauOpcode::LoadB | LuauOpcode::LoadN
                            | LuauOpcode::LoadK | LuauOpcode::LoadNil
                    )
            });
            if !ok {
                return None;
            }
        }
        pc += n;
    }
    if arm_writes == 0 {
        return None;
    }
    Some(ExitPreload { load_pc, reg, value })
}

/// Declare `local R = <const>` before the loop and arrange for the exit load to
/// be skipped. Call after the iterator/limit setup has been absorbed.
pub(super) fn apply(
    ctx: &mut DecompileContext,
    proto: &Proto,
    proto_index: usize,
    regs: &mut Vec<RegVal>,
    locals: &mut LocalTracker,
    stmts: &mut Vec<Stat>,
    code_range: (usize, usize),
) {
    let Some(pre) = detect(&proto.code, code_range.0, code_range.1) else { return };
    if pre.reg >= regs.len() {
        regs.resize(pre.reg + 8, RegVal::Unknown);
    }
    regs[pre.reg] = RegVal::Expr(pre.value);
    premateralize_loop_carried(
        ctx, proto, regs, locals, stmts, &[pre.reg], &[], code_range.0,
        Some((code_range.0, pre.load_pc + 2)),
    );
    ctx.preloaded_exit_loads.insert((proto_index, pre.load_pc));
}
