//! Branch-merge register identity: arm writes must reach the join.
//!
//! A register written across if/else(if) arms and read after the merge lost
//! its value three ways at once: the arm writes were PARKED in the register
//! file (no statements emitted), the join collapsed the disagreeing parks —
//! `merge_regs` to `Unknown` at the region level, `*regs = regs_snapshot` on
//! the inline paths — and `is_empty_if` then deleted the statement-free arms.
//! Every post-merge read arrived as a permanently-nil hoisted `vN`. Census
//! exhibits: 95_TaskTypes GetStat (`LOADNIL R2` + three-way ORK diamond +
//! `SUB R2 ...` after the merge, whole diamond deleted), 180_NPC (three
//! `LOADK R18` arm writes deleted, `.Text = v18` emitted).
//!
//! The fix threads ONE identity through every path: the pre-branch parked
//! literal is materialized as a local BEFORE the register file is snapshotted
//! (`premateralize_branch_escapes`, now wired into the region-level
//! IfThenElse lift and both remaining inline diamond paths), and the register
//! is pinned across the arm ranges so `store_complex` emits each arm write as
//! a reassignment to that name instead of parking it.

use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, Proto};

const OP_LOADNIL: u8 = 2;
const OP_LOADN: u8 = 4;
const OP_MOVE: u8 = 6;
const OP_CALL: u8 = 21;
const OP_RETURN: u8 = 22;
const OP_JUMP: u8 = 23;
const OP_JUMPIFNOT: u8 = 26;

fn insn_ad(op: u8, a: u8, d: i16) -> u32 {
    let du = d as u16 as u32;
    (op as u32) | ((a as u32) << 8) | (du << 16)
}

fn insn_abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
}

fn chunk_of(code: Vec<u32>, num_params: u8, max_stack: u8) -> Chunk {
    let proto = Proto {
        max_stack_size: max_stack,
        num_params,
        num_upvalues: 0,
        is_vararg: false,
        flags: 0,
        typeinfo: None,
        code,
        constants: Vec::new(),
        child_protos: Vec::new(),
        line_defined: 1,
        debug_name: Some("main".to_string()),
        line_info: None,
        debug_info: None,
    };
    Chunk {
        version: 6,
        types_version: 0,
        strings: Vec::new(),
        protos: vec![proto],
        main_proto: 0,
    }
}

/// `local r = nil; if a then r = 7 elseif b then r = 8 else r = 9 end; b(r)`
/// hand-assembled as the three-way diamond the census measured:
///
/// ```text
/// pc0:  LOADNIL R2
/// pc1:  JUMPIFNOT R0, +2     -- → pc4 (elseif chain)
/// pc2:  LOADN R2, 7          -- then arm
/// pc3:  JUMP +4              -- → pc8 (merge)
/// pc4:  JUMPIFNOT R1, +2     -- → pc7 (else arm)
/// pc5:  LOADN R2, 8          -- elseif arm
/// pc6:  JUMP +1              -- → pc8 (merge)
/// pc7:  LOADN R2, 9          -- else arm
/// pc8:  MOVE R3, R1          -- merge: consume the diamond register
/// pc9:  MOVE R4, R2
/// pc10: CALL R3, 2, 1
/// pc11: RETURN R0, 1
/// ```
fn build_three_way_diamond_chunk() -> Chunk {
    let code = vec![
        insn_abc(OP_LOADNIL, 2, 0, 0),
        insn_ad(OP_JUMPIFNOT, 0, 2),
        insn_ad(OP_LOADN, 2, 7),
        insn_ad(OP_JUMP, 0, 4),
        insn_ad(OP_JUMPIFNOT, 1, 2),
        insn_ad(OP_LOADN, 2, 8),
        insn_ad(OP_JUMP, 0, 1),
        insn_ad(OP_LOADN, 2, 9),
        insn_abc(OP_MOVE, 3, 1, 0),
        insn_abc(OP_MOVE, 4, 2, 0),
        insn_abc(OP_CALL, 3, 2, 1),
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    chunk_of(code, 2, 8)
}

#[test]
fn three_way_diamond_arm_writes_survive_to_the_join() {
    let chunk = build_three_way_diamond_chunk();
    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);
    eprintln!("=== emitted source ===\n{}\n======================", out);

    // The defect deleted the diamond outright: none of the three arm values
    // appeared anywhere in the output. Whatever surface form the lifter
    // chooses (if/else assignments or a reconstructed selection expression),
    // all three values must survive.
    for v in ["7", "8", "9"] {
        assert!(
            out.contains(v),
            "arm value {v} was deleted at the branch merge:\n{out}"
        );
    }
}

