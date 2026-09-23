//! A pending table whose last use is already behind the branch must not be
//! materialised by the `Region::IfThenElse` "pending tables" block.
//!
//! That block names every `Expr::Table` still parked in the register file before
//! an if/else runs, so a table an arm mutates survives `merge_regs`. The register
//! file is never invalidated when a value is consumed, though, so it also held
//! tables that were already DEAD: the literal a `for _, v in {a, b, c} do`
//! iterated (MOVEd into the loop base, never read again) and the copy sitting in
//! the loop base itself. Each was re-emitted inside the loop body as a
//! byte-for-byte duplicate — one live module's `start_server` printed
//! `local tbl2 = {workspace, ReplicatedStorage, ServerStorage,
//! ServerScriptService}` plus a second copy under a hint name, where the
//! bytecode has ONE `NEWTABLE`+`SETLIST` before the loop and no reader after
//! the `FORGPREP` (semantic checker: `discarded_table_write`).
//!
//! The fix gates the block on `reg_live_at`, a one-sided bytecode liveness
//! query that answers "dead" only when every path redefines the register
//! before any possible read. These tests pin the query's answers on the exact
//! shapes (dead literal, dead loop base, live forward read, live read around
//! the loop back-edge, unmodelled opcode) and the end-to-end output.

use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, Proto};

// Canonical Luau opcode bytes (identity opmap, Chunk.version = 6).
const OP_NOP: u32 = 0;
const OP_LOADNIL: u8 = 2;
const OP_LOADK: u8 = 5;
const OP_MOVE: u8 = 6;
const OP_GETTABLEN: u8 = 17;
const OP_NAMECALL: u8 = 20;
const OP_CALL: u8 = 21;
const OP_RETURN: u8 = 22;
const OP_JUMPIFNOT: u8 = 26;
const OP_NEWTABLE: u8 = 53;
const OP_SETLIST: u8 = 55;
const OP_FORGPREP: u8 = 58;
const OP_FORGLOOP: u8 = 59;
const OP_RBXEXT92: u8 = 92;

fn insn_ad(op: u8, a: u8, d: i16) -> u32 {
    let du = d as u16 as u32;
    (op as u32) | ((a as u32) << 8) | (du << 16)
}

fn insn_abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
}

/// `for _, v in {…} do <body> end` over a table built in R2.
///
/// ```text
///   0: NEWTABLE  R2 (+AUX)
///   2: MOVE      R5 R2          -- the literal's ONLY read
///   3: LOADNIL   R6
///   4: LOADNIL   R7
///   5: FORGPREP  R5 -> 9
///   6: <body6>
///   7: JUMPIFNOT R10 -> 8       -- the branch the query is asked at
///   8: <body8>
///   9: FORGLOOP  R5 -> 6 (+AUX nvars=2)
///  11: RETURN    R0 1
/// ```
fn loop_code(body6: u32, body8: u32) -> Vec<u32> {
    vec![
        insn_abc(OP_NEWTABLE, 2, 0, 0),
        0,
        insn_abc(OP_MOVE, 5, 2, 0),
        insn_abc(OP_LOADNIL, 6, 0, 0),
        insn_abc(OP_LOADNIL, 7, 0, 0),
        insn_ad(OP_FORGPREP, 5, 3),
        body6,
        insn_ad(OP_JUMPIFNOT, 10, 0),
        body8,
        insn_ad(OP_FORGLOOP, 5, -4),
        2,
        insn_abc(OP_RETURN, 0, 1, 0),
    ]
}

#[test]
fn consumed_loop_literal_and_loop_base_are_dead_at_body_branch() {
    let live = super::super::reg_live_at;
    let code = loop_code(insn_ad(OP_LOADK, 10, 0), OP_NOP);
    // The literal's only reader is the MOVE into the loop base.
    assert!(live(&code, 2, 2), "R2 is read by the MOVE at pc 2");
    assert!(!live(&code, 7, 2), "R2 is dead inside the loop body");
    // The loop base is hidden iteration state: FORGLOOP reads it, no source
    // expression can.
    assert!(!live(&code, 7, 5), "the FORGLOOP base is not a source-visible read");
    // FORGPREP itself IS a read of the base (the `in` expression).
    assert!(live(&code, 5, 5));
}

#[test]
fn a_later_read_in_the_body_keeps_the_table_live() {
    let live = super::super::reg_live_at;
    let code = loop_code(insn_ad(OP_LOADK, 10, 0), insn_abc(OP_GETTABLEN, 11, 2, 0));
    assert!(live(&code, 7, 2), "R2 is read at pc 8 on the fall-through path");
}

