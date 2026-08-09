//! An arithmetic result is a legal table base in this corpus, and discarding
//! it manufactures the defect it was guarding against.
//!
//! `table_expr` (via `is_impossible_as_table`) rejected EVERY arithmetic
//! BinOp as a table base on the B0.59 assumption "arith → number". That
//! assumption is false for Roblox Luau: Vector3/Vector2/CFrame/UDim2 all
//! overload `+ - * /` and their results are indexed constantly —
//! `(a - b).magnitude` is the idiomatic distance. When the guard fires it
//! throws away the exactly-correct parked expression and mints an unbound
//! `vN`, which `free_var_decls` hoists and `declared_never_assigned` flags.
//!
//! Round-3 census of the 111 bc defects: 86 READONLY chunk-top hoists, 46
//! involving TABLE_EXPR_REJECT, and the discarded values were Sub x92 /
//! Add x12 / Mul x11 / Div x3 — nothing else. Two exhibits proven from raw
//! operand fields under the MEASURED v9 map:
//!
//!   * 473_DragManager proto 13 "updateDrag": pc31 `SUB R2 = R1 - R3`
//!     (Vector2 - Vector2, the drag delta), pcs 39/47 `GETTABLEKS R6 =
//!     R2["X"/"Y"]` — the bytecode indexes the SUB result directly.
//!   * 511_TriangleMaker proto 1 "createTriangle": pc21 `SUB R26 = R0 - R1`,
//!     pc22 `GETTABLEKS R26 = R26["magnitude"]` — Heron's formula over
//!     `(a - b).magnitude` three times.
//!
//! The fix removes exactly Add/Sub/Mul/Div from the impossible set — the
//! four operators Roblox math types overload, and the only four the census
//! ever saw discarded. Mod/Pow/IDiv/Concat/comparisons/bitwise stay
//! rejected: no evidence they occur legitimately, and they still guard the
//! decode-artifact garbage B0.59 was written against.

use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, Constant, Proto};

// Canonical Luau opcode bytes (identity opmap, Chunk.version = 6).
const OP_LOADNIL: u8 = 2;
const OP_LOADN: u8 = 4;
const OP_GETTABLEKS: u8 = 15;
const OP_RETURN: u8 = 22;
const OP_ADD: u8 = 33;
const OP_SUB: u8 = 34;
const OP_MUL: u8 = 35;
const OP_DIV: u8 = 36;
const OP_CONCAT: u8 = 49;

fn insn_ad(op: u8, a: u8, d: i16) -> u32 {
    let du = d as u16 as u32;
    (op as u32) | ((a as u32) << 8) | (du << 16)
}

fn insn_abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
}

/// Two-param proto: `<binop> R2 = R0 <op> R1`, then read K0 off R2, return it.
///
/// ```text
///   0: <op>        R2, R0, R1
///   1: GETTABLEKS  R3, R2, K0     (+AUX)
///   3: RETURN      R3, 2
/// ```
fn field_off_op_chunk(producer: u32, key: &str) -> Chunk {
    let code = vec![
        producer,
        insn_ad(OP_GETTABLEKS, 3, 2),
        0u32, // AUX: key constant index 0
        insn_abc(OP_RETURN, 3, 2, 0),
    ];
    let proto = Proto {
        max_stack_size: 8,
        num_params: 2,
        num_upvalues: 0,
        is_vararg: false,
        flags: 0,
        typeinfo: None,
        code,
        constants: vec![Constant::String(key.to_string())],
        child_protos: Vec::new(),
        line_defined: 1,
        debug_name: Some("dist".to_string()),
        line_info: None,
        debug_info: None,
    };
    Chunk {
        version: 6,
        types_version: 0,
        strings: vec![key.to_string()],
        protos: vec![proto],
        main_proto: 0,
    }
}

fn decompiled(chunk: &Chunk) -> String {
    let mut ctx = DecompileContext::new(chunk);
    decompile_proto(&mut ctx, &chunk.protos[0], 0, 0)
}

/// The output must carry the arithmetic through to the field read — some
/// line must contain BOTH the operator expression and the key — and must not
/// invent an unbound `v2` for a value the register file provably held.
fn assert_arith_base_survives(out: &str, op_text: &str, key: &str) {
    let carried = out
        .lines()
        .any(|l| l.contains(op_text) && l.contains(key));
    assert!(
        carried,
        "the parked `{op_text}` must reach the `{key}` read instead of being \
         discarded as an impossible table base:\n{out}"
    );
    assert!(
        !out.contains("v2"),
        "no minted `v2` may replace the arithmetic result:\n{out}"
    );
}

#[test]
fn sub_result_is_a_legal_table_base() {
    let out = decompiled(&field_off_op_chunk(insn_abc(OP_SUB, 2, 0, 1), "magnitude"));
    assert_arith_base_survives(&out, " - ", "magnitude");
}

#[test]
fn add_result_is_a_legal_table_base() {
    let out = decompiled(&field_off_op_chunk(insn_abc(OP_ADD, 2, 0, 1), "Unit"));
    assert_arith_base_survives(&out, " + ", "Unit");
}

#[test]
fn mul_result_is_a_legal_table_base() {
    let out = decompiled(&field_off_op_chunk(insn_abc(OP_MUL, 2, 0, 1), "X"));
    assert_arith_base_survives(&out, " * ", "X");
}

#[test]
fn div_result_is_a_legal_table_base() {
    let out = decompiled(&field_off_op_chunk(insn_abc(OP_DIV, 2, 0, 1), "Y"));
    assert_arith_base_survives(&out, " / ", "Y");
}

// ── Guards: the reject must keep firing where it is RIGHT ────────────────

/// A number literal indexed with `.field` is still a decode artifact:
/// `(7).magnitude` must not appear; the fallback name is the honest output.
#[test]
fn number_literal_base_still_rejected() {
    let out = decompiled(&field_off_op_chunk(insn_ad(OP_LOADN, 2, 7), "magnitude"));
    assert!(
        !out.contains("(7)"),
        "a number literal must never be emitted as a table base:\n{out}"
    );
}

/// A nil base is the round-2 arm-write family, not this one — the reject
/// stays so the loss remains visible to `declared_never_assigned`.
#[test]
fn nil_base_still_rejected() {
    let out = decompiled(&field_off_op_chunk(insn_abc(OP_LOADNIL, 2, 0, 0), "magnitude"));
    assert!(
        !out.contains("nil.magnitude") && !out.contains("(nil)"),
        "a nil base must never be emitted:\n{out}"
    );
}

/// Concat yields a string; `.field` off it stays rejected — zero census
/// evidence it occurs legitimately, and it still guards misdecodes.
#[test]
fn concat_base_still_rejected() {
    let out = decompiled(&field_off_op_chunk(insn_abc(OP_CONCAT, 2, 0, 1), "magnitude"));
    assert!(
        !out.contains(".. "),
        "a concat result must not be emitted as a table base:\n{out}"
    );
}
