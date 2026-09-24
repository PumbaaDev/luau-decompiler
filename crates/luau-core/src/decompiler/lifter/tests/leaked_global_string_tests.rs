//! B0.127 leaked-global-string: a LOADK of a string is a string.
//!
//! `sanitize_leaked_global_string` rewrote an assignment/return value
//! `Expr::String(s)` into the global `Expr::Name(s)` whenever `s` was a stdlib
//! or Roblox global name, on the premise that the compiler never LOADKs such a
//! name. Real source does: the v12 oracle file 1b8591b7 compiles
//! `return "error", v35, v36` to `LOADK R8 K34 ; "error"` + `RETURN R8..R10`,
//! and the rule emitted `return error, ...` (the global function). On an exact
//! decode the string must survive; on a detector-inferred decode the old
//! salvage stays, since there a string in that slot can be a misread import.

use crate::decompiler::lifter::InferredDecodeGuard;
use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, Constant, Proto};

const OP_LOADK: u8 = 5;
const OP_LOADB: u8 = 3;
const OP_SETTABLEKS: u8 = 16;
const OP_RETURN: u8 = 22;

fn insn_ad(op: u8, a: u8, d: i16) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((d as u16 as u32) << 16)
}

fn insn_abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
}

fn make_chunk(code: Vec<u32>, constants: Vec<Constant>, num_params: u8) -> Chunk {
    let proto = Proto {
        max_stack_size: 8,
        num_params,
        num_upvalues: 0,
        is_vararg: false,
        flags: 0,
        typeinfo: None,
        code,
        constants,
        child_protos: Vec::new(),
        line_defined: 1,
        debug_name: Some("leaked_global_string_test".to_string()),
        line_info: None,
        debug_info: None,
    };
    Chunk { version: 6, types_version: 0, strings: vec![], protos: vec![proto], main_proto: 0 }
}

fn decompile(chunk: &Chunk) -> String {
    let mut ctx = DecompileContext::new(chunk);
    decompile_proto(&mut ctx, &chunk.protos[0], 0, 0)
}

/// `return "error", true` — the 1b8591b7 shape.
fn return_error_tag_chunk() -> Chunk {
    let code = vec![
        insn_ad(OP_LOADK, 0, 0),          // R0 = K0 "error"
        insn_ad(OP_LOADB, 1, 1),          // R1 = true
        insn_abc(OP_RETURN, 0, 3, 0),     // return R0, R1
    ];
    make_chunk(code, vec![Constant::String("error".into())], 0)
}

/// `arg0.Status = "math"` — a SETTABLEKS value that equals a library name.
fn settableks_math_chunk() -> Chunk {
    let code = vec![
        insn_ad(OP_LOADK, 1, 1),                // R1 = K1 "math"
        insn_abc(OP_SETTABLEKS, 1, 0, 0), 0,    // R0.K0 = R1  (AUX = K0 "Status")
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    make_chunk(
        code,
        vec![Constant::String("Status".into()), Constant::String("math".into())],
        1,
    )
}

#[test]
fn exact_decode_keeps_loadk_string_in_return() {
    let src = decompile(&return_error_tag_chunk());
    assert!(src.contains("return \"error\", true"), "string constant lost:\n{src}");
    assert!(!src.contains("return error"), "string rewritten to the global:\n{src}");
}

#[test]
fn exact_decode_keeps_loadk_string_in_field_write() {
    let src = decompile(&settableks_math_chunk());
    assert!(src.contains("= \"math\""), "string constant lost:\n{src}");
    assert!(!src.contains("= math\n") && !src.trim_end().ends_with("= math"),
        "string rewritten to the global:\n{src}");
}

#[test]
fn inferred_decode_keeps_legacy_leaked_global_salvage() {
    // Under a detector-guessed opmap a stdlib-name string in a return/assignment
    // slot can be a misread GETIMPORT; the salvage is still applied there.
    let src = {
        let _g = InferredDecodeGuard::install(true);
        decompile(&return_error_tag_chunk())
    };
    assert!(src.contains("return error, true"), "legacy salvage lost:\n{src}");
    // Guard restored: the next (exact) lift keeps the string again.
    let src2 = decompile(&return_error_tag_chunk());
    assert!(src2.contains("return \"error\", true"), "guard leaked:\n{src2}");
}