#[test]
fn a_read_around_the_back_edge_keeps_the_table_live() {
    let live = super::super::reg_live_at;
    // pc 6 reads R2 (into R10, the branch condition); reachable from pc 7 only
    // through FORGLOOP's back-edge.
    let code = loop_code(insn_abc(OP_GETTABLEN, 10, 2, 0), OP_NOP);
    assert!(live(&code, 7, 2), "R2 is read at pc 6 on the next iteration");
}

#[test]
fn an_unmodelled_opcode_answers_live() {
    let live = super::super::reg_live_at;
    let code = loop_code(insn_ad(OP_LOADK, 10, 0), insn_abc(OP_RBXEXT92, 12, 13, 0));
    assert!(live(&code, 7, 2), "an opcode with unknown semantics must never prove death");
}

#[test]
fn a_call_argument_window_is_a_read_and_fixed_results_are_a_kill() {
    let live = super::super::reg_live_at;
    // 0: CALL R1 B=2 C=2  (one arg: reads R1 fn, R2 arg; one result: writes R1)
    // 1: CALL R2 B=1 C=2  (no args: reads R2 fn; one result: writes R2)
    // 2: RETURN R0 1
    let code = vec![
        insn_abc(OP_CALL, 1, 2, 2),
        insn_abc(OP_CALL, 2, 1, 2),
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    assert!(live(&code, 0, 2), "R2 is in the argument window");
    assert!(live(&code, 1, 2), "R2 is the called function");
    // R3 is never mentioned: dead.
    assert!(!live(&code, 0, 3));
    // 0: CALL R3 B=1 C=3 writes R3 and R4 before RETURN reads them: R4 is dead
    // on entry, R3 (the callee) is live.
    let code = vec![insn_abc(OP_CALL, 3, 1, 3), insn_abc(OP_RETURN, 3, 3, 0)];
    assert!(!live(&code, 0, 4), "a fixed-count CALL result redefines R4");
    assert!(live(&code, 0, 3));
    // RETURN R0 with B=0 reads everything from R0 up.
    let code = vec![insn_abc(OP_RETURN, 0, 0, 0)];
    assert!(live(&code, 0, 7));
}

/// A table written to AFTER its consumer took the snapshot is not a duplicate:
/// the later field write lives only in the parked register.
///
/// ```text
///   0: LOADNIL    R1
///   1: NEWTABLE   R7 (+AUX)
///   3: SETTABLEKS R7 -> R0."SystemChat" (+AUX)   -- consumer snapshots `{}`
///   5: DUPCLOSURE R8 K0
///   6: SETTABLEKS R8 -> R7."send" (+AUX)         -- write after the snapshot
///   8: RETURN     R0 1
/// ```
#[test]
fn a_write_after_consumption_is_not_a_pure_duplicate() {
    let pure = super::super::pending_table_is_pure_duplicate;
    const OP_SETTABLEKS: u8 = 16;
    const OP_DUPCLOSURE: u8 = 82;
    let code = vec![
        insn_abc(OP_LOADNIL, 1, 0, 0),
        insn_abc(OP_NEWTABLE, 7, 0, 0),
        0,
        insn_abc(OP_SETTABLEKS, 7, 0, 0),
        0,
        insn_ad(OP_DUPCLOSURE, 8, 0),
        insn_abc(OP_SETTABLEKS, 8, 7, 0),
        0,
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    assert!(!pure(&code, 8, 7), "the `send` field exists only in the parked table");
    // Before the late write the same table IS a pure duplicate.
    assert!(pure(&code, 5, 7));
}

#[test]
fn a_filled_then_iterated_literal_and_its_loop_base_copy_are_pure_duplicates() {
    let pure = super::super::pending_table_is_pure_duplicate;
    let chunk = loop_over_literal_chunk();
    let code = &chunk.protos[0].code;
    // Branch at pc 12 (JUMPIFNOT R9): R2 is the literal, R5 the MOVE copy.
    assert!(pure(code, 12, 2));
    assert!(pure(code, 12, 5));
    // R6 was never a table constructor.
    assert!(!pure(code, 12, 6));
}

#[test]
fn arm_touch_scan_sees_register_windows() {
    let touched = super::super::reg_touched_in_ranges;
    // 0: NAMECALL R4 R2 (+AUX)   -- writes R4 and its self slot R5
    // 2: CALL R4 B=3 C=3         -- args R5,R6; results R4,R5
    let code = vec![insn_abc(OP_NAMECALL, 4, 2, 0), 0, insn_abc(OP_CALL, 4, 3, 3)];
    assert!(touched(&code, &[(0, 1)], 5), "NAMECALL writes the self slot A+1");
    assert!(touched(&code, &[(2, 3)], 6), "an argument inside the CALL window");
    assert!(!touched(&code, &[(0, 3)], 1), "R1 is never touched");
    assert!(!touched(&code, &[(2, 3)], 2), "below the CALL base");
}

/// End to end, the live module's shape: `for _, v in {a, b} do if v then for
/// _, w in a do w:Touch() end end end`. The loop nested in the then-arm makes
/// the structurer build a `Region::IfThenElse` (an arm without one lifts as an
/// inline if and never reaches the pending-table block).
///
/// ```text
///   0: LOADNIL   R3
///   1: LOADNIL   R4
///   2: NEWTABLE  R2 (+AUX)
///   4: MOVE      R3 R0
///   5: MOVE      R4 R1
///   6: SETLIST   R2 R3 C=3 (+AUX 1)
///   8: MOVE      R5 R2          -- the literal's ONLY read
///   9: LOADNIL   R6
///  10: LOADNIL   R7
///  11: FORGPREP  R5 -> 22
///  12: JUMPIFNOT R9 -> 22
///  13: MOVE      R10 R0
///  14: LOADNIL   R11
///  15: LOADNIL   R12
///  16: FORGPREP  R10 -> 20
///  17: NAMECALL  R15 R14 K0 (+AUX)
///  19: CALL      R15 args=1 results=0
///  20: FORGLOOP  R10 -> 17 (+AUX nvars=2)
///  22: FORGLOOP  R5 -> 12 (+AUX nvars=2)
///  24: RETURN    R0 1
/// ```
fn loop_over_literal_chunk() -> Chunk {
    let code = vec![
        // pcs 0-1 are scratch loads: a NEWTABLE at pc <= 1 is bound eagerly as
        // the ModuleScript `local M = {}` shape and never parks.
        insn_abc(OP_LOADNIL, 3, 0, 0),
        insn_abc(OP_LOADNIL, 4, 0, 0),
        insn_abc(OP_NEWTABLE, 2, 0, 0),
        2,
        insn_abc(OP_MOVE, 3, 0, 0),
        insn_abc(OP_MOVE, 4, 1, 0),
        insn_abc(OP_SETLIST, 2, 3, 3),
        1,
        insn_abc(OP_MOVE, 5, 2, 0),
        insn_abc(OP_LOADNIL, 6, 0, 0),
        insn_abc(OP_LOADNIL, 7, 0, 0),
        insn_ad(OP_FORGPREP, 5, 10),
        insn_ad(OP_JUMPIFNOT, 9, 9),
        insn_abc(OP_MOVE, 10, 0, 0),
        insn_abc(OP_LOADNIL, 11, 0, 0),
        insn_abc(OP_LOADNIL, 12, 0, 0),
        insn_ad(OP_FORGPREP, 10, 3),
        insn_abc(OP_NAMECALL, 15, 14, 0),
        0,
        insn_abc(OP_CALL, 15, 2, 1),
        insn_ad(OP_FORGLOOP, 10, -4),
        2,
        insn_ad(OP_FORGLOOP, 5, -11),
        2,
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    let proto = Proto {
        max_stack_size: 16,
        num_params: 2,
        num_upvalues: 0,
        is_vararg: false,
        flags: 0,
        typeinfo: None,
        code,
        constants: vec![crate::parser::types::Constant::String("Touch".to_string())],
        child_protos: Vec::new(),
        line_defined: 1,
        debug_name: Some("walk".to_string()),
        line_info: None,
        debug_info: None,
    };
    Chunk {
        version: 6,
        types_version: 0,
        strings: vec!["Touch".to_string()],
        protos: vec![proto],
        main_proto: 0,
    }
}

#[test]
fn iterated_literal_is_not_duplicated_inside_the_loop_body() {
    let chunk = loop_over_literal_chunk();
    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);
    // Exactly one table constructor exists in the bytecode (pc 0), so exactly
    // one may exist in the output: the `for ... in {…}` header's.
    let constructors = out.matches('{').count();
    assert_eq!(
        constructors, 1,
        "the consumed loop literal was re-emitted as a phantom local:\n{out}"
    );
    assert!(out.contains("Touch"), "the arm's call must survive:\n{out}");
}
