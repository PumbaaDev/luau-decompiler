//! Detector-era rewrite rules must not run on an exact opcode map.
//!
//! B0.124, B0.126, B0.128 and C10e second-guess the bytecode on the premise
//! that the opmap is a detector guess. On a measured-DB / canonical /
//! unshuffled decode the bytecode is authoritative, so these rules corrupt
//! correct output (oracle audit goal/oracle_audit_full.json). Each is gated on
//! `is_inferred_opmap_decode()`, same pattern as B0.127. Every rule has an
//! exact-decode test (rule off) and an inferred-decode test (legacy salvage
//! kept).

use crate::decompiler::lifter::InferredDecodeGuard;
use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, Constant, Proto};

const OP_LOADN: u8 = 4;
const OP_LOADK: u8 = 5;
const OP_MOVE: u8 = 6;
const OP_GETGLOBAL: u8 = 7;
const OP_GETTABLEKS: u8 = 15;
const OP_SETTABLEKS: u8 = 16;
const OP_NAMECALL: u8 = 20;
const OP_CALL: u8 = 21;
const OP_RETURN: u8 = 22;
const OP_ORK: u8 = 48;

fn insn_ad(op: u8, a: u8, d: i16) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((d as u16 as u32) << 16)
}

fn insn_abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
}

fn make_chunk(code: Vec<u32>, constants: Vec<Constant>, num_params: u8) -> Chunk {
    let proto = Proto {
        max_stack_size: 16,
        num_params,
        num_upvalues: 0,
        is_vararg: false,
        flags: 0,
        typeinfo: None,
        code,
        constants,
        child_protos: Vec::new(),
        line_defined: 1,
        debug_name: Some("detector_rule_gating".to_string()),
        line_info: None,
        debug_info: None,
    };
    Chunk { version: 6, types_version: 0, strings: vec![], protos: vec![proto], main_proto: 0 }
}

fn decompile(chunk: &Chunk) -> String {
    let mut ctx = DecompileContext::new(chunk);
    decompile_proto(&mut ctx, &chunk.protos[0], 0, 0)
}

fn decompile_inferred(chunk: &Chunk) -> String {
    let _g = InferredDecodeGuard::install(true);
    decompile(chunk)
}

fn s(x: &str) -> Constant {
    Constant::String(x.to_string())
}

// ---------------------------------------------------------------- B0.124

