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

/// Does `op` deliver its result to `R(A)`?
///
/// Only opcodes whose `A` field is genuinely a destination register belong
/// here. Every jump is deliberately excluded: a `JUMP`'s `A` field is unused
/// and reads back as `0`, so a span that merely *ends* in a jump would
/// otherwise be accepted as a writer of register 0 by pure coincidence.
/// `FASTCALL*` is excluded for the same reason — its `A` is a builtin id, not
/// a register.
fn writes_reg_a(op: LuauOpcode) -> bool {
    matches!(
        op,
        LuauOpcode::LoadNil
            | LuauOpcode::LoadB
            | LuauOpcode::LoadN
            | LuauOpcode::LoadK
            | LuauOpcode::LoadKX
            | LuauOpcode::Move
            | LuauOpcode::GetGlobal
            | LuauOpcode::GetUpval
            | LuauOpcode::GetImport
            | LuauOpcode::GetTable
            | LuauOpcode::GetTableKS
            | LuauOpcode::GetTableN
            | LuauOpcode::NewClosure
            | LuauOpcode::DupClosure
            | LuauOpcode::NewTable
            | LuauOpcode::DupTable
            | LuauOpcode::Add
            | LuauOpcode::Sub
            | LuauOpcode::Mul
            | LuauOpcode::Div
            | LuauOpcode::Mod
            | LuauOpcode::Pow
            | LuauOpcode::IDiv
            | LuauOpcode::AddK
            | LuauOpcode::SubK
            | LuauOpcode::MulK
            | LuauOpcode::DivK
            | LuauOpcode::ModK
            | LuauOpcode::PowK
            | LuauOpcode::IDivK
            | LuauOpcode::And
            | LuauOpcode::Or
            | LuauOpcode::AndK
            | LuauOpcode::OrK
            | LuauOpcode::Concat
            | LuauOpcode::Not
            | LuauOpcode::Minus
            | LuauOpcode::Length
            | LuauOpcode::SubRK
            | LuauOpcode::DivRK
            | LuauOpcode::NameCall
            | LuauOpcode::Call
            | LuauOpcode::Band
            | LuauOpcode::Bor
            | LuauOpcode::Bxor
            | LuauOpcode::Bnot
            | LuauOpcode::Shl
            | LuauOpcode::Shr
            | LuauOpcode::Bandk
            | LuauOpcode::Bork
            | LuauOpcode::RbxExt92
            | LuauOpcode::RbxExt93
            | LuauOpcode::RbxExt94
            | LuauOpcode::RbxExt95
            | LuauOpcode::RbxExt96
            | LuauOpcode::RbxExt97
            | LuauOpcode::RbxExt98
            | LuauOpcode::RbxExt99
            | LuauOpcode::RbxExt100
            | LuauOpcode::RbxExt101
            | LuauOpcode::RbxExt102
            | LuauOpcode::RbxExt103
            | LuauOpcode::RbxExt104
            | LuauOpcode::RbxExt105
            | LuauOpcode::GetVarargs
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
        //
        // Walk the segment instruction-aligned so an AUX word is never decoded
        // as an instruction, then require the final instruction to be a genuine
        // writer of `dest`. Checking only `insn_a` is not enough: the last word
        // of an ordinary `if/else` then-arm is the else-skipping `JUMP`, whose
        // unused `A` field reads as 0 — which matches `dest == 0` by pure
        // coincidence and swallows the else arm.
        let mut writer_pc: Option<usize> = None;
        let mut w = seg_start;
        while w < seg_end {
            writer_pc = Some(w);
            w += if LuauOpcode::from_u8(insn_op(code[w])).has_aux() {
                2
            } else {
                1
            };
        }
        let writer_pc = writer_pc?;
        let writer = code[writer_pc];
        if !writes_reg_a(LuauOpcode::from_u8(insn_op(writer)))
            || insn_a(writer) as usize != dest
        {
            return None;
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

/// Does any instruction *outside* `[start, end)` jump to a pc strictly inside
/// it?
///
/// `recognize_or_and_chain` decides, from a raw instruction window, that a span
/// is one value-producing expression. That decision is only safe if nothing
/// else branches into the middle of the span: the CFG suppresses block splits
/// for a recognised chain, and an external edge into a suppressed interior
/// silently disappears — taking the block it pointed at with it.
///
/// The span is walked instruction-aligned so AUX words are never decoded.
pub fn span_has_external_entry(code: &[u32], start: usize, end: usize) -> bool {
    let mut pc = 0usize;
    while pc < code.len() {
        let insn = code[pc];
        let op = LuauOpcode::from_u8(insn_op(insn));
        let target = match op {
            LuauOpcode::Jump | LuauOpcode::JumpBack => {
                Some((pc as i32 + insn_d(insn) as i32 + 1) as usize)
            }
            LuauOpcode::JumpX => Some((pc as i32 + insn_e(insn) + 1) as usize),
            _ if is_conditional_jump(op)
                || matches!(
                    op,
                    LuauOpcode::ForNPrep
                        | LuauOpcode::ForNLoop
                        | LuauOpcode::ForGPrep
                        | LuauOpcode::ForGLoop
                        | LuauOpcode::ForGPrepINext
                        | LuauOpcode::ForGPrepNext
                        | LuauOpcode::Deprecated61
                ) =>
            {
                Some((pc as i32 + insn_d(insn) as i32 + 1) as usize)
            }
            _ => None,
        };
        if let Some(t) = target {
            let inside_span = pc >= start && pc < end;
            if !inside_span && t > start && t < end {
                return true;
            }
        }
        pc += if op.has_aux() { 2 } else { 1 };
    }
    false
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

#[cfg(test)]
mod tests {
    use super::*;

    fn abc(op: LuauOpcode, a: u8, b: u8, c: u8) -> u32 {
        (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
    }
    fn ad(op: LuauOpcode, a: u8, d: i16) -> u32 {
        (op as u32) | ((a as u32) << 8) | (((d as u16) as u32) << 16)
    }

    /// The then-arm of an ordinary `if c then ... else ... end` ends in the
    /// else-skipping `JUMP`, whose unused `A` field reads back as 0. When the
    /// condition also lives in register 0 that used to match `dest` by
    /// coincidence, the CFG suppressed the split and the else arm became
    /// unreachable — silently deleting it from the output.
    #[test]
    fn if_else_then_arm_ending_in_jump_is_not_an_or_and_chain() {
        // 0: JUMPIFNOT R0 -> 6      (else arm at 6)
        // 1: GETIMPORT R1 (+aux)
        // 3: LOADK     R2
        // 4: CALL      R1
        // 5: JUMP      -> 7         (A field unused, reads as 0 == dest)
        // 6: <else arm>
        let code = vec![
            ad(LuauOpcode::JumpIfNot, 0, 5),
            ad(LuauOpcode::GetImport, 1, 0),
            0,
            abc(LuauOpcode::LoadK, 2, 0, 0),
            abc(LuauOpcode::Call, 1, 2, 1),
            ad(LuauOpcode::Jump, 0, 1),
            abc(LuauOpcode::LoadNil, 1, 0, 0),
            abc(LuauOpcode::LoadNil, 2, 0, 0),
        ];
        assert_eq!(recognize_or_and_chain(&code, 0), None);
    }

    /// A genuine `a or b or c` merged in one register must still be recognised:
    /// every operand ends in a real writer of the shared register.
    #[test]
    fn genuine_or_chain_with_call_operands_is_recognised() {
        // 0: JUMPIF R3 -> 8     4: JUMPIF R3 -> 8      (both target the join)
        let code = vec![
            ad(LuauOpcode::JumpIf, 3, 7),
            abc(LuauOpcode::Move, 3, 2, 0),
            abc(LuauOpcode::LoadK, 4, 0, 0),
            abc(LuauOpcode::Call, 3, 2, 1),
            ad(LuauOpcode::JumpIf, 3, 3),
            abc(LuauOpcode::Move, 3, 2, 0),
            abc(LuauOpcode::LoadK, 4, 0, 0),
            abc(LuauOpcode::Call, 3, 2, 1),
            abc(LuauOpcode::Return, 3, 2, 0),
        ];
        let chain = recognize_or_and_chain(&code, 0).expect("genuine or-chain");
        assert_eq!(chain.dest, 3);
        assert!(chain.is_or);
        assert_eq!(chain.segments.len(), 2);
    }

    /// The inner `or` of `t and t.n or -1`: a one-instruction operand that does
    /// write the shared register.
    #[test]
    fn single_load_operand_is_recognised() {
        // 0: JUMPIF R7 -> 2 ; 1: LOADN R7 -1 ; 2: CALL
        let code = vec![
            ad(LuauOpcode::JumpIf, 7, 1),
            abc(LuauOpcode::LoadN, 7, 0xFF, 0xFF),
            abc(LuauOpcode::Call, 6, 1, 1),
        ];
        let chain = recognize_or_and_chain(&code, 0).expect("inner or operand");
        assert_eq!(chain.dest, 7);
        assert_eq!(chain.end_pc, 2);
        assert_eq!(chain.segments, vec![(1, 2)]);
    }

    /// An operand whose last word is an AUX must be judged on the instruction
    /// that owns the AUX, not on the AUX word itself.
    #[test]
    fn aux_word_is_never_decoded_as_the_writer() {
        // 0: JUMPIF R7 -> 3 ; 1: GETTABLEKS R7 R5.k (aux at 2) ; 3: CALL
        let code = vec![
            ad(LuauOpcode::JumpIf, 7, 2),
            abc(LuauOpcode::GetTableKS, 7, 5, 0),
            17, // AUX: constant index, decodes as insn_a == 0
            abc(LuauOpcode::Call, 6, 1, 1),
        ];
        let chain = recognize_or_and_chain(&code, 0).expect("GETTABLEKS operand");
        assert_eq!(chain.dest, 7);
        assert_eq!(chain.segments, vec![(1, 3)]);

        // Same shape, but the AUX belongs to a write of a DIFFERENT register.
        // The old code stepped back onto the AUX word (insn_a == 0) and, for
        // dest 0, accepted it.
        let code0 = vec![
            ad(LuauOpcode::JumpIf, 0, 2),
            abc(LuauOpcode::GetTableKS, 7, 5, 0),
            17,
            abc(LuauOpcode::Call, 6, 1, 1),
        ];
        assert_eq!(recognize_or_and_chain(&code0, 0), None);
    }

    /// `FASTCALL`'s `A` is a builtin id, not a destination register.
    #[test]
    fn fastcall_a_field_is_not_a_register_write() {
        assert!(!writes_reg_a(LuauOpcode::FastCall));
        assert!(!writes_reg_a(LuauOpcode::FastCall1));
        assert!(!writes_reg_a(LuauOpcode::Jump));
        assert!(!writes_reg_a(LuauOpcode::JumpBack));
        assert!(!writes_reg_a(LuauOpcode::Nop));
        assert!(writes_reg_a(LuauOpcode::Call));
        assert!(writes_reg_a(LuauOpcode::Move));
    }
}
