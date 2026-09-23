//! Pins the `capture_global_upvals` trust widening added for the
//! `upvalue_capture` family (campaign3/upvalue_capture_b).
//!
//! Before this change, `capture_global_upvals` (and the matching population
//! site in opcode_handlers.rs) trusted a CAPTURE-inferred name through the
//! `is_stdlib_shadow_name` guard ONLY for the three Roblox instance globals
//! (`game` / `workspace` / `script`) — see
//! `capture_param_global_upval_tests::val_capture_of_script_global_keeps_the_global_name`.
//! Any OTHER stdlib global captured the identical way (`local error = error`,
//! `task.spawn(error, msg)`, `for k, v in next, t do`) was refused by the
//! guard and surfaced as a bare `upval_N`, or — once a sibling usage-based
//! heuristic ran — an unbound placeholder like `func`/`func_2`/`lib`.
//!
//! Real corpus origins (11-game v12 live capture): BloxFruits
//! `ReplicatedStorage_Modules_Net_netutil.lua` (`for k, v in upval_1, tbl`
//! should be `next`), Slayers2
//! `.../simplesignal.lua` (`task.spawn(upval_3, formatted)` should be
//! `error`), and dozens of `func`/`typeof`/`setmetatable`/`coroutine.create`
//! sites across every game once the naming heuristic's placeholder was
//! traced back to its CAPTURE.
//!
//! The fix widens the SAME mechanism (never invents a name — only lets a
//! CAPTURE-proven one through the shadow guard) from the 3-name allowlist to
//! `is_stdlib_shadow_name` at large, and additionally makes a `CAPTURE upval`
//! re-capture of an already-trusted PARENT upvalue inherit that trust by
//! walking the parent's own trust set (never by probing a register — for a
//! type-2 capture `cap_idx` is an upvalue INDEX, not a register, and
//! `regs[cap_idx]` would test an unrelated value that happens to share the
//! same small index).

use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, Constant, Proto};

// Canonical Luau opcode bytes (identity opmap, Chunk.version = 6) — same
// encoding `capture_param_global_upval_tests.rs` uses.
const OP_GETGLOBAL: u8 = 7;
const OP_GETUPVAL: u8 = 9;
const OP_NEWCLOSURE: u8 = 19;
const OP_RETURN: u8 = 22;
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

/// A VAL capture whose source register genuinely held the `error` global (an
/// ordinary stdlib function, NOT one of the three Roblox instance globals)
/// must be named exactly `error`, not demoted to `upval_N`.
///
/// ```text
/// parent():                                 ; num_params = 0 (main-like)
///   0: GETGLOBAL R0, K0 ("error")           ; regs[0] := global `error`
///   2: NEWCLOSURE R1, child#0
///   3: CAPTURE   val 0                        ; child U0 = error (by value)
///   4: RETURN    R1, 1
/// child(U0):
///   0: GETUPVAL   R0, U0
///   1: RETURN     R0, 1
/// ```
#[test]
fn val_capture_of_ordinary_stdlib_global_keeps_the_global_name() {
    let parent_code = vec![
        insn_ad(OP_GETGLOBAL, 0, 0),
        0u32, // AUX
        insn_ad(OP_NEWCLOSURE, 1, 0),
        insn_abc(OP_CAPTURE, 0, 0, 0), // val R0
        insn_abc(OP_RETURN, 1, 2, 0),
    ];
    let parent_constants = vec![Constant::String("error".to_string())];
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
        strings: vec!["error".to_string()],
        protos: vec![parent, child],
        main_proto: 0,
    };

    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    assert!(
        !out.contains("upval_"),
        "a val-captured `error` global must keep its exact name, not become a \
         bare upval_N:\n{out}"
    );
    assert!(
        out.contains("return error"),
        "child should return the captured `error` global by name:\n{out}"
    );
}

/// Two sibling closures each capture a DIFFERENT ordinary stdlib global by
/// value from the SAME parent. Both must resolve — the fix must not be
/// order-sensitive or apply to only the first capture in a proto (the exact
/// inconsistency a prior, reverted attempt at this family left in place:
/// one of two structurally identical sibling closures kept the unbound
/// `func` placeholder while the other correctly resolved).
///
/// ```text
/// parent():
///   0: GETGLOBAL R0, K0 ("print")
///   2: GETGLOBAL R1, K1 ("warn")
///   4: NEWCLOSURE R2, child0#0
///   5: CAPTURE   val 0                        ; child0 U0 = print
///   6: NEWCLOSURE R3, child1#0
///   7: CAPTURE   val 1                        ; child1 U0 = warn
///   8: RETURN    R2, 2
/// child0(U0): GETUPVAL R0,U0; RETURN R0,1
/// child1(U0): GETUPVAL R0,U0; RETURN R0,1
/// ```
#[test]
fn val_captures_of_two_sibling_stdlib_globals_both_resolve() {
    let parent_code = vec![
        insn_ad(OP_GETGLOBAL, 0, 0),
        0u32, // AUX
        insn_ad(OP_GETGLOBAL, 1, 1),
        0u32, // AUX
        insn_ad(OP_NEWCLOSURE, 2, 0),
        insn_abc(OP_CAPTURE, 0, 0, 0), // val R0 ("print")
        insn_ad(OP_NEWCLOSURE, 3, 1),
        insn_abc(OP_CAPTURE, 0, 1, 0), // val R1 ("warn")
        insn_abc(OP_RETURN, 2, 3, 0),
    ];
    let parent_constants = vec![
        Constant::String("print".to_string()),
        Constant::String("warn".to_string()),
    ];
    let child0_code = vec![
        insn_abc(OP_GETUPVAL, 0, 0, 0),
        insn_abc(OP_RETURN, 0, 2, 0),
    ];
    let child1_code = child0_code.clone();
    let mut parent = proto(parent_code, parent_constants, 0, 0, "main");
    parent.child_protos = vec![1, 2];
    let child0 = proto(child0_code, Vec::new(), 0, 1, "child0");
    let child1 = proto(child1_code, Vec::new(), 0, 1, "child1");
    let chunk = Chunk {
        version: 6,
        types_version: 0,
        strings: vec!["print".to_string(), "warn".to_string()],
        protos: vec![parent, child0, child1],
        main_proto: 0,
    };

    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    // "func" as a standalone identifier (the unbound-usage-guess placeholder
    // this family's `func`/`func_2` symptom uses) — not merely as a substring
    // of the `function` keyword, which every closure literal contains.
    let has_bare_func_placeholder = out
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .any(|tok| tok == "func" || tok == "func_2");
    assert!(
        !out.contains("upval_") && !has_bare_func_placeholder,
        "neither sibling closure may fall back to a bare upval_N or an \
         unbound `func` placeholder:\n{out}"
    );
    assert!(
        out.contains("return print"),
        "the first closure should return the captured `print` global:\n{out}"
    );
    assert!(
        out.contains("return warn"),
        "the second closure should return the captured `warn` global:\n{out}"
    );
}
