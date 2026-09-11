//! Phase C8 — require({K=X}) wrapper unwrap.
//!
//! The C6b cold-corpus audit found a dominant pattern in ClientScript*.lua
//! and framework-style files:
//!
//!     local Client = { Gui = v13 }
//!     local Parent2 = require(Client)
//!     Parent2.Play = ...
//!
//! The `{ K = X }` wrapper is a decompiler artifact — `require(table)` is a
//! Roblox runtime error, so the wrapper is never semantically correct. Any
//! surviving case is either a decompiler artifact or broken code.
//!
//! C8 unwraps at the CALL handler: when the `require` argument is a single
//! `TableField::Named` wrapping a Name/Field/Index/MethodCall/Call, emit the
//! inner expression directly as the argument. Multi-field tables and non-
//! simple inner values still go through the B0.114b materialization path.
//!
//! Tests:
//!   1. Single-field `{ K = R }` with a Name inner → unwrap.
//!   2. Non-require CALL with same arg shape → NOT touched.
//!   3. Multi-field table `{ A = ..., B = ... }` passed to require → still
//!      falls through to B0.114b materialization (not unwrapped).

use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, Constant, Proto};

const OP_LOADK: u8       = 5;
const OP_GETIMPORT: u8   = 12;
const OP_SETTABLEKS: u8  = 16;
const OP_CALL: u8        = 21;
const OP_RETURN: u8      = 22;
const OP_NEWTABLE: u8    = 53;

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

fn decompile(chunk: &Chunk) -> String {
    let mut ctx = DecompileContext::new(chunk);
    decompile_proto(&mut ctx, &chunk.protos[0], 0, 0)
}

fn proto(code: Vec<u32>, constants: Vec<Constant>) -> Proto {
    Proto {
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
        debug_name: Some("c8_test".to_string()),
        line_info: None,
        debug_info: None,
    }
}

fn chunk(proto: Proto, strings: Vec<String>) -> Chunk {
    Chunk {
        version: 6,
        types_version: 0,
        strings,
        protos: vec![proto],
        main_proto: 0,
    }
}

// ─── 1. require({ Gui = Module }) → require(Module) ─────────────────────────
#[test]
fn c8_require_single_field_wrapper_unwrapped() {
    // K[0] = Import(require)               → id0 = "require" (string 1)
    // K[1] = String("require")
    // K[2] = Import(script.MyModule)       → id0=script(2), id1=MyModule(3)
    // K[3] = String("script")
    // K[4] = String("MyModule")
    // K[5] = String("Gui")                 — wrapper field name
    //
    // R0 = require             GETIMPORT R0, K[0]
    // R3 = script.MyModule     GETIMPORT R3, K[2]
    // R1 = {}                  NEWTABLE R1  (wrapper)
    // R1.Gui = R3              SETTABLEKS A=3 B=1 AUX=5
    // R0 = require(R1)         CALL R0 B=2 C=2
    // return R0
    let require_import  = pack_import(&[1]);
    let module_import   = pack_import(&[3, 4]);
    let code = vec![
        insn_ad(OP_GETIMPORT, 0, 0), require_import,
        insn_ad(OP_GETIMPORT, 3, 2), module_import,
        insn_ad(OP_NEWTABLE, 1, 0), 0u32,
        insn_abc(OP_SETTABLEKS, 3, 1, 0), 5u32, // AUX = K[5] = "Gui"
        insn_abc(OP_CALL, 0, 2, 2),
        insn_abc(OP_RETURN, 0, 2, 0),
    ];
    let constants = vec![
        Constant::Import(require_import),
        Constant::String("require".into()),
        Constant::Import(module_import),
        Constant::String("script".into()),
        Constant::String("MyModule".into()),
        Constant::String("Gui".into()),
    ];
    let strings = vec![
        "require".into(),
        "script".into(),
        "MyModule".into(),
        "Gui".into(),
    ];
    let out = decompile(&chunk(proto(code, constants), strings));
    // Wrapper should be gone — no `{ Gui = ...` inside require.
    assert!(!out.contains("Gui ="),
        "wrapper `Gui = ...` must be unwrapped inside require:\n{}", out);
    assert!(!out.contains("{"),
        "no table literal expected in final output — require arg should be bare:\n{}", out);
    // The MyModule identifier tail must appear somewhere (either as arg or result name).
    assert!(out.contains("MyModule"),
        "expected MyModule tail to survive unwrap:\n{}", out);
}

