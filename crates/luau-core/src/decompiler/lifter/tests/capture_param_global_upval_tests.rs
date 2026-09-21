//! Two CAPTURE-resolution cases that used to surface as a bare `upval_N`
//! (declared-but-never-assigned, read → nil) even though the parent's CAPTURE
//! instructions name the source exactly. Both were long filed as an
//! "unfixable floor: Roblox strips CAPTURE data" — the disassembly proves the
//! CAPTURE is present and the lifter simply did not use it.
//!
//! Real corpus origins (the measured corpus):
//!   * case A — child U2 read `upval_2`. Parent proto
//!     "Translate" captures it with `CAPTURE ref 1`, i.e. the SECOND parameter
//!     (`arg2`), conditionally defaulted with `arg2 = arg2 or game`. The value
//!     tracker moved `regs[1]` to the global `game`, so the upvalue was named
//!     after a global and the `is_stdlib_shadow_name` guard dropped it to
//!     `upval_2`. A REF capture of a PARAMETER shares that parameter's cell, so
//!     the correct, stable name is the parameter name.
//!   * case B — child U5 read `upval_5`. Parent (main)
//!     captures it with `CAPTURE val 5`, and register 5 genuinely held the
//!     `script` global. Naming the upvalue `script` is EXACT (its value IS the
//!     global), yet the same shadow guard dropped it to `upval_5`.

use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, Constant, Proto};

// Canonical Luau opcode bytes (identity opmap, Chunk.version = 6).
const OP_GETGLOBAL: u8 = 7;
const OP_GETUPVAL: u8 = 9;
const OP_NEWCLOSURE: u8 = 19;
const OP_RETURN: u8 = 22;
const OP_CLOSEUPVALS: u8 = 11;
const OP_CAPTURE: u8 = 70;

fn insn_ad(op: u8, a: u8, d: i16) -> u32 {
    let du = d as u16 as u32;
    (op as u32) | ((a as u32) << 8) | (du << 16)
}
fn insn_abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
}
fn proto(code: Vec<u32>, constants: Vec<Constant>, num_params: u8, num_upvalues: u8, name: &str) -> Proto {
    Proto {
        max_stack_size: 16,
        num_params,
        num_upvalues,
        is_vararg: false,
        flags: 0,
        typeinfo: None,
        code,
        constants,
        child_protos: Vec::new(),
        line_defined: 1,
        debug_name: Some(name.to_string()),
        line_info: None,
        debug_info: None,
    }
}

/// A REF capture of a PARAMETER whose tracked value was overwritten by a
/// global must still name the upvalue after the PARAMETER, never after the
/// global and never as a bare `upval_N`.
///
/// ```text
/// parent(arg1, arg2):                       ; num_params = 2
///   0: GETGLOBAL R1, K0 ("game")            ; regs[1] := global `game`
///   2: NEWCLOSURE R2, child#0
///   3: CAPTURE   ref 1                       ; child U0 = &R1  (the arg2 cell)
///   4: CLOSEUPVALS R1
///   5: RETURN    R2, 1                        ; return the closure
/// child(U0):
///   0: GETUPVAL R0, U0
///   1: RETURN   R0, 1                         ; return <upvalue>
/// ```
#[test]
fn ref_capture_of_param_names_the_parameter_not_the_global() {
    let parent_code = vec![
        insn_ad(OP_GETGLOBAL, 1, 0),
        0u32, // AUX
        insn_ad(OP_NEWCLOSURE, 2, 0),
        insn_abc(OP_CAPTURE, 1, 1, 0), // ref R1
        insn_abc(OP_CLOSEUPVALS, 1, 0, 0),
        insn_abc(OP_RETURN, 2, 2, 0),
    ];
    let parent_constants = vec![
        Constant::String("game".to_string()),
    ];
    let child_code = vec![
        insn_abc(OP_GETUPVAL, 0, 0, 0),
        insn_abc(OP_RETURN, 0, 2, 0),
    ];
    let mut parent = proto(parent_code, parent_constants, 2, 0, "translate_parent");
    parent.child_protos = vec![1];
    let child = proto(child_code, Vec::new(), 0, 1, "child");
    let chunk = Chunk {
        version: 6,
        types_version: 0,
        strings: vec!["game".to_string()],
        protos: vec![parent, child],
        main_proto: 0,
    };

    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    // Core regression: the ref-captured parameter must resolve to a real
    // parameter binding — never a bare `upval_N` (the declared-never-assigned
    // defect) and never the global `game` its register transiently held. The
    // exact synthesised parameter name is a naming-hint detail; the invariants
    // that matter are these two negatives. Without the fix, the upvalue is
    // either dropped to `upval_0` (guard reject) or surfaced as `game`
    // (trusted-global path) — both of which these assertions catch.
    assert!(
        !out.contains("upval_"),
        "a ref-captured parameter must resolve to a real binding, not a bare \
         upval_N:\n{out}"
    );
    assert!(
        !out.contains("game"),
        "the upvalue is the parameter CELL captured by reference, never the \
         global `game` its register transiently held:\n{out}"
    );
    // And it must actually return a concrete local, not nothing.
    assert!(
        out.contains("return "),
        "child should return the captured parameter:\n{out}"
    );
}

/// A VAL capture whose source register genuinely held the `script` global is
/// named exactly `script`; the shadow guard must not demote it to `upval_N`.
///
/// ```text
/// parent():                                 ; num_params = 0 (main-like)
///   0: GETGLOBAL R0, K0 ("script")          ; regs[0] := global `script`
///   2: NEWCLOSURE R1, child#0
///   3: CAPTURE   val 0                        ; child U0 = script (by value)
///   4: RETURN    R1, 1
/// child(U0):
///   0: GETUPVAL   R0, U0
///   1: RETURN     R0, 1
/// ```
#[test]
fn val_capture_of_script_global_keeps_the_global_name() {
    let parent_code = vec![
        insn_ad(OP_GETGLOBAL, 0, 0),
        0u32, // AUX
        insn_ad(OP_NEWCLOSURE, 1, 0),
        insn_abc(OP_CAPTURE, 0, 0, 0), // val R0
        insn_abc(OP_RETURN, 1, 2, 0),
    ];
    let parent_constants = vec![
        Constant::String("script".to_string()),
    ];
    let child_code = vec![
        insn_abc(OP_GETUPVAL, 0, 0, 0),
        insn_abc(OP_RETURN, 0, 2, 0),
    ];
    let mut parent = proto(parent_code, parent_constants, 0, 0, "main");
    parent.child_protos = vec![1];
    let child = proto(child_code, Vec::new(), 0, 1, "child");
    let chunk = Chunk {
        version: 6,
        types_version: 0,
        strings: vec!["script".to_string()],
        protos: vec![parent, child],
        main_proto: 0,
    };

    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    assert!(
        !out.contains("upval_"),
        "a val-captured `script` global must keep its exact name, not become a \
         bare upval_N:\n{out}"
    );
    assert!(
        out.contains("return script"),
        "child should return the captured `script` global by name:\n{out}"
    );
}
