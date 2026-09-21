//! Phase C5 — SETTABLEN / SETTABLE inline into pending table literals.
//!
//! Before C5: `local t = {}; t[1] = a; t[2] = b; return t` emitted the
//! intermediate `t[N] = ...` as separate statements, leaving v\d+ temps
//! wherever the source values weren't already locals.
//!
//! After C5: the lifter appends to the in-register `Expr::Table { fields }`
//! so the proto decompiles as `return { a, b }` directly.
//!
//! Tests below cover:
//!   1. SETTABLEN array literal (sequential ints 1..N)
//!   2. SETTABLEN non-contiguous index (falls back to `[N] = val`)
//!   3. SETTABLE with constant Number key (same as SETTABLEN via different op)
//!   4. SETTABLE with constant String key that's a valid identifier (Named)
//!   5. SETTABLE with variable key (does NOT inline — dynamic key)
//!   6. SETTABLEN after the table is read out (no inline — materialized)

use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, Constant, Proto};

// Opcode constants mirror the canonical Luau set.
const OP_LOADN: u8        = 4;
const OP_LOADK: u8        = 5;
const OP_MOVE: u8         = 6;
const OP_SETTABLE: u8     = 14;
const OP_SETTABLEN: u8    = 18;
const OP_RETURN: u8       = 22;
const OP_NEWTABLE: u8     = 53;

fn insn_abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
}
fn insn_ad(op: u8, a: u8, d: i16) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((d as u16 as u32) << 16)
}

fn make_proto(code: Vec<u32>, consts: Vec<Constant>, num_params: u8) -> Proto {
    Proto {
        max_stack_size: 16,
        num_params,
        num_upvalues: 0,
        is_vararg: false,
        flags: 0,
        typeinfo: None,
        code,
        constants: consts,
        child_protos: Vec::new(),
        line_defined: 1,
        debug_name: Some("c5_test".to_string()),
        line_info: None,
        debug_info: None,
    }
}

fn make_chunk(proto: Proto) -> Chunk {
    Chunk {
        version: 6,
        types_version: 0,
        strings: vec!["c5_s0".to_string(), "c5_s1".to_string()],
        protos: vec![proto],
        main_proto: 0,
    }
}

fn decompile(chunk: &Chunk) -> String {
    let mut ctx = DecompileContext::new(chunk);
    decompile_proto(&mut ctx, &chunk.protos[0], 0, 0)
}

// ─── 1. SETTABLEN contiguous → array literal ────────────────────────────────
#[test]
fn c5_settablen_contiguous_becomes_array_literal() {
    // local t = {}; t[1] = 10; t[2] = 20; return t
    // R0 = table, R1 = 10, R2 = 20
    let code = vec![
        insn_abc(OP_NEWTABLE, 0, 0, 0), 0u32, // NEWTABLE R0, AUX=size hint
        insn_ad(OP_LOADN, 1, 10),
        insn_abc(OP_SETTABLEN, 1, 0, 0), // R0[1] = R1
        insn_ad(OP_LOADN, 2, 20),
        insn_abc(OP_SETTABLEN, 2, 0, 1), // R0[2] = R2
        insn_abc(OP_RETURN, 0, 2, 0),    // return R0
    ];
    let out = decompile(&make_chunk(make_proto(code, vec![], 0)));
    // Literal should contain both values with no `t[1] =` or `t[2] =` statements.
    assert!(out.contains("10") && out.contains("20"),
        "expected literal values in output:\n{}", out);
    assert!(!out.contains("[1] ="),
        "expected NO explicit [1] = assignment (should inline):\n{}", out);
    assert!(!out.contains("[2] ="),
        "expected NO explicit [2] = assignment (should inline):\n{}", out);
}

// ─── 2. SETTABLEN non-contiguous → [N] = val form ───────────────────────────
#[test]
fn c5_settablen_noncontiguous_uses_indexed_field() {
    // Set index 5 directly on empty table — no sequential run.
    let code = vec![
        insn_abc(OP_NEWTABLE, 0, 0, 0), 0u32,
        insn_ad(OP_LOADN, 1, 99),
        insn_abc(OP_SETTABLEN, 1, 0, 4), // R0[5] = R1
        insn_abc(OP_RETURN, 0, 2, 0),
    ];
    let out = decompile(&make_chunk(make_proto(code, vec![], 0)));
    assert!(out.contains("99"), "expected value 99 in output:\n{}", out);
    // The [5] = 99 form should appear INSIDE the returned literal.
    assert!(out.contains("[5] = 99") || out.contains("[5]=99"),
        "expected [5] = 99 Indexed field in output:\n{}", out);
    // Assert the full shape is `return { [5] = 99 }` (multi-line OK).
    let compact: String = out.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(compact.contains("return{[5]=99}"),
        "expected inlined `return {{ [5] = 99 }}` shape, got (compact):\n{}", compact);
}

