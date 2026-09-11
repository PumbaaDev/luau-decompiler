//! Regression: NAMECALL without a following CALL must not leak a
//! `{ K = "K" }` table literal into emitted source.
//!
//! Investigated 2026-04-20 while tracking the 1611-occurrence
//! `local Foo = { MethodName = "MethodName" }` artifact in HUD/Shop/
//! ChallengePass. Hypothesis that an orphan NAMECALL produces that
//! table shape was FALSIFIED here — the lifter correctly emits empty
//! output when the pending MethodCall is never consumed by a CALL.
//! These tests lock that good behavior so regressions are caught.

use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, Constant, Proto};

const OP_LOADN: u8 = 4;
const OP_GETGLOBAL: u8 = 7;
const OP_SETTABLEKS: u8 = 16;
const OP_NEWTABLE: u8 = 54;
const OP_NAMECALL: u8 = 20;
const OP_RETURN: u8 = 22;
const OP_LOADK: u8 = 5;

fn insn_ad(op: u8, a: u8, d: i16) -> u32 {
    let du = d as u16 as u32;
    (op as u32) | ((a as u32) << 8) | (du << 16)
}

fn insn_abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
}

fn make_proto(code: Vec<u32>, constants: Vec<Constant>, num_params: u8) -> Proto {
    Proto {
        max_stack_size: 32,
        num_params,
        num_upvalues: 2,
        is_vararg: true,
        flags: 0,
        typeinfo: None,
        code,
        constants,
        child_protos: Vec::new(),
        line_defined: 1,
        debug_name: Some("namecall_orphan_diag".to_string()),
        line_info: None,
        debug_info: None,
    }
}

fn make_chunk(proto: Proto) -> Chunk {
    Chunk {
        version: 6,
        types_version: 0,
        strings: vec!["s0".to_string(), "s1".to_string(), "s2".to_string()],
        protos: vec![proto],
        main_proto: 0,
    }
}

fn decompile(chunk: &Chunk) -> String {
    let mut ctx = DecompileContext::new(chunk);
    decompile_proto(&mut ctx, &chunk.protos[0], 0, 0)
}

#[test]
fn namecall_without_call_does_not_leak_method_name_table() {
    // R0 = game, NAMECALL R1,R0 AUX="Get" (→ R1 holds pending MethodCall),
    // then RETURN with no CALL in between. Must not emit `{Get = "Get"}`
    // or any table literal carrying the method name.
    let code = vec![
        insn_ad(OP_GETGLOBAL, 0, 0),
        0u32,                         // GETGLOBAL AUX = K0 = "game"
        insn_abc(OP_NAMECALL, 1, 0, 0),
        0u32,                         // NAMECALL AUX = K0 = "Get"
        insn_abc(OP_RETURN, 0, 1, 0), // RETURN no values
    ];
    let chunk = make_chunk(make_proto(
        code,
        vec![Constant::String("Get".to_string())],
        0,
    ));
    let out = decompile(&chunk);
    assert!(
        !out.contains("Get = \"Get\""),
        "orphan NAMECALL leaked {{K = \"K\"}} table: {:?}",
        out
    );
    assert!(
        !out.contains("{ Get"),
        "orphan NAMECALL produced a method-name table: {:?}",
        out
    );
}

#[test]
fn newtable_settableks_emits_table_literal() {
    // Baseline sanity: NEWTABLE + SETTABLEKS with a numeric value collapses
    // into a return-table literal. Guards the pending-table inline path.
    let code = vec![
        insn_abc(OP_NEWTABLE, 0, 0, 0),
        0u32,                            // NEWTABLE AUX (capacity)
        insn_ad(OP_LOADN, 1, 0),         // R1 = 0
        insn_abc(OP_SETTABLEKS, 1, 0, 0),
        0u32,                            // SETTABLEKS AUX = K0 = "Get"
        insn_abc(OP_RETURN, 0, 2, 0),    // RETURN R0
    ];
    let chunk = make_chunk(make_proto(
        code,
        vec![Constant::String("Get".to_string())],
        0,
    ));
    let out = decompile(&chunk);
    assert!(out.contains("Get ="), "SETTABLEKS didn't emit key: {:?}", out);
    assert!(out.contains("return"), "no return in output: {:?}", out);
}

#[test]
fn newtable_at_pending_namecall_register_does_not_leak() {
    // Hypothesis: NAMECALL leaves pending MethodCall at R1, NEWTABLE at
    // the SAME register R1 should cleanly overwrite — no `{Get = "Get"}`
    // leaking from the NAMECALL's method-name string through.
    let code = vec![
        insn_ad(OP_GETGLOBAL, 0, 0),
        0u32,                            // GETGLOBAL AUX = K0 = "Get"
        insn_abc(OP_NAMECALL, 1, 0, 0),
        0u32,                            // NAMECALL AUX = K0 = "Get"
        insn_abc(OP_NEWTABLE, 1, 0, 0),  // NEWTABLE at R1 (same reg)
        0u32,                            // NEWTABLE AUX
        insn_abc(OP_RETURN, 1, 2, 0),    // RETURN R1
    ];
    let chunk = make_chunk(make_proto(
        code,
        vec![Constant::String("Get".to_string())],
        0,
    ));
    let out = decompile(&chunk);
    assert!(
        !out.contains("Get = \"Get\""),
        "NAMECALL→NEWTABLE at same reg leaked K=\"K\": {:?}",
        out
    );
}

#[test]
fn loadk_then_settableks_emits_matching_key_value() {
    // Direct repro of the corpus shape: LOADK R1 = "Get", NEWTABLE R0,
    // SETTABLEKS R0["Get"] = R1. Produces exactly `{ Get = "Get" }`.
    // This test documents that when BOTH the SETTABLEKS AUX string AND
    // the value-register string happen to match, the lifter faithfully
    // emits the K=K shape — meaning the corpus bug is upstream (some
    // bytecode is being (mis)decoded to produce these two matching
    // strings). NOT a lifter bug at this level.
    let code = vec![
        insn_ad(OP_LOADK, 1, 0),          // R1 = K0 = "Get"
        insn_abc(OP_NEWTABLE, 0, 0, 0),
        0u32,                             // NEWTABLE AUX
        insn_abc(OP_SETTABLEKS, 1, 0, 0),
        0u32,                             // SETTABLEKS AUX = K0 = "Get"
        insn_abc(OP_RETURN, 0, 2, 0),     // RETURN R0
    ];
    let chunk = make_chunk(make_proto(
        code,
        vec![Constant::String("Get".to_string())],
        0,
    ));
    let out = decompile(&chunk);
    assert!(
        out.contains("Get = \"Get\""),
        "LOADK+SETTABLEKS with matching strings didn't emit K=\"K\": {:?}",
        out
    );
}