/// oracle 53bf495b: `LOADK R4 "PlayerGui"; LOADN R5 5; NAMECALL WaitForChild`
/// is `gui:WaitForChild("PlayerGui", 5)`; the rule read 5 as constant index 5.
fn waitforchild_timeout_chunk() -> Chunk {
    let code = vec![
        insn_ad(OP_GETGLOBAL, 0, 0), 0,        // R0 = game (K0)
        insn_abc(OP_NAMECALL, 3, 0, 0), 1,     // R3 = R0.WaitForChild (aux K1), R4 = R0
        insn_ad(OP_LOADK, 5, 2),               // R5 = "PlayerGui"
        insn_ad(OP_LOADN, 6, 5),               // R6 = 5
        insn_abc(OP_CALL, 3, 4, 1),            // CALL R3 args=3 results=0
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    let k = vec![
        s("game"), s("WaitForChild"), s("PlayerGui"),
        Constant::Nil, Constant::Nil, s("DECOY_K5"),
    ];
    make_chunk(code, k, 0)
}

#[test]
fn b0124_exact_decode_keeps_numeric_waitforchild_timeout() {
    let src = decompile(&waitforchild_timeout_chunk());
    assert!(!src.contains("DECOY_K5"), "LOADN timeout read as constant index:\n{src}");
    assert!(src.contains("\"PlayerGui\", 5"), "timeout lost:\n{src}");
}

#[test]
fn b0124_inferred_decode_keeps_legacy_number_to_constant_salvage() {
    let src = decompile_inferred(&waitforchild_timeout_chunk());
    assert!(src.contains("DECOY_K5"), "legacy salvage lost:\n{src}");
}

// ---------------------------------------------------------------- B0.128

/// `local a, b = f(); game:FindFirstChild(b)`. R6 (`b`) is written by the CALL
/// (A=5, two results), not by an instruction whose A is 6, so the back-scan
/// walks past it to an older, dead `LOADK R6 "dead"` and substitutes that
/// constant for the live variable.
fn findfirstchild_call_result_arg_chunk() -> Chunk {
    let code = vec![
        insn_ad(OP_GETGLOBAL, 1, 0), 0,        // pc0-1 R1 = game
        insn_ad(OP_LOADK, 6, 2),               // pc2 R6 = "dead"
        insn_ad(OP_GETGLOBAL, 5, 3), 3,        // pc3-4 R5 = f
        insn_abc(OP_CALL, 5, 1, 3),            // pc5 R5, R6 = f()
        insn_abc(OP_NAMECALL, 4, 1, 0), 1,     // pc6-7 R4 = R1.FindFirstChild, R5 = R1
        insn_abc(OP_CALL, 4, 3, 1),            // pc8 R4(R5, R6)
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    make_chunk(code, vec![s("game"), s("FindFirstChild"), s("dead"), s("f")], 0)
}

#[test]
fn b0128_exact_decode_keeps_variable_argument() {
    let src = decompile(&findfirstchild_call_result_arg_chunk());
    assert!(!src.contains("\"dead\""), "variable replaced by a stale constant:
{src}");
}

#[test]
fn b0128_inferred_decode_keeps_legacy_backscan_salvage() {
    let src = decompile_inferred(&findfirstchild_call_result_arg_chunk());
    assert!(src.contains("FindFirstChild(\"dead\")"), "legacy salvage lost:
{src}");
}

// ---------------------------------------------------------------- B0.126

const OP_AND: u8 = 45;
const OP_OR: u8 = 46;

/// oracle d802d5f4 / a24e7939: `return self.Description or "Error"` is
/// `GETTABLEKS R4 R0.Description; ORK R3 R4 K1 "Error"; RETURN R3`. The rule
/// treated the identifier-shaped "Error" as NAMECALL leakage and dropped it.
fn ork_identifier_default_chunk() -> Chunk {
    let code = vec![
        insn_abc(OP_GETTABLEKS, 4, 0, 0), 0,   // R4 = R0.Description (aux K0)
        insn_abc(OP_ORK, 3, 4, 1),             // R3 = R4 or K1
        insn_abc(OP_RETURN, 3, 2, 0),
    ];
    make_chunk(code, vec![s("Description"), s("Error")], 1)
}

/// Register form: `R2 = R1 or R3` with R3 = LOADK "Default".
fn or_identifier_default_chunk() -> Chunk {
    let code = vec![
        insn_abc(OP_GETTABLEKS, 1, 0, 0), 0,   // R1 = R0.Mode
        insn_ad(OP_LOADK, 3, 1),               // R3 = "Default"
        insn_abc(OP_OR, 2, 1, 3),              // R2 = R1 or R3
        insn_abc(OP_RETURN, 2, 2, 0),
    ];
    make_chunk(code, vec![s("Mode"), s("Default")], 1)
}

#[test]
fn b0126_exact_decode_keeps_ork_identifier_default() {
    let src = decompile(&ork_identifier_default_chunk());
    assert!(src.contains("or \"Error\""), "`or K` default dropped:\n{src}");
}

#[test]
fn b0126_exact_decode_keeps_or_register_identifier_default() {
    let src = decompile(&or_identifier_default_chunk());
    assert!(src.contains("or \"Default\""), "`or` default dropped:\n{src}");
}

#[test]
fn b0126_inferred_decode_keeps_legacy_leak_guard() {
    let src = decompile_inferred(&ork_identifier_default_chunk());
    assert!(!src.contains("or \"Error\""), "legacy guard lost:\n{src}");
    let src = decompile_inferred(&or_identifier_default_chunk());
    assert!(!src.contains("or \"Default\""), "legacy guard lost:\n{src}");
}

// ---------------------------------------------------------------- C10e

/// oracle bc327095 / c74a26c8: `SETTABLEKS R0."Destroy" R4` is a real method
/// store (`function Class:Destroy()`); the rule dropped it as an "inherited
/// Roblox method" artifact, leaving the method defined but never stored.
fn settableks_method_name_chunk() -> Chunk {
    let code = vec![
        insn_ad(OP_LOADN, 1, 5),               // R1 = 5
        insn_abc(OP_SETTABLEKS, 1, 0, 0), 0,   // R0.Destroy = R1 (aux K0)
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    make_chunk(code, vec![s("Destroy")], 1)
}

#[test]
fn c10e_exact_decode_keeps_method_named_field_store() {
    let src = decompile(&settableks_method_name_chunk());
    assert!(src.contains(".Destroy = 5"), "method store dropped:\n{src}");
}

#[test]
fn c10e_inferred_decode_keeps_legacy_method_store_drop() {
    let src = decompile_inferred(&settableks_method_name_chunk());
    assert!(!src.contains(".Destroy ="), "legacy drop lost:\n{src}");
}

// ------------------------------------------------- self-referential store

/// oracle 7aa4449e: `SETTABLEKS R0."Name" R0` is a real `v.Name = v`. The
/// NAMECALL-misread premise of the filter needs a guessed opmap.
fn settableks_self_store_chunk() -> Chunk {
    let code = vec![
        insn_abc(OP_SETTABLEKS, 0, 0, 0), 0,   // R0.Name = R0 (aux K0)
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    make_chunk(code, vec![s("Name")], 1)
}

#[test]
fn self_ref_exact_decode_keeps_field_store_of_base() {
    let src = decompile(&settableks_self_store_chunk());
    assert!(src.contains(".Name = "), "real self store dropped:\n{src}");
}

#[test]
fn self_ref_inferred_decode_keeps_legacy_drop() {
    let src = decompile_inferred(&settableks_self_store_chunk());
    assert!(!src.contains(".Name = "), "legacy drop lost:\n{src}");
}
