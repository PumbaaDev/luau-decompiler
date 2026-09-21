//! Arm-interior for-loop recovery.
//!
//! `structure_control_flow` marks if/else arm blocks `handled` and stores them
//! as plain block-id lists, so a FORGPREP inside an arm is never seen by
//! `try_match_for_loop` and the linear dispatcher's for-opcode arm is empty:
//! the loop was emitted as NOTHING (iterator call flattened to a bare local,
//! body unguarded, loop variables permanently nil). Corpus-wide, 607 of 1094
//! FORGPREPs sat in if/else arms and were dropped this way.
//!
//! The fix structures each maximal CONTIGUOUS run of arm blocks with
//! `structure_numeric_for_body` (the same scan whose loops already survive in
//! for-loop bodies) — but ONLY contiguous runs, because a gap in an arm's
//! block list is exactly where another region's blocks live. The previous
//! attempt structured `min(start)..max(end)` of the whole arm, double-lifted
//! foreign blocks in disjoint arms, and broke 23 compile-gate files.

use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, Proto};

use super::super::split_contiguous_runs;

const OP_LOADNIL: u8 = 2;
const OP_MOVE: u8 = 6;
const OP_CALL: u8 = 21;
const OP_RETURN: u8 = 22;
const OP_JUMPIFNOT: u8 = 26;
const OP_FORGPREP: u8 = 58;
const OP_FORGLOOP: u8 = 59;

fn insn_ad(op: u8, a: u8, d: i16) -> u32 {
    let du = d as u16 as u32;
    (op as u32) | ((a as u32) << 8) | (du << 16)
}

fn insn_abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
}

// ── split_contiguous_runs ───────────────────────────────────────────────────

#[test]
fn splits_adjacent_ranges_into_one_run() {
    let ranges = vec![(1, 5), (5, 10), (10, 12)];
    assert_eq!(split_contiguous_runs(&ranges), vec![(0, 2)]);
}

#[test]
fn splits_at_every_gap() {
    // A gap is another region's territory; a run must never span one.
    let ranges = vec![(1, 5), (7, 10), (10, 12), (20, 22)];
    assert_eq!(split_contiguous_runs(&ranges), vec![(0, 0), (1, 2), (3, 3)]);
}

#[test]
fn runs_partition_the_input_exactly() {
    let ranges = vec![(0, 3), (3, 4), (9, 11), (11, 12), (12, 13), (30, 31)];
    let runs = split_contiguous_runs(&ranges);
    // Every index in exactly one run, in order, no overlap.
    let mut covered = Vec::new();
    for &(lo, hi) in &runs {
        assert!(lo <= hi && hi < ranges.len());
        for i in lo..=hi {
            covered.push(i);
        }
    }
    assert_eq!(covered, (0..ranges.len()).collect::<Vec<_>>());
    // And each run really is contiguous while each boundary really is a gap.
    for &(lo, hi) in &runs {
        for i in lo..hi {
            assert_eq!(ranges[i].1, ranges[i + 1].0);
        }
    }
    for w in runs.windows(2) {
        let prev_end = ranges[w[0].1].1;
        let next_start = ranges[w[1].0].0;
        assert_ne!(prev_end, next_start, "adjacent runs should have merged");
    }
}

#[test]
fn empty_input_yields_no_runs() {
    assert!(split_contiguous_runs(&[]).is_empty());
}

// ── behavioral: a generic-for inside an if-arm must survive ────────────────

/// `if cond then for k, v in t do t(k) end end` hand-assembled:
///
/// ```text
/// pc0:  JUMPIFNOT R0, +9      -- skip arm → pc10
/// pc1:  MOVE R2, R1           -- iterator function slot = t
/// pc2:  LOADNIL R3
/// pc3:  LOADNIL R4
/// pc4:  FORGPREP R2, +3       -- loop_pc = 4+3+1 = 8
/// pc5:  MOVE R6, R1           --   body: t(k)
/// pc6:  MOVE R7, R5
/// pc7:  CALL R6, 2, 1
/// pc8:  FORGLOOP R2, -4       -- back to pc5
/// pc9:  AUX (2 loop vars)
/// pc10: RETURN R0, 1
/// ```
fn build_if_arm_generic_for_chunk() -> Chunk {
    let code = vec![
        insn_ad(OP_JUMPIFNOT, 0, 9),
        insn_abc(OP_MOVE, 2, 1, 0),
        insn_abc(OP_LOADNIL, 3, 0, 0),
        insn_abc(OP_LOADNIL, 4, 0, 0),
        insn_ad(OP_FORGPREP, 2, 3),
        insn_abc(OP_MOVE, 6, 1, 0),
        insn_abc(OP_MOVE, 7, 5, 0),
        insn_abc(OP_CALL, 6, 2, 1),
        insn_ad(OP_FORGLOOP, 2, -4),
        2u32, // AUX: two loop variables
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    let parent = Proto {
        max_stack_size: 10,
        num_params: 2,
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
        protos: vec![parent],
        main_proto: 0,
    }
}

#[test]
fn generic_for_inside_if_arm_is_emitted_as_a_loop() {
    let chunk = build_if_arm_generic_for_chunk();
    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);
    eprintln!("=== emitted source ===\n{}\n======================", out);

    // The defect emitted the loop as nothing: no `for ... in` header at all.
    assert!(
        out.contains("for ") && out.contains(" in "),
        "generic-for inside the if-arm was dropped:\n{out}"
    );
    // The wrapping if must also survive.
    assert!(out.contains("if "), "wrapping if disappeared:\n{out}");
    // The arm is NOT a loop context: recovering the for must not translate
    // the arm's exit jump into `break`/`continue` (a compile error here).
    assert!(
        !out.contains("break") && !out.contains("continue"),
        "loop-context statement leaked into a non-loop arm:\n{out}"
    );
    // The for header must come AFTER the if header (loop nested in the arm).
    let if_pos = out.find("if ").unwrap();
    let for_pos = out.find("for ").unwrap();
    assert!(
        for_pos > if_pos,
        "for-loop was not nested under the if:\n{out}"
    );
}
