//! Jump threading for an inlined boolean helper whose result feeds a RETURN
//! guard.
//!
//! The Luau inliner turns `if not helper() then return end`, with
//! `helper = function() return a and f(b) and not c end`, into
//!
//! ```text
//!     <conditional jumps, every one -> F, with calls between them>
//!  T: LOADB   R true
//!     JUMP    -> J
//!  F: LOADB   R false
//!  J: JUMPIF  R -> +1        ; continue when R is truthy
//!     RETURN
//!  C: <the rest of the function>
//! ```
//!
//! The chain holds calls, so `find_value_join` (pure blocks only) refuses it;
//! the lifter then reads `J:` as the proto's final return and drops `C:` and
//! everything after it (BloxFruits `AnimateEntrance`: 12 touch handlers kept 2
//! of ~22 calls each). The value in `R` is only ever tested by the guard, so
//! the chain is a plain or-chain return guard: every jump to `F` is a jump to
//! the RETURN, and the `T:` path continues at `C:`. Threading the jumps that
//! way, and NOP-ing the two boolean loads and the test, hands the existing
//! guard machinery exactly the shape it already structures.
//!
//! Soundness gates (any failure leaves the proto untouched):
//!   * the layout is exactly `LOADB R <v>; JUMP J; LOADB R <w>; J: JUMPIF[NOT] R +1; RETURN 0`
//!     with `v` continuing and `w` returning;
//!   * nothing but the `T:` JUMP targets `J` or the RETURN (no other value of
//!     `R` can reach the test);
//!   * `R` is dead after the guard (control-flow-aware liveness).

use crate::parser::opcodes::LuauOpcode;
use crate::parser::types::{insn_a, insn_b, insn_c, insn_d, insn_op, Proto};

const NOP: u32 = 0;

fn with_d(word: u32, d: i64) -> Option<u32> {
    let d16 = i16::try_from(d).ok()?;
    Some((word & 0xFFFF) | (u32::from(d16 as u16) << 16))
}

fn is_d_jump(op: LuauOpcode) -> bool {
    matches!(
        op,
        LuauOpcode::Jump
            | LuauOpcode::JumpIf
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
    )
}

/// Returns a patched copy of `proto` when it holds the boolean-guard shape.
pub(super) fn thread(proto: &Proto) -> Option<Proto> {
    let code = &proto.code;
    // A proto that also materialises a boolean for a non-return test keeps its
    // old shape: that value join binds its conditions wrongly once the body is
    // no longer dropped (BloxFruits AnimateEntrance `dropped_arm_value`).
    if has_non_return_bool_join(code) {
        return None;
    }
    let mut patched: Option<Vec<u32>> = None;
    let mut pc = 0usize;
    while pc + 4 < code.len() {
        let op = LuauOpcode::from_u8(insn_op(code[pc]));
        let step = 1 + usize::from(op.has_aux());
        let work: &[u32] = patched.as_deref().unwrap_or(code);
        if let Some(next) = try_site(work, pc) {
            patched = Some(next);
        }
        pc += step;
    }
    let code = patched?;
    let mut out = proto.clone();
    out.code = code;
    Some(out)
}

/// True when some `LOADB R v; JUMP +1; LOADB R w; JUMPIF[NOT] R` boolean
/// materialisation is NOT followed by a bare RETURN.
fn has_non_return_bool_join(code: &[u32]) -> bool {
    let is = |x: u32, o: LuauOpcode| LuauOpcode::from_u8(insn_op(x)) == o;
    let mut pc = 0usize;
    while pc + 3 < code.len() {
        let op = LuauOpcode::from_u8(insn_op(code[pc]));
        let step = 1 + usize::from(op.has_aux());
        if op == LuauOpcode::LoadB
            && is(code[pc + 1], LuauOpcode::Jump)
            && is(code[pc + 2], LuauOpcode::LoadB)
            && insn_a(code[pc + 2]) == insn_a(code[pc])
            && matches!(
                LuauOpcode::from_u8(insn_op(code[pc + 3])),
                LuauOpcode::JumpIf | LuauOpcode::JumpIfNot
            )
            && !code.get(pc + 4).is_some_and(|&r| is(r, LuauOpcode::Return) && insn_b(r) == 1)
        {
            return true;
        }
        pc += step;
    }
    false
}

