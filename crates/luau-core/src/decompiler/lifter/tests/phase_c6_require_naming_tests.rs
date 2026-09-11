//! Phase C6 — require(path) → result register name propagation.
//!
//! The dominant residual `v\d+` source in the post-C5 corpus was undeclared
//! upvalue references in module scope. Roblox module preludes compile to:
//!
//!     local Foo = require(script.Foo)   -- local stored in R0
//!     -- later, nested closures read Foo via upvalues
//!
//! Before C6 the CALL handler computed the result name via `ctx.reg_name`,
//! which returned a generic `v0`. SETUPVAL's backscan rejected that (generic
//! placeholders don't propagate), so the upvalue stayed `upval_N`.
//!
//! After C6, a `require(...)` call with an identifier-tail argument (Field,
//! `WaitForChild("X")`, etc.) seeds the result register with the tail name.
//! SETUPVAL backscan then adopts it, and every nested capture inherits the
//! real name instead of `upval_N`.
//!
//! These tests exercise the CALL-handler branch directly by decompiling a
//! proto that:
//!   1. GETIMPORTs `require`
//!   2. GETIMPORTs a path whose last id resolves to the target identifier
//!   3. CALLs require with the path
//!   4. Returns the single result
//!
//! Success condition: the decompiled output uses the import-tail identifier
//! (e.g. `local Foo = require(...)`), not `local v0 = ...` or `local call0`.

use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, Constant, Proto};

const OP_LOADK: u8     = 5;
const OP_GETIMPORT: u8 = 12;
const OP_CALL: u8      = 21;
const OP_RETURN: u8    = 22;

fn insn_ad(op: u8, a: u8, d: i16) -> u32 {
    let du = d as u16 as u32;
    (op as u32) | ((a as u32) << 8) | (du << 16)
}
fn insn_abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
}

fn pack_import(ids: &[u32]) -> u32 {
    let count = (ids.len() as u32) & 0x3;
    let mut v = count << 30;
    if !ids.is_empty() { v |= (ids[0] & 0x3FF) << 20; }
    if ids.len() >= 2  { v |= (ids[1] & 0x3FF) << 10; }
    if ids.len() >= 3  { v |=  ids[2] & 0x3FF;        }
    v
}

fn make_proto(code: Vec<u32>, constants: Vec<Constant>, strings: Vec<String>) -> (Proto, Vec<String>) {
    let proto = Proto {
        max_stack_size: 16,
        num_params: 0,
        num_upvalues: 0,
        is_vararg: false,
        flags: 0,
        typeinfo: None,
        code,
        constants,
        child_protos: Vec::new(),
        line_defined: 1,
        debug_name: Some("c6_test".to_string()),
        line_info: None,
        debug_info: None,
    };
    (proto, strings)
}

fn decompile_single(proto: Proto, strings: Vec<String>) -> String {
    let chunk = Chunk {
        version: 6,
        types_version: 0,
        strings,
        protos: vec![proto],
        main_proto: 0,
    };
    let mut ctx = DecompileContext::new(&chunk);
    decompile_proto(&mut ctx, &chunk.protos[0], 0, 0)
}

// ─── Test 1: require(import.path.Module) → local Module = require(...) ──────
#[test]
fn c6_require_getimport_tail_names_result() {
    // K[0] = Import(require)                    — resolves to global "require"
    // K[1] = String("require")                  — id0 for import K[0]
    // K[2] = Import(GameModules.MyModule)       — two-level import
    // K[3] = String("GameModules")              — id0
    // K[4] = String("MyModule")                 — id1 (the tail)
    //
    // Bytecode:
    //   0: GETIMPORT R0, K[0]  AUX=import([1])
    //   2: GETIMPORT R1, K[2]  AUX=import([3, 4])
    //   4: CALL R0, B=2 (1 arg), C=2 (1 result)
    //   5: RETURN R0, B=2 (1 val)
    let require_import  = pack_import(&[1]);
    let module_import   = pack_import(&[3, 4]);
    let code = vec![
        insn_ad(OP_GETIMPORT, 0, 0), require_import,
        insn_ad(OP_GETIMPORT, 1, 2), module_import,
        insn_abc(OP_CALL, 0, 2, 2),
        insn_abc(OP_RETURN, 0, 2, 0),
    ];
    let constants = vec![
        Constant::Import(require_import),
        Constant::String("require".into()),
        Constant::Import(module_import),
        Constant::String("GameModules".into()),
        Constant::String("MyModule".into()),
    ];
    let strings = vec![
        "require".into(), "GameModules".into(), "MyModule".into(),
    ];
    let (proto, strings) = make_proto(code, constants, strings);
    let out = decompile_single(proto, strings);
    assert!(out.contains("MyModule"),
        "expected require(...) result bound to `MyModule`, got:\n{}", out);
    assert!(!out.contains("local v0"),
        "result register should NOT fall back to v0:\n{}", out);
    assert!(!out.contains("local call"),
        "result register should NOT fall back to callN:\n{}", out);
}

// ─── Test 2: non-require CALL does NOT adopt require-naming ─────────────────
#[test]
fn c6_non_require_call_unaffected() {
    // Same shape but func is `print`, not `require`. The result register
    // should fall back to the default naming (fn/call/v) — we just assert
    // that the final tail segment "Something" does NOT appear as the LHS.
    // K[0] = Import(print)
    // K[1] = "print"
    // K[2] = Import(some.Something)
    // K[3] = "some"
    // K[4] = "Something"
    let print_import = pack_import(&[1]);
    let path_import  = pack_import(&[3, 4]);
    let code = vec![
        insn_ad(OP_GETIMPORT, 0, 0), print_import,
        insn_ad(OP_GETIMPORT, 1, 2), path_import,
        insn_abc(OP_CALL, 0, 2, 2),
        insn_abc(OP_RETURN, 0, 2, 0),
    ];
    let constants = vec![
        Constant::Import(print_import),
        Constant::String("print".into()),
        Constant::Import(path_import),
        Constant::String("some".into()),
        Constant::String("Something".into()),
    ];
    let strings = vec!["print".into(), "some".into(), "Something".into()];
    let (proto, strings) = make_proto(code, constants, strings);
    let out = decompile_single(proto, strings);
    // "Something" appears on the RHS (inside the print call) but MUST NOT
    // appear as a local name on the left — check there's no "local Something".
    assert!(!out.contains("local Something"),
        "non-require CALL must not adopt require-name heuristic:\n{}", out);
}

// ─── Test 3: require with LOADK-string arg (dynamic path) falls back ────────
#[test]
fn c6_require_with_string_literal_arg_unnamed() {
    // require("some.module.path")  — string arg, not a Field/MethodCall path.
    // Our helper only fires on Field / MethodCall / Name — this test locks
    // the String-arg fallback so we don't accidentally name the local after
    // an arbitrary string content.
    let require_import = pack_import(&[1]);
    let code = vec![
        insn_ad(OP_GETIMPORT, 0, 0), require_import,
        insn_ad(OP_LOADK, 1, 2),                  // R1 = K[2] = string
        insn_abc(OP_CALL, 0, 2, 2),
        insn_abc(OP_RETURN, 0, 2, 0),
    ];
    let constants = vec![
        Constant::Import(require_import),
        Constant::String("require".into()),
        Constant::String("some.module.path".into()),
    ];
    let strings = vec!["require".into(), "some.module.path".into()];
    let (proto, strings) = make_proto(code, constants, strings);
    let out = decompile_single(proto, strings);
    // Must not produce `local some.module.path = ...`
    assert!(!out.contains("local some.module.path"),
        "string-arg require must not name the local with dotted string:\n{}", out);
}
