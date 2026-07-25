//! Detection of **value joins** — acyclic, side-effect-free branch regions
//! whose only observable result is the value some register holds at the point
//! where control flow reconverges.
//!
//! Luau compiles `and`/`or`/`a and b or c` — and, more generally, any
//! conditional that merely *selects a value* — into branches. A plain
//! `if/then/else` region cannot always express the result, because the arms of
//! a short-circuit ladder overlap:
//!
//! ```text
//!     JUMPIFNOT R5 -> L1        ; t falsy      -> -1
//!     GETTABLEKS R7 R5."n"
//!     JUMPIF     R7 -> L2       ; t.n truthy   -> t.n
//!  L1: LOADN     R7 -1          ; t.n falsy    -> -1
//!  L2: <join; R7 is the result>
//! ```
//!
//! The `LOADN` block is simultaneously the outer branch's else-arm *and* the
//! inner one's second operand — it has two predecessors, so it belongs to both
//! arms at once. `Region::IfThenElse` requires disjoint arm lists and therefore
//! cannot represent it; `find_merge_point` compounds the problem by reporting
//! that block as the merge, which hides the real join entirely.
//!
//! A value join sidesteps the shape question. It records the branching entry,
//! the pure blocks between it and the reconvergence point, and the join itself.
//! The lifter then enumerates the paths, pairs each with its path condition,
//! and reconstructs the value each register carries at the join. Because the
//! member blocks are required to be side-effect-free, a block reachable along
//! several paths may be evaluated once per path without changing behaviour.

use std::collections::BTreeSet;

use super::cfg::ControlFlowGraph;
use crate::parser::opcodes::LuauOpcode;
use crate::parser::types::*;

/// Upper bounds. A value join is a small, local idiom; anything larger is real
/// control flow that merely happens to look acyclic and pure.
const MAX_ARM_BLOCKS: usize = 8;
const MAX_ARM_WORDS: usize = 24;

/// A recognised value join.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueJoin {
    /// Block that branches into the region.
    pub entry: usize,
    /// Blocks strictly between `entry` and `join`, in ascending order.
    pub arms: Vec<usize>,
    /// Block where every path reconverges. Deliberately **not** part of the
    /// region: it keeps its own region so it is lifted exactly once.
    pub join: usize,
}