/// `pc` is the candidate `T: LOADB R <v>`.
fn try_site(code: &[u32], pc: usize) -> Option<Vec<u32>> {
    let w = |i: usize| code.get(i).copied();
    let (t_load, t_jump, f_load, test, ret) = (w(pc)?, w(pc + 1)?, w(pc + 2)?, w(pc + 3)?, w(pc + 4)?);
    let is = |x: u32, o: LuauOpcode| LuauOpcode::from_u8(insn_op(x)) == o;
    if !is(t_load, LuauOpcode::LoadB) || !is(t_jump, LuauOpcode::Jump) || !is(f_load, LuauOpcode::LoadB) {
        return None;
    }
    let reg = insn_a(t_load);
    if insn_a(f_load) != reg || insn_c(t_load) != 0 || insn_c(f_load) != 0 {
        return None;
    }
    // J is pc+3: the JUMP must land exactly there.
    if pc as i64 + 1 + i64::from(insn_d(t_jump)) + 1 != (pc + 3) as i64 {
        return None;
    }
    let (v, wv) = (insn_b(t_load) != 0, insn_b(f_load) != 0);
    let continue_when_truthy = match LuauOpcode::from_u8(insn_op(test)) {
        LuauOpcode::JumpIf => true,
        LuauOpcode::JumpIfNot => false,
        _ => return None,
    };
    if insn_a(test) != reg || insn_d(test) != 1 {
        return None;
    }
    // RETURN with no values.
    if !is(ret, LuauOpcode::Return) || insn_b(ret) != 1 {
        return None;
    }
    // T continues, F returns.
    if v != continue_when_truthy || wv == continue_when_truthy {
        return None;
    }
    let (f_pc, test_pc, ret_pc, cont_pc) = (pc + 2, pc + 3, pc + 4, pc + 5);
    // R is dead after the guard.
    if !reg_dead_after(code, cont_pc, reg) {
        return None;
    }
    let mut out = code.to_vec();
    let mut f_hits = 0usize;
    let mut i = 0usize;
    while i < code.len() {
        let op = LuauOpcode::from_u8(insn_op(code[i]));
        let step = 1 + usize::from(op.has_aux());
        if i != pc + 1 && is_d_jump(op) && !(f_pc..=ret_pc).contains(&i) {
            let target = i as i64 + 1 + i64::from(insn_d(code[i]));
            let to = if target == f_pc as i64 {
                f_hits += 1;
                Some(pc)
            } else if target == (pc + 1) as i64 || target == test_pc as i64 || target == ret_pc as i64 {
                // Another path reaches the test or the return with its own R.
                return None;
            } else {
                None
            };
            if let Some(to) = to {
                out[i] = with_d(code[i], to as i64 - (i as i64 + 1))?;
            }
        }
        i += step;
    }
    if f_hits == 0 {
        return None;
    }
    // The conditional that falls into `T:` becomes the standard guard: inverted,
    // it jumps OVER the RETURN to the continuation and falls into the RETURN.
    let last = prev_insn_pc(code, pc)?;
    let last_target = last as i64 + 1 + i64::from(insn_d(code[last]));
    if last_target != f_pc as i64 {
        return None;
    }
    let (inv_op, flip_aux) = invert(LuauOpcode::from_u8(insn_op(code[last])))?;
    out[last] = with_d(
        (code[last] & !0xFF) | u32::from(inv_op as u8),
        cont_pc as i64 - (last as i64 + 1),
    )?;
    if flip_aux {
        out[last + 1] ^= 1 << 31;
    }
    out[pc] = NOP;
    out[pc + 1] = NOP;
    out[f_pc] = NOP;
    out[test_pc] = NOP;
    Some(out)
}

fn prev_insn_pc(code: &[u32], pc: usize) -> Option<usize> {
    let mut i = 0usize;
    let mut prev = None;
    while i < pc {
        prev = Some(i);
        i += 1 + usize::from(LuauOpcode::from_u8(insn_op(code[i])).has_aux());
    }
    if i == pc { prev } else { None }
}

/// The exact logical complement of a conditional jump, and whether its AUX
/// word carries the negate flag.
fn invert(op: LuauOpcode) -> Option<(LuauOpcode, bool)> {
    use LuauOpcode as O;
    Some(match op {
        O::JumpIf => (O::JumpIfNot, false),
        O::JumpIfNot => (O::JumpIf, false),
        O::JumpIfEq => (O::JumpIfNotEq, false),
        O::JumpIfNotEq => (O::JumpIfEq, false),
        O::JumpIfLE => (O::JumpIfNotLE, false),
        O::JumpIfNotLE => (O::JumpIfLE, false),
        O::JumpIfLT => (O::JumpIfNotLT, false),
        O::JumpIfNotLT => (O::JumpIfLT, false),
        O::JumpXEqKNil | O::JumpXEqKB | O::JumpXEqKN | O::JumpXEqKS => (op, true),
        _ => return None,
    })
}

/// True when no path from `from` can still observe `reg`.
fn reg_dead_after(code: &[u32], from: usize, reg: u8) -> bool {
    !super::reg_live_at(code, from, usize::from(reg))
}