#[test]
fn diamond_register_read_after_merge_is_a_bound_name() {
    let chunk = build_three_way_diamond_chunk();
    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    // The post-merge consumer must not read an undeclared register name.
    // Every `vN` the output reads must also be written somewhere: with the
    // identity threaded through the arms this is the premateralized local
    // (`local v2 = nil` + `v2 = 7/8/9`); a reconstructed expression that
    // inlines the values is equally acceptable. What is NOT acceptable is the
    // pre-fix shape — a `vN` that appears only as a read.
    for cap in regex_lite_reads(&out) {
        assert!(
            out.contains(&format!("{cap} =")) || out.contains(&format!("local {cap}")),
            "post-merge read of `{cap}` has no binding:\n{out}"
        );
    }
}

/// `local r = nil; if a then r = b(1) else r = 9 end; b(r)` — the impure-arm
/// diamond. A CALL in the then-arm defeats the value-join reconstruction (it
/// only claims pure arms), so this MUST come out as a statement-form if/else,
/// which is exactly the path where the arm writes used to be parked and then
/// annihilated at the join:
///
/// ```text
/// pc0:  LOADNIL R2
/// pc1:  JUMPIFNOT R0, +5     -- → pc7 (else)
/// pc2:  MOVE R3, R1          -- then: r = b(1)
/// pc3:  LOADN R4, 1
/// pc4:  CALL R3, 2, 2        -- one result → R3
/// pc5:  MOVE R2, R3
/// pc6:  JUMP +1              -- → pc8 (merge)
/// pc7:  LOADN R2, 9          -- else: r = 9
/// pc8:  MOVE R3, R1          -- merge: consume r
/// pc9:  MOVE R4, R2
/// pc10: CALL R3, 2, 1
/// pc11: RETURN R0, 1
/// ```
fn build_impure_arm_diamond_chunk() -> Chunk {
    let code = vec![
        insn_abc(OP_LOADNIL, 2, 0, 0),
        insn_ad(OP_JUMPIFNOT, 0, 5),
        insn_abc(OP_MOVE, 3, 1, 0),
        insn_ad(OP_LOADN, 4, 1),
        insn_abc(OP_CALL, 3, 2, 2),
        insn_abc(OP_MOVE, 2, 3, 0),
        insn_ad(OP_JUMP, 0, 1),
        insn_ad(OP_LOADN, 2, 9),
        insn_abc(OP_MOVE, 3, 1, 0),
        insn_abc(OP_MOVE, 4, 2, 0),
        insn_abc(OP_CALL, 3, 2, 1),
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    chunk_of(code, 2, 8)
}

#[test]
fn impure_arm_diamond_keeps_one_identity_across_the_merge() {
    let chunk = build_impure_arm_diamond_chunk();
    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);
    eprintln!("=== emitted source ===\n{}\n======================", out);

    // The CALL makes the then-arm a statement, so this cannot be folded to a
    // selection expression — the if/else must survive with BOTH arm writes.
    assert!(out.contains("if "), "if/else structure disappeared:\n{out}");
    assert!(
        out.contains("9"),
        "else-arm value 9 was deleted at the branch merge:\n{out}"
    );
    // Every generated register name that is read must be bound somewhere.
    for cap in regex_lite_reads(&out) {
        assert!(
            out.contains(&format!("{cap} =")) || out.contains(&format!("local {cap}")),
            "post-merge read of `{cap}` has no binding:\n{out}"
        );
    }
}

/// Tiny scan for generated `vN` identifiers in the output (no regex dep).
fn regex_lite_reads(out: &str) -> Vec<String> {
    let mut found = std::collections::BTreeSet::new();
    let bytes = out.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'v' && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric() && bytes[i - 1] != b'_') {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 1 && (j == bytes.len() || (!bytes[j].is_ascii_alphanumeric() && bytes[j] != b'_')) {
                found.insert(out[i..j].to_string());
            }
            i = j;
        } else {
            i += 1;
        }
    }
    found.into_iter().collect()
}
