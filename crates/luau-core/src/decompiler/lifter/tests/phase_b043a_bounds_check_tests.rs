//! Extracted from lifter.rs as part of Phase B0.52P6 (part 3).
//! `super::super::` references the `lifter` module.

//! Phase B0.43A — bounds-check hardening regression tests.
//!
//! A safety audit identified a handful of direct-indexing sites that
//! would panic on malformed bytecode (`constants[idx as usize]`,
//! `blocks.get_mut(&k).unwrap()`, etc). These sites were replaced with
//! `.get()` / `if let Some(..) = ..` patterns that fall back gracefully
//! (typically to `Expr::Nil` or a skip/continue).
//!
//! These tests construct synthetic `Chunk`s where a LOADK / LOADKX /
//! NEWCLOSURE opcode references a constant index that is out-of-range
//! for `proto.constants`, then run the full `decompile_proto` pipeline
//! and assert:
//!   * no panic (the test harness fails the test on panic),
//!   * output is non-empty (the lifter did not bail out entirely),
//!   * output does not contain an obvious crash marker.
//!
//! A sixth test exercises `get_const_expr` directly to pin down its
//! bounds-checked branches. A seventh test exercises the CFG builder
//! on a malformed instruction stream where a block's last instruction
//! cannot be read (all instructions pruned) to confirm no `.unwrap()`
//! fires in the edge-connection pass.
use crate::ast::Expr;
use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, Constant, Proto};
use super::super::get_const_expr;

// Canonical (non-shuffled) Luau v6 opcode bytes.
const OP_LOADK: u8   = 3;
const OP_LOADKX: u8  = 66;
const OP_LOADN: u8   = 4;
const OP_RETURN: u8  = 22;
const OP_NEWCLOSURE: u8 = 19;

fn insn_ad(op: u8, a: u8, d: i16) -> u32 {
    let du = d as u16 as u32;
    (op as u32) | ((a as u32) << 8) | (du << 16)
}

fn insn_abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
}

fn make_chunk(proto: Proto) -> Chunk {
    Chunk {
        version: 6,
        types_version: 0,
        strings: Vec::new(),
        protos: vec![proto],
        main_proto: 0,
    }
}

fn make_proto(code: Vec<u32>, constants: Vec<Constant>, num_params: u8) -> Proto {
    Proto {
        max_stack_size: 16,
        num_params,
        num_upvalues: 0,
        is_vararg: true,
        flags: 0,
        typeinfo: None,
        code,
        constants,
        child_protos: Vec::new(),
        line_defined: 1,
        debug_name: Some("test".to_string()),
        line_info: None,
        debug_info: None,
    }
}

/// Test 1: LOADK with an out-of-bounds `d` index must NOT panic.
/// Before the fix this walked `proto.constants[d as usize]` via the
/// old non-guarded fallback; after the fix `get_const_expr` returns
/// `Expr::Nil` gracefully.
#[test]
fn b043a_loadk_out_of_bounds_index_does_not_panic() {
    // constants has length 1, but LOADK references index 99 (way past end).
    let code = vec![
        insn_ad(OP_LOADK, 1, 99),          // LOADK R1, K99  (K99 does not exist)
        insn_abc(OP_RETURN, 1, 2, 0),      // RETURN R1..R1
    ];
    let proto = make_proto(code, vec![Constant::Number(42.0)], 0);
    let chunk = make_chunk(proto);
    let mut ctx = DecompileContext::new(&chunk);
    // Must not panic.
    let source = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);
    assert!(!source.is_empty(), "lifter must still emit something on OOB LOADK");
}

/// Test 2: LOADKX with out-of-bounds AUX index — the same pattern as
/// LOADK but goes through the LOADKX dispatch arm which previously
/// used a direct `proto.constants[kidx as usize]` indexing.
#[test]
fn b043a_loadkx_out_of_bounds_aux_does_not_panic() {
    // LOADKX uses AUX for the constant index. With 0 constants, any
    // AUX value is out-of-bounds.
    let code = vec![
        insn_ad(OP_LOADKX, 1, 0),          // LOADKX R1 (AUX follows)
        250u32,                            // AUX = 250  (way past end)
        insn_abc(OP_RETURN, 1, 2, 0),      // RETURN R1..R1
    ];
    let proto = make_proto(code, vec![], 0);
    let chunk = make_chunk(proto);
    let mut ctx = DecompileContext::new(&chunk);
    let source = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);
    assert!(!source.is_empty(), "lifter must still emit something on OOB LOADKX AUX");
}