// ─── 2. non-require CALL with `{ K = R }` arg → NOT unwrapped ────────────────
#[test]
fn c8_non_require_call_wrapper_preserved() {
    // Same shape but func is `print`. We must NOT unwrap the wrapper — it
    // changes semantics for any function that actually takes tables.
    //
    // K[0] = Import(print)
    // K[1] = "print"
    // K[2] = Import(script.SomeValue)
    // K[3] = "script"
    // K[4] = "SomeValue"
    // K[5] = "Field"
    let print_import = pack_import(&[1]);
    let val_import   = pack_import(&[3, 4]);
    let code = vec![
        insn_ad(OP_GETIMPORT, 0, 0), print_import,
        insn_ad(OP_GETIMPORT, 3, 2), val_import,
        insn_ad(OP_NEWTABLE, 1, 0), 0u32,
        insn_abc(OP_SETTABLEKS, 3, 1, 0), 5u32,
        insn_abc(OP_CALL, 0, 2, 1), // C=1 → 0 results (statement)
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    let constants = vec![
        Constant::Import(print_import),
        Constant::String("print".into()),
        Constant::Import(val_import),
        Constant::String("script".into()),
        Constant::String("SomeValue".into()),
        Constant::String("Field".into()),
    ];
    let strings = vec![
        "print".into(), "script".into(),
        "SomeValue".into(), "Field".into(),
    ];
    let out = decompile(&chunk(proto(code, constants), strings));
    // The wrapper must still appear somewhere in the output — C8 only fires
    // for `require` calls. Either as `{ Field = ... }` inline or via a local.
    let has_wrapper = out.contains("Field =") || out.contains("Field=");
    assert!(has_wrapper,
        "non-require call must preserve the wrapper table:\n{}", out);
}

// ─── 3. multi-field wrapper → falls back to B0.114b materialize ─────────────
#[test]
fn c8_require_multi_field_wrapper_not_unwrapped() {
    // require({ Gui = X, Framework = Y }) — two Named fields.
    // Must NOT unwrap (which field would we pick?) — B0.114b path materializes
    // a local wrapper instead.
    let require_import = pack_import(&[1]);
    let x_import = pack_import(&[3, 4]);
    let y_import = pack_import(&[3, 5]);
    let code = vec![
        insn_ad(OP_GETIMPORT, 0, 0), require_import,
        insn_ad(OP_GETIMPORT, 3, 2), x_import,
        insn_ad(OP_GETIMPORT, 4, 6), y_import,
        insn_ad(OP_NEWTABLE, 1, 0), 0u32,
        insn_abc(OP_SETTABLEKS, 3, 1, 0), 7u32, // K[7] = "Gui"
        insn_abc(OP_SETTABLEKS, 4, 1, 0), 8u32, // K[8] = "Framework"
        insn_abc(OP_CALL, 0, 2, 2),
        insn_abc(OP_RETURN, 0, 2, 0),
    ];
    let constants = vec![
        Constant::Import(require_import),         // K[0]
        Constant::String("require".into()),       // K[1]
        Constant::Import(x_import),               // K[2]
        Constant::String("script".into()),        // K[3]
        Constant::String("X".into()),             // K[4]
        Constant::String("Y".into()),             // K[5]
        Constant::Import(y_import),               // K[6]
        Constant::String("Gui".into()),           // K[7]
        Constant::String("Framework".into()),     // K[8]
    ];
    let strings = vec![
        "require".into(), "script".into(),
        "X".into(), "Y".into(),
        "Gui".into(), "Framework".into(),
    ];
    let out = decompile(&chunk(proto(code, constants), strings));
    // Multi-field wrapper must be preserved — at least one key appears.
    let has_gui = out.contains("Gui =") || out.contains("Gui=");
    let has_fw = out.contains("Framework =") || out.contains("Framework=");
    assert!(has_gui || has_fw,
        "multi-field wrapper must NOT be unwrapped — expected Gui or Framework field to survive:\n{}", out);
}
