//! Recognition of the compiler's "materialize a comparison as a boolean" idiom.
//!
//! Luau compiles `local x = a < b` to a *branch*, not to a value-producing
//! instruction:
//!
//! ```text
//!     JUMPIF<cmp> ... -> L      ; jump taken  => value is code[L].B
//!     LOADB Rx <v> +1           ; fallthrough => value is <v>; C=1 skips one word
//!   L: LOADB Rx <!v>
//! ```
//!
//! Left alone, the two halves land in different basic blocks and are lowered
//! into two different shapes — the fallthrough `LOADB` becomes a real statement
//! inside an `if`, while the target `LOADB` is merely parked in a register. The
//! result is a dead `local _ = false` plus an unconditional `true`, i.e. the
//! comparison is silently replaced by a constant.
//!
//! Recognising the pair up front lets the CFG keep it inside one basic block and
//! lets the lifter store the comparison straight into the destination register.

use crate::parser::opcodes::LuauOpcode;
use crate::parser::types::*;

/// A recognised compare-to-boolean region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoolIdiom {
    /// Register the boolean is written to.
    pub dest: usize,
    /// First pc after the whole idiom.
    pub end_pc: usize,
    /// Value produced when the conditional jump is TAKEN.
    pub taken_value: bool,
}

/// Is `op` a conditional jump whose target is encoded in the D field?
fn is_conditional_jump(op: LuauOpcode) -> bool {
    matches!(
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
    )
}

/// A recognised short-circuit `and` / `or` chain that merges its operands in a
/// single register.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrAndChain {
    /// Register holding both the tested value and the result.
    pub dest: usize,
    /// The join pc — first instruction after the chain.
    pub end_pc: usize,
    /// `true` for `or` (JUMPIF short-circuits on truthy), `false` for `and`.
    pub is_or: bool,
    /// Instruction ranges, one per operand after the first. The first operand
    /// is whatever `dest` already holds at the chain's first jump.
    pub segments: Vec<(usize, usize)>,
}

/// Try to recognise `a or b or c` / `a and b and c` merged in one register:
///
/// ```text
///     MOVE  Rt, Ra
///     JUMPIF Rt -> L        ; `or`  (JUMPIFNOT for `and`)
///     MOVE  Rt, Rb
///     JUMPIF Rt -> L
///     MOVE  Rt, Rc
///   L: <Rt is the result>
/// ```
///
/// The defining feature — and the whole shape gate — is that the register being
/// TESTED is the same register each arm WRITES, and every jump in the chain
/// targets the same join. A plain `if cond then <other reg> = x end` writes a
/// different register and therefore cannot match.
pub fn recognize_or_and_chain(code: &[u32], pc: usize) -> Option<OrAndChain> {
    if pc >= code.len() {
        return None;
    }
    let insn = code[pc];
    let op = LuauOpcode::from_u8(insn_op(insn));
    if !matches!(op, LuauOpcode::JumpIf | LuauOpcode::JumpIfNot) {
        return None;
    }
    let dest = insn_a(insn) as usize;
    let join = (pc as i32 + insn_d(insn) as i32 + 1) as usize;
    if join <= pc + 1 || join > code.len() {
        return None;
    }

    let mut segments: Vec<(usize, usize)> = Vec::new();
    let mut jump_pc = pc;
    loop {
        let seg_start = jump_pc + 1;
        // Look for the next jump of the SAME kind, on the SAME register, to the
        // SAME join. Scan instruction-aligned so AUX words are never decoded.
        let mut next_jump: Option<usize> = None;
        let mut i = seg_start;
        while i < join {
            let cur = code[i];
            let cur_op = LuauOpcode::from_u8(insn_op(cur));
            if cur_op == op
                && insn_a(cur) as usize == dest
                && (i as i32 + insn_d(cur) as i32 + 1) as usize == join
            {
                next_jump = Some(i);
                break;
            }
            // An operand of a short-circuit expression is straight-line value
            // code. A back-edge, loop header or return inside the span means
            // this is real control flow that merely happens to write `dest` on
            // its last instruction — e.g. a whole `while` body whose final
            // statement assigns the tested register. Folding that into
            // `dest = dest or <body>` would delete the loop.
            if matches!(
                cur_op,
                LuauOpcode::JumpBack
                    | LuauOpcode::Return
                    | LuauOpcode::ForNPrep
                    | LuauOpcode::ForNLoop
                    | LuauOpcode::ForGPrep
                    | LuauOpcode::ForGPrepINext
                    | LuauOpcode::ForGPrepNext
                    | LuauOpcode::ForGLoop
            ) {
                return None;
            }
            i += if cur_op.has_aux() { 2 } else { 1 };
        }
        let seg_end = next_jump.unwrap_or(join);
        if seg_end <= seg_start {
            return None;
        }
        // Each operand must END by writing the shared register, otherwise this
        // is ordinary control flow that happens to test `dest`.
        let last = code[seg_end - 1];
        let last_op = LuauOpcode::from_u8(insn_op(last));
        let writer_pc = if last_op == LuauOpcode::Nop && seg_end >= 2 {
            seg_end - 2
        } else {
            seg_end - 1
        };
        if insn_a(code[writer_pc]) as usize != dest {
            // The final word may be an AUX; step back one and retry.
            if writer_pc == 0 || insn_a(code[writer_pc - 1]) as usize != dest {
                return None;
            }
        }
        segments.push((seg_start, seg_end));
        match next_jump {
            Some(j) => jump_pc = j,
            None => break,
        }
    }

    if segments.is_empty() {
        return None;
    }
    Some(OrAndChain {
        dest,
        end_pc: join,
        is_or: op == LuauOpcode::JumpIf,
        segments,
    })
}