/// May `op` appear inside a value join?
///
/// This is an allowlist, not a blocklist: an opcode has to be positively known
/// to compute a value without observable effects. Everything absent — `CALL`,
/// `NAMECALL`, every `SET*`, closure creation, upvalue capture, table
/// allocation, `RETURN`, and all loop opcodes — rejects the region, so a
/// side-effecting operand can never be silently evaluated twice, nor a loop
/// body folded into an expression.
fn is_pure_value_op(op: LuauOpcode) -> bool {
    matches!(
        op,
        LuauOpcode::Nop
            | LuauOpcode::LoadNil
            | LuauOpcode::LoadB
            | LuauOpcode::LoadN
            | LuauOpcode::LoadK
            | LuauOpcode::LoadKX
            | LuauOpcode::Move
            | LuauOpcode::GetUpval
            | LuauOpcode::GetGlobal
            | LuauOpcode::GetImport
            | LuauOpcode::GetTable
            | LuauOpcode::GetTableKS
            | LuauOpcode::GetTableN
            | LuauOpcode::Not
            | LuauOpcode::Minus
            | LuauOpcode::Length
            | LuauOpcode::Jump
            | LuauOpcode::JumpX
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

/// Are all instructions in `[start, end)` pure value computations?
///
/// Walks instruction-aligned so an AUX word is never decoded as an opcode.
///
/// Table construction is admitted under one extra condition: a `SET*` may only
/// target a table this same block allocated. `local p = c and { q = 1 } or nil`
/// compiles to `DUPTABLE` followed by `SETTABLEKS`, and the writes are part of
/// building the literal — nothing outside the region can observe them. Writing
/// through a register that came from *outside* is an ordinary side effect and
/// still rejects the block.
fn range_is_pure(code: &[u32], start: usize, end: usize) -> bool {
    let mut fresh_tables: BTreeSet<usize> = BTreeSet::new();
    let mut pc = start;
    while pc < end {
        if pc >= code.len() {
            return false;
        }
        let insn = code[pc];
        let op = LuauOpcode::from_u8(insn_op(insn));
        let a = insn_a(insn) as usize;
        match op {
            LuauOpcode::NewTable | LuauOpcode::DupTable => {
                fresh_tables.insert(a);
            }
            // `SETTABLE* A B ..` is `R(B)[key] = R(A)` — the TABLE is B.
            // `SETLIST A B ..` is the other way round: the table is A.
            LuauOpcode::SetTable | LuauOpcode::SetTableKS | LuauOpcode::SetTableN => {
                if !fresh_tables.contains(&(insn_b(insn) as usize)) {
                    return false;
                }
            }
            LuauOpcode::SetList => {
                if !fresh_tables.contains(&a) {
                    return false;
                }
            }
            _ => {
                if !is_pure_value_op(op) {
                    return false;
                }
                // Overwriting a register drops its "freshly allocated" status.
                fresh_tables.remove(&a);
            }
        }
        pc += if op.has_aux() { 2 } else { 1 };
    }
    true
}

/// Try to recognise a value join whose branching entry is `entry`.
///
/// Grows a set of member blocks from `entry`, always absorbing the lowest
/// pending exit, until exactly one exit remains — that exit is the join. Every
/// absorbed block must be reached *only* from inside the region, must lie
/// forward of the entry, and must be pure.
///
/// `loop_headers` guards the one thing the forward-only walk cannot see for
/// itself: claiming a loop header as a region member would stop the loop from
/// being recognised at all. Loop *bodies* need no such guard — every back edge
/// is rejected structurally (a successor at or before the entry, or one already
/// in the region), and `JUMPBACK`/`FOR*LOOP` are absent from the purity
/// allowlist. A value join is therefore free to occur inside a loop body, which
/// is where the null-safe accessor idiom most often appears.
pub fn find_value_join(
    cfg: &ControlFlowGraph,
    code: &[u32],
    entry: usize,
    loop_headers: &BTreeSet<usize>,
) -> Option<ValueJoin> {
    let entry_block = cfg.blocks.get(&entry)?;
    if entry_block.successors.len() != 2 {
        return None;
    }
    if loop_headers.contains(&entry) {
        return None;
    }

    let mut member: BTreeSet<usize> = BTreeSet::new();
    member.insert(entry);
    let mut exits: BTreeSet<usize> = BTreeSet::new();
    let mut words = 0usize;

    for &s in &entry_block.successors {
        // A successor at or before the entry is a back edge: not a value join.
        if s <= entry {
            return None;
        }
        exits.insert(s);
    }
    if exits.len() != 2 {
        // Both arms landing on the same block means the branch selects nothing.
        return None;
    }

    while exits.len() > 1 {
        // Absorbing the lowest pending exit keeps the walk in program order, so
        // a block is only ever absorbed after all of its in-region predecessors.
        let b = *exits.iter().next()?;
        exits.remove(&b);

        if b <= entry || loop_headers.contains(&b) {
            return None;
        }
        if member.len() > MAX_ARM_BLOCKS {
            return None;
        }
        let block = cfg.blocks.get(&b)?;
        // Every path into an arm must come from within the region, otherwise
        // code outside it would jump into the middle of a reconstructed
        // expression.
        if !block.predecessors.iter().all(|p| member.contains(p)) {
            return None;
        }
        if !range_is_pure(code, block.start, block.end) {
            return None;
        }
        words += block.end.saturating_sub(block.start);
        if words > MAX_ARM_WORDS {
            return None;
        }
        // A block with no successors (a `RETURN`) can never reconverge.
        if block.successors.is_empty() {
            return None;
        }

        member.insert(b);
        for &s in &block.successors {
            if s <= entry || member.contains(&s) {
                // Backward edge inside the region — this is a loop, not a join.
                return None;
            }
            exits.insert(s);
        }
    }

    let join = *exits.iter().next()?;
    if join <= entry || member.len() < 2 {
        return None;
    }
    // The members must tile one contiguous pc range that ends exactly where the
    // join begins. Anything else would leave a gap holding a block that is not
    // part of the region but sits inside its span — the lifter's fallback path
    // lifts that span wholesale, and would then emit the stray block twice.
    let mut cursor = cfg.blocks.get(&entry)?.start;
    for &b in &member {
        let blk = cfg.blocks.get(&b)?;
        if blk.start != cursor {
            return None;
        }
        cursor = blk.end;
    }
    if cursor != cfg.blocks.get(&join)?.start {
        return None;
    }

    let arms: Vec<usize> = member.iter().copied().filter(|&b| b != entry).collect();
    if arms.is_empty() {
        return None;
    }
    Some(ValueJoin { entry, arms, join })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::types::Proto;

    fn abc(op: LuauOpcode, a: u8, b: u8, c: u8) -> u32 {
        (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
    }
    fn ad(op: LuauOpcode, a: u8, d: i16) -> u32 {
        (op as u32) | ((a as u32) << 8) | (((d as u16) as u32) << 16)
    }

    fn proto_with(code: Vec<u32>) -> Proto {
        Proto {
            max_stack_size: 16,
            num_params: 0,
            num_upvalues: 0,
            is_vararg: false,
            flags: 0,
            typeinfo: None,
            code,
            constants: Vec::new(),
            child_protos: Vec::new(),
            line_defined: 0,
            debug_name: None,
            line_info: None,
            debug_info: None,
        }
    }

    /// The short-circuit ladder `t and t.n or -1`: the `-1` block has two
    /// predecessors and belongs to both arms, so the region must be found as a
    /// three-block value join, not as an if/else.
    #[test]
    fn ladder_with_shared_arm_is_a_value_join() {
        // 0: JUMPIFNOT R5 -> 4      2: JUMPIF R7 -> 5
        // 1: GETTABLEKS R7 (aux)    4: LOADN R7 -1     5: CALL (join)
        let code = vec![
            ad(LuauOpcode::JumpIfNot, 5, 3),
            abc(LuauOpcode::GetTableKS, 7, 5, 0),
            17,
            ad(LuauOpcode::JumpIf, 7, 1),
            abc(LuauOpcode::LoadN, 7, 0xFF, 0xFF),
            abc(LuauOpcode::Call, 6, 1, 1),
        ];
        let proto = proto_with(code.clone());
        let cfg = ControlFlowGraph::build(&proto);
        let vj = find_value_join(&cfg, &code, 0, &BTreeSet::new()).expect("ladder");
        assert_eq!(vj.entry, 0);
        assert_eq!(vj.arms, vec![1, 4]);
        assert_eq!(vj.join, 5);
    }

    /// A plain value diamond is also a value join.
    #[test]
    fn pure_diamond_is_a_value_join() {
        // 0: JUMPIFNOT R8 -> 3 ; 1: LOADK R7 ; 2: JUMP -> 4 ; 3: LOADK R7 ; 4: CALL
        let code = vec![
            ad(LuauOpcode::JumpIfNot, 8, 2),
            abc(LuauOpcode::LoadK, 7, 0, 0),
            ad(LuauOpcode::Jump, 0, 1),
            abc(LuauOpcode::LoadK, 7, 1, 0),
            abc(LuauOpcode::Call, 6, 1, 1),
        ];
        let proto = proto_with(code.clone());
        let cfg = ControlFlowGraph::build(&proto);
        let vj = find_value_join(&cfg, &code, 0, &BTreeSet::new()).expect("diamond");
        assert_eq!(vj.arms, vec![1, 3]);
        assert_eq!(vj.join, 4);
    }

    /// A call inside an arm rejects the region: the block would otherwise be
    /// evaluated once per path, duplicating the side effect.
    #[test]
    fn side_effecting_arm_is_rejected() {
        let code = vec![
            ad(LuauOpcode::JumpIfNot, 8, 3),
            abc(LuauOpcode::Move, 7, 2, 0),
            abc(LuauOpcode::Call, 7, 1, 2),
            ad(LuauOpcode::Jump, 0, 1),
            abc(LuauOpcode::LoadK, 7, 1, 0),
            abc(LuauOpcode::Call, 6, 1, 1),
        ];
        let proto = proto_with(code.clone());
        let cfg = ControlFlowGraph::build(&proto);
        assert_eq!(find_value_join(&cfg, &code, 0, &BTreeSet::new()), None);
    }

    /// A loop header is never absorbed: claiming it would stop the loop from
    /// being recognised.
    #[test]
    fn loop_member_is_rejected() {
        let code = vec![
            ad(LuauOpcode::JumpIfNot, 8, 2),
            abc(LuauOpcode::LoadK, 7, 0, 0),
            ad(LuauOpcode::Jump, 0, 1),
            abc(LuauOpcode::LoadK, 7, 1, 0),
            abc(LuauOpcode::Call, 6, 1, 1),
        ];
        let proto = proto_with(code.clone());
        let cfg = ControlFlowGraph::build(&proto);
        let mut loop_blocks = BTreeSet::new();
        loop_blocks.insert(3usize);
        assert_eq!(find_value_join(&cfg, &code, 0, &loop_blocks), None);
    }

    /// Building a table literal inside an arm is fine; writing through a table
    /// that came from outside the region is a side effect and rejects it.
    #[test]
    fn table_writes_are_only_pure_for_freshly_built_tables() {
        // Fresh: DUPTABLE R7, then SETTABLEKS value=R8 into table=R7.
        assert!(range_is_pure(
            &[
                abc(LuauOpcode::DupTable, 7, 0, 0),
                abc(LuauOpcode::LoadN, 8, 5, 0),
                abc(LuauOpcode::SetTableKS, 8, 7, 0),
                0,
            ],
            0,
            4
        ));
        // External: the table register was never allocated in this block.
        assert!(!range_is_pure(
            &[
                abc(LuauOpcode::LoadN, 8, 5, 0),
                abc(LuauOpcode::SetTableKS, 8, 3, 0),
                0,
            ],
            0,
            3
        ));
        // The table register is reused for something else before the write.
        assert!(!range_is_pure(
            &[
                abc(LuauOpcode::DupTable, 7, 0, 0),
                abc(LuauOpcode::Move, 7, 2, 0),
                abc(LuauOpcode::SetTableKS, 8, 7, 0),
                0,
            ],
            0,
            4
        ));
    }

    /// A block entered from outside the region rejects it: an external jump
    /// into the middle of a reconstructed expression has nowhere to land.
    #[test]
    fn externally_entered_arm_is_rejected() {
        // 5 jumps into the middle of the would-be region at 3.
        let code = vec![
            ad(LuauOpcode::JumpIfNot, 8, 2),
            abc(LuauOpcode::LoadK, 7, 0, 0),
            ad(LuauOpcode::Jump, 0, 1),
            abc(LuauOpcode::LoadK, 7, 1, 0),
            abc(LuauOpcode::Call, 6, 1, 1),
            ad(LuauOpcode::Jump, 0, -3),
        ];
        let proto = proto_with(code.clone());
        let cfg = ControlFlowGraph::build(&proto);
        assert_eq!(find_value_join(&cfg, &code, 0, &BTreeSet::new()), None);
    }
}