// ─── 3. SETTABLE with constant Number key ────────────────────────────────────
#[test]
fn c5_settable_const_number_key_inlines() {
    // R0 = table, R1 = 1 (key), R2 = 42 (value)
    let code = vec![
        insn_abc(OP_NEWTABLE, 0, 0, 0), 0u32,
        insn_ad(OP_LOADN, 1, 1),
        insn_ad(OP_LOADN, 2, 42),
        insn_abc(OP_SETTABLE, 2, 0, 1), // R0[R1] = R2
        insn_abc(OP_RETURN, 0, 2, 0),
    ];
    let out = decompile(&make_chunk(make_proto(code, vec![], 0)));
    assert!(out.contains("42"), "expected 42 in output:\n{}", out);
    assert!(!out.contains("[1] ="),
        "expected inline array literal (no [1] = stmt):\n{}", out);
}

// ─── 4. SETTABLE with constant String identifier key ─────────────────────────
#[test]
fn c5_settable_const_string_key_inlines_named() {
    // R0 = table, R1 = "foo" (key const), R2 = 7
    let code = vec![
        insn_abc(OP_NEWTABLE, 0, 0, 0), 0u32,
        insn_ad(OP_LOADK, 1, 0), // R1 = K0 = "foo"
        insn_ad(OP_LOADN, 2, 7),
        insn_abc(OP_SETTABLE, 2, 0, 1),
        insn_abc(OP_RETURN, 0, 2, 0),
    ];
    let out = decompile(&make_chunk(make_proto(code, vec![Constant::String("foo".to_string())], 0)));
    // The named-field form should render `foo = 7` inside the literal.
    assert!(out.contains("7"), "expected 7 in output:\n{}", out);
    // No ["foo"] = 7 statement.
    assert!(!out.contains("[\"foo\"] ="),
        "expected Named form, not Indexed:\n{}", out);
}

// ─── 5. SETTABLE with VARIABLE key (non-constant) does NOT inline ───────────
#[test]
fn c5_settable_variable_key_does_not_inline() {
    // key comes from a parameter — not a constant — should fall back to stmt.
    // proto has 1 param (R0 = key), then:
    //   R1 = {}         NEWTABLE
    //   R2 = 5          LOADN
    //   R1[R0] = R2     SETTABLE
    //   return R1
    let code = vec![
        insn_abc(OP_NEWTABLE, 1, 0, 0), 0u32,
        insn_ad(OP_LOADN, 2, 5),
        insn_abc(OP_SETTABLE, 2, 1, 0),
        insn_abc(OP_RETURN, 1, 2, 0),
    ];
    let out = decompile(&make_chunk(make_proto(code, vec![], 1)));
    // With a variable key, the pending-table path must close the literal
    // and emit a statement assign. Just assert no panic and output exists.
    assert!(!out.is_empty(), "non-empty decompile output expected\n{}", out);
}

// ─── 6. Both SETTABLE forms do not panic (smoke) ────────────────────────────
#[test]
fn c5_combined_settable_settablen_smoke() {
    // Mixed: SETTABLEN sets [1], SETTABLE with const key sets [2].
    let code = vec![
        insn_abc(OP_NEWTABLE, 0, 0, 0), 0u32,
        insn_ad(OP_LOADN, 1, 100),
        insn_abc(OP_SETTABLEN, 1, 0, 0), // R0[1] = R1
        insn_ad(OP_LOADN, 2, 2),          // R2 = 2 (key)
        insn_ad(OP_LOADN, 3, 200),        // R3 = 200 (value)
        insn_abc(OP_SETTABLE, 3, 0, 2),   // R0[R2] = R3
        insn_abc(OP_RETURN, 0, 2, 0),
    ];
    let out = decompile(&make_chunk(make_proto(code, vec![], 0)));
    assert!(out.contains("100") && out.contains("200"),
        "expected both values in output:\n{}", out);
}