/// Try to recognise the compare-to-boolean idiom starting at the conditional
/// jump at `pc`.
///
/// The shape gate is deliberately exact — every one of these must hold, so a
/// hand-written `if c then x = false end` (which has no paired second LOADB at
/// the target) can never match:
///
/// * the instruction right after the jump (and its AUX word) is `LOADB` with
///   `C == 1`, i.e. it skips exactly one word;
/// * the word it skips over is precisely the jump's own target;
/// * the target is also a `LOADB`;
/// * both `LOADB`s write the same register with opposite values.
pub fn recognize_bool_idiom(code: &[u32], pc: usize) -> Option<BoolIdiom> {
    if pc >= code.len() {
        return None;
    }
    let insn = code[pc];
    let op = LuauOpcode::from_u8(insn_op(insn));
    if !is_conditional_jump(op) {
        return None;
    }

    let next_pc = if op.has_aux() { pc + 2 } else { pc + 1 };
    let target = (pc as i32 + insn_d(insn) as i32 + 1) as usize;

    // The fallthrough LOADB must sit immediately after the jump, and the word
    // its `C == 1` skip jumps over must be exactly the jump's own target.
    // `LOADB A B C` is `R(A) = B; pc += C`, so a LOADB at P with C == 1 resumes
    // at P + 2 and the skipped word is P + 1 — hence `target == next_pc + 1`.
    if target != next_pc + 1 || target >= code.len() {
        return None;
    }

    let fall = code[next_pc];
    let taken = code[target];
    if LuauOpcode::from_u8(insn_op(fall)) != LuauOpcode::LoadB
        || LuauOpcode::from_u8(insn_op(taken)) != LuauOpcode::LoadB
    {
        return None;
    }
    // C is the skip count: 1 on the fallthrough half, 0 on the target half.
    if insn_c(fall) != 1 || insn_c(taken) != 0 {
        return None;
    }
    // Same destination register, opposite boolean values.
    if insn_a(fall) != insn_a(taken) || insn_b(fall) == insn_b(taken) {
        return None;
    }

    Some(BoolIdiom {
        dest: insn_a(taken) as usize,
        end_pc: target + 1,
        taken_value: insn_b(taken) != 0,
    })
}