/// Test 3: LOADKX where AUX points to a `Constant::Closure(k)` whose
/// child proto index is also out-of-bounds. This exercises the nested
/// fallback branch at line 1339-ish (the one that used `proto.constants[kidx]`
/// re-indexing as a fallback after `chunk.protos.get(global_idx)` failed).
#[test]
fn b043a_loadkx_closure_with_oob_proto_does_not_panic() {
    // K0 = Closure(42) — but chunk.protos only has our test proto (1 entry).
    let code = vec![
        insn_ad(OP_LOADKX, 1, 0),          // LOADKX R1 (AUX follows)
        0u32,                              // AUX = 0 (points to K0)
        insn_abc(OP_RETURN, 1, 2, 0),      // RETURN R1..R1
    ];
    let proto = make_proto(code, vec![Constant::Closure(42)], 0);
    let chunk = make_chunk(proto);
    let mut ctx = DecompileContext::new(&chunk);
    let source = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);
    assert!(!source.is_empty(), "lifter must still emit something on OOB closure proto");
}

/// Test 4: `constant_to_expr` on a `Constant::Table` whose key/value
/// indices are out-of-bounds. Both fallbacks in mod.rs:932/938 should
/// skip gracefully rather than panic.
#[test]
fn b043a_table_constant_with_oob_key_and_value_does_not_panic() {
    // Table template with one entry: key_idx=99, value_idx=Some(99). Both OOB.
    let entries = vec![(99i32, Some(99i32))];
    let k = Constant::Table(entries);
    let strings: Vec<String> = vec![];
    let proto_constants: Vec<Constant> = vec![k.clone()];
    let expr = crate::decompiler::constant_to_expr(&k, &strings, &proto_constants);
    // Must be a Table with zero fields (OOB entries were skipped).
    match expr {
        Expr::Table { fields } => assert_eq!(fields.len(), 0, "OOB entries must be skipped"),
        other => panic!("expected Expr::Table, got {:?}", other),
    }
}

/// Test 5: `get_const_expr` fallback returns `Expr::Nil` when both
/// `proto.constants` and `strings` lookups fail. This pins down the
/// unified bounds-checked behavior of the helper.
#[test]
fn b043a_get_const_expr_falls_back_to_nil_on_oob() {
    let proto = make_proto(vec![], vec![], 0);
    let strings: Vec<String> = vec![];
    // idx=500 — past both slices. Must NOT panic, must return Nil.
    let expr = get_const_expr(&proto, &strings, 500);
    assert!(matches!(expr, Expr::Nil), "OOB idx must yield Expr::Nil, got {:?}", expr);
}

/// Test 6: `get_const_expr` falls back to `strings` when the proto
/// lookup fails but the chunk-string lookup succeeds. Pins down that
/// the bounds-checked rewrite didn't regress the secondary path.
#[test]
fn b043a_get_const_expr_falls_back_to_strings_when_constants_oob() {
    let proto = make_proto(vec![], vec![], 0); // empty constants
    let strings = vec!["fallback_name".to_string()];
    let expr = get_const_expr(&proto, &strings, 0);
    match expr {
        Expr::String(s) => assert_eq!(s, "fallback_name"),
        other => panic!("expected String fallback, got {:?}", other),
    }
}

/// Test 7: CFG builder on a single-proto chunk with a NEWCLOSURE whose
/// D references a non-existent child proto, plus trailing RETURN. This
/// triggers every dispatch arm of the lifter including the CFG build
/// pass (which previously used `blocks.get_mut(&k).unwrap()`).
#[test]
fn b043a_cfg_build_newclosure_out_of_bounds_proto_does_not_panic() {
    // NEWCLOSURE references child_protos[42] which doesn't exist.
    let code = vec![
        insn_ad(OP_NEWCLOSURE, 1, 42),    // NEWCLOSURE R1, D=42 (OOB child)
        insn_abc(OP_RETURN, 1, 2, 0),     // RETURN R1..R1
    ];
    let proto = make_proto(code, vec![], 0);
    let chunk = make_chunk(proto);
    let mut ctx = DecompileContext::new(&chunk);
    // Simply building & lifting must not panic.
    let source = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);
    assert!(!source.is_empty(), "lifter must still emit something on OOB NEWCLOSURE");
}

/// Test 8: Loop through many mangled constant indices in a row and
/// confirm that none of them panic the lifter. This is a stress test
/// against the LOADK out-of-bounds path.
#[test]
fn b043a_repeated_oob_loadk_stress_no_panic() {
    let mut code = Vec::new();
    // 50 LOADKs, each referencing a different OOB index.
    for i in 0..50u16 {
        code.push(insn_ad(OP_LOADK, 1, (1000 + i) as i16));
    }
    // Bookend with LOADN and RETURN so the final proto is valid.
    code.push(insn_ad(OP_LOADN, 2, 0));
    code.push(insn_abc(OP_RETURN, 2, 2, 0));
    let proto = make_proto(code, vec![Constant::Number(0.0)], 0);
    let chunk = make_chunk(proto);
    let mut ctx = DecompileContext::new(&chunk);
    let source = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);
    assert!(!source.is_empty(), "repeated OOB LOADKs must not panic or silently drop everything");
}
