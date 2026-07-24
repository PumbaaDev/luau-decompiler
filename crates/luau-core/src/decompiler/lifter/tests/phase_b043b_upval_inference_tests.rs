//! Extracted from lifter.rs as part of Phase B0.52P6 (part 3).
//! `super::super::` references the `lifter` module.

//! Phase B0.43B — expand `infer_main_proto_upval_names` with three new
//! patterns:
//!
//!   1. `SETGLOBAL R(A), "X"` following `GETUPVAL R(A), U(idx)`
//!      → upval idx is named "X" (the idiom `_G.X = upval` or just
//!      `X = upval`).
//!
//!   5. Roblox-instance NAMECALLs: `:Connect`, `:Fire`, `:FireServer`,
//!      `:InvokeServer`, `:FindFirstAncestor`, etc. → "signal", "remote",
//!      or "script"/"instance" as appropriate.
//!
//!   6. `require(upval)` (where `require` was loaded via GETIMPORT or
//!      GETGLOBAL) → upval is a ModuleScript, name it "module".
//!
//! These tests build minimal synthetic `Proto`s exercising each new
//! pattern and call `infer_main_proto_upval_names` directly.

use super::super::{infer_main_proto_upval_names, is_sane_identifier};
use crate::parser::types::{Constant, Proto};

// Canonical (non-shuffled) Luau v6 opcode bytes.
const OP_GETGLOBAL: u8   = 7;
const OP_SETGLOBAL: u8   = 8;
const OP_GETUPVAL: u8    = 9;
const OP_GETIMPORT: u8   = 12;
const OP_GETTABLEKS: u8  = 15;
const OP_NEWCLOSURE: u8  = 19;
const OP_NAMECALL: u8    = 20;
const OP_CALL: u8        = 21;
const OP_RETURN: u8      = 22;

fn insn_ad(op: u8, a: u8, d: i16) -> u32 {
    let du = d as u16 as u32;
    (op as u32) | ((a as u32) << 8) | (du << 16)
}
fn insn_abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
}

/// Pack a Luau import value: `count << 30 | id0 << 20 | id1 << 10 | id2`.
fn pack_import(ids: &[u32]) -> u32 {
    let count = (ids.len() as u32) & 0x3;
    let mut v = count << 30;
    if !ids.is_empty()   { v |= (ids[0] & 0x3FF) << 20; }
    if ids.len() >= 2    { v |= (ids[1] & 0x3FF) << 10; }
    if ids.len() >= 3    { v |=  ids[2] & 0x3FF;        }
    v
}

fn make_proto(code: Vec<u32>, constants: Vec<Constant>, num_upvalues: u8) -> Proto {
    Proto {
        max_stack_size: 16,
        num_params: 0,
        num_upvalues,
        is_vararg: false,
        flags: 0,
        typeinfo: None,
        code,
        constants,
        child_protos: Vec::new(),
        line_defined: 1,
        debug_name: None,
        line_info: None,
        debug_info: None,
    }
}

// ─── is_sane_identifier ──────────────────────────────────────────────

#[test]
fn b043b_is_sane_identifier_accepts_plain_names() {
    assert!(is_sane_identifier("Config"));
    assert!(is_sane_identifier("_private"));
    assert!(is_sane_identifier("MyVar2"));
    assert!(is_sane_identifier("x"));
}

#[test]
fn b043b_is_sane_identifier_rejects_bad_strings() {
    assert!(!is_sane_identifier(""));
    assert!(!is_sane_identifier("1foo"));      // starts with digit
    assert!(!is_sane_identifier("foo.bar"));   // dotted
    assert!(!is_sane_identifier("foo-bar"));   // hyphen
    assert!(!is_sane_identifier(" name"));     // leading space
}

// ─── Pattern 1: SETGLOBAL <- upval ───────────────────────────────────

#[test]
fn b043b_setglobal_from_upval_names_by_global_name() {
    // K[0] = "MyConfig" — the global being assigned to.
    //
    // Bytecode:
    //   0: GETUPVAL R0, U0      -- R0 = upval_0
    //   1: SETGLOBAL R0, K[0]   -- _G.MyConfig = R0
    //   2: AUX (hash, unused)
    //   3: RETURN R0, 1
    let code = vec![
        insn_abc(OP_GETUPVAL, 0, 0, 0),
        insn_ad(OP_SETGLOBAL, 0, 0),
        0,                                // AUX for SETGLOBAL
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    let constants = vec![Constant::String("MyConfig".to_string())];
    let proto = make_proto(code, constants, 1);
    let names = infer_main_proto_upval_names(&proto, &[]);
    assert_eq!(names.len(), 1);
    assert_eq!(names[0], "MyConfig",
        "SETGLOBAL <- GETUPVAL should name the upval by the global it's written to");
}

#[test]
fn b043b_setglobal_from_upval_does_not_fire_after_overwrite() {
    // If the upval register is OVERWRITTEN (by LOADN etc.) before the
    // SETGLOBAL, pattern 1 must not fire — otherwise we'd mis-name the
    // upval with an unrelated global.
    //
    // Bytecode:
    //   0: GETUPVAL R0, U0      -- R0 = upval_0
    //   1: LOADN    R0, 42      -- R0 = 42 (overwrite!)
    //   2: SETGLOBAL R0, K[0]   -- _G.Unrelated = 42
    //   3: AUX
    //   4: RETURN R0, 1
    let code = vec![
        insn_abc(OP_GETUPVAL, 0, 0, 0),
        insn_ad(4 /* LOADN */, 0, 42),
        insn_ad(OP_SETGLOBAL, 0, 0),
        0,
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    let constants = vec![Constant::String("Unrelated".to_string())];
    let proto = make_proto(code, constants, 1);
    let names = infer_main_proto_upval_names(&proto, &[]);
    assert_eq!(names.len(), 1);
    assert!(names[0].is_empty() || names[0] != "Unrelated",
        "overwrite must break the GETUPVAL → SETGLOBAL chain; got {:?}", names[0]);
}

#[test]
fn b043b_setglobal_rejects_non_identifier_names() {
    // If the resolved global name isn't a legal identifier (hash artifact,
    // path segment, etc.) we must fall back to no-name rather than
    // producing `upval_<garbage>`.
    //
    // Bytecode:
    //   0: GETUPVAL R0, U0
    //   1: SETGLOBAL R0, K[0]   -- K[0] = "not.an.ident"
    //   2: AUX
    //   3: RETURN
    let code = vec![
        insn_abc(OP_GETUPVAL, 0, 0, 0),
        insn_ad(OP_SETGLOBAL, 0, 0),
        0,
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    let constants = vec![Constant::String("not.an.ident".to_string())];
    let proto = make_proto(code, constants, 1);
    let names = infer_main_proto_upval_names(&proto, &[]);
    assert!(names[0].is_empty(),
        "non-identifier global name must not be used as upval name; got {:?}", names[0]);
}

// ─── Pattern 5: NAMECALL-based instance/signal/remote detection ──────

#[test]
fn b043b_namecall_connect_names_upval_as_signal() {
    // Bytecode:
    //   K[0] = "Connect"
    //   0: GETUPVAL R0, U0      -- R0 = upval_0
    //   1: NAMECALL R0, R0, ??  -- R0:Connect(...)
    //   2: AUX = K[0]           -- "Connect"
    //   3: CALL R0, 2, 1        -- (...) (just to be a valid shape)
    //   4: RETURN R0, 1
    let code = vec![
        insn_abc(OP_GETUPVAL, 0, 0, 0),
        insn_abc(OP_NAMECALL, 0, 0, 0),
        0,                                // AUX = 0 (K[0])
        insn_abc(OP_CALL, 0, 2, 1),
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    let constants = vec![Constant::String("Connect".to_string())];
    let proto = make_proto(code, constants, 1);
    let names = infer_main_proto_upval_names(&proto, &[]);
    assert_eq!(names[0], "signal",
        "`:Connect(...)` on an upval must name it \"signal\"");
}

#[test]
fn b043b_namecall_fireserver_names_upval_as_remote() {
    // K[0] = "FireServer"
    let code = vec![
        insn_abc(OP_GETUPVAL, 0, 0, 0),
        insn_abc(OP_NAMECALL, 0, 0, 0),
        0,
        insn_abc(OP_CALL, 0, 2, 1),
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    let constants = vec![Constant::String("FireServer".to_string())];
    let proto = make_proto(code, constants, 1);
    let names = infer_main_proto_upval_names(&proto, &[]);
    assert_eq!(names[0], "remote",
        "`:FireServer(...)` on an upval must name it \"remote\"");
}

#[test]
fn b043b_namecall_findfirstancestor_names_upval_as_instance() {
    // K[0] = "FindFirstAncestor"
    let code = vec![
        insn_abc(OP_GETUPVAL, 0, 0, 0),
        insn_abc(OP_NAMECALL, 0, 0, 0),
        0,
        insn_abc(OP_CALL, 0, 2, 2),
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    let constants = vec![Constant::String("FindFirstAncestor".to_string())];
    let proto = make_proto(code, constants, 1);
    let names = infer_main_proto_upval_names(&proto, &[]);
    // No field access was observed → we get "instance", not "script".
    assert_eq!(names[0], "instance",
        "`:FindFirstAncestor(...)` alone must name upval \"instance\" (no field-access evidence)");
}

#[test]
fn b043b_namecall_invokeserver_names_upval_as_remote() {
    // K[0] = "InvokeServer"
    let code = vec![
        insn_abc(OP_GETUPVAL, 0, 0, 0),
        insn_abc(OP_NAMECALL, 0, 0, 0),
        0,
        insn_abc(OP_CALL, 0, 2, 2),
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    let constants = vec![Constant::String("InvokeServer".to_string())];
    let proto = make_proto(code, constants, 1);
    let names = infer_main_proto_upval_names(&proto, &[]);
    assert_eq!(names[0], "remote",
        "`:InvokeServer(...)` on an upval must name it \"remote\"");
}

// ─── Pattern 6: require(upval) ───────────────────────────────────────

#[test]
fn b043b_require_upval_names_as_module_via_getimport() {
    // K[0] = "require" (string for import id0)
    // K[1] = Import([0])  — packed import referring to K[0]
    //
    // Bytecode:
    //   0: GETIMPORT R0, K[1]   -- R0 = require
    //   1: AUX = pack_import([0])
    //   2: GETUPVAL R1, U0      -- R1 = upval_0
    //   3: CALL R0, 2, 2        -- require(R1)
    //   4: RETURN R0, 1
    let code = vec![
        insn_ad(OP_GETIMPORT, 0, 1),  // D = 1 (K[1] = Import)
        pack_import(&[0]),            // AUX = packed import ids
        insn_abc(OP_GETUPVAL, 1, 0, 0),
        insn_abc(OP_CALL, 0, 2, 2),
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    let constants = vec![
        Constant::String("require".to_string()),
        Constant::Import(pack_import(&[0])),
    ];
    let proto = make_proto(code, constants, 1);
    let names = infer_main_proto_upval_names(&proto, &[]);
    assert_eq!(names[0], "module",
        "require(upval_0) via GETIMPORT should name upval_0 \"module\"");
}

#[test]
fn b043b_require_upval_names_as_module_via_getglobal() {
    // K[0] = "require" (string constant the GETGLOBAL reads from).
    //
    // Bytecode:
    //   0: GETGLOBAL R0, K[0]   -- R0 = require
    //   1: AUX (unused)
    //   2: GETUPVAL R1, U0      -- R1 = upval_0
    //   3: CALL R0, 2, 2        -- require(R1)
    //   4: RETURN R0, 1
    let code = vec![
        insn_ad(OP_GETGLOBAL, 0, 0),
        0,
        insn_abc(OP_GETUPVAL, 1, 0, 0),
        insn_abc(OP_CALL, 0, 2, 2),
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    let constants = vec![Constant::String("require".to_string())];
    let proto = make_proto(code, constants, 1);
    let names = infer_main_proto_upval_names(&proto, &[]);
    assert_eq!(names[0], "module",
        "require(upval_0) via GETGLOBAL should name upval_0 \"module\"");
}

#[test]
fn b043b_non_require_call_does_not_trigger_module_naming() {
    // A call like `print(upval_0)` must NOT name the upval "module" —
    // pattern 6 is restricted to `require`.
    //
    // Bytecode:
    //   0: GETGLOBAL R0, K[0]   -- R0 = print
    //   1: AUX
    //   2: GETUPVAL R1, U0
    //   3: CALL R0, 2, 1
    //   4: RETURN R0, 1
    let code = vec![
        insn_ad(OP_GETGLOBAL, 0, 0),
        0,
        insn_abc(OP_GETUPVAL, 1, 0, 0),
        insn_abc(OP_CALL, 0, 2, 1),
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    let constants = vec![Constant::String("print".to_string())];
    let proto = make_proto(code, constants, 1);
    let names = infer_main_proto_upval_names(&proto, &[]);
    assert_ne!(names[0], "module",
        "print(upval_0) must not trigger pattern 6 module naming; got {:?}", names[0]);
}

// ─── Existing patterns must still work (regression guards) ───────────

#[test]
fn b043b_getservice_still_names_upval_game() {
    // K[0] = "GetService"
    let code = vec![
        insn_abc(OP_GETUPVAL, 0, 0, 0),
        insn_abc(OP_NAMECALL, 0, 0, 0),
        0,                                 // AUX = K[0] = "GetService"
        insn_abc(OP_CALL, 0, 2, 2),
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    let constants = vec![Constant::String("GetService".to_string())];
    let proto = make_proto(code, constants, 1);
    let names = infer_main_proto_upval_names(&proto, &[]);
    assert_eq!(names[0], "game",
        ":GetService() detection must still produce \"game\"");
}

#[test]
fn b043b_parent_field_still_names_upval_script() {
    // K[0] = "Parent"
    let code = vec![
        insn_abc(OP_GETUPVAL, 0, 0, 0),
        insn_abc(OP_GETTABLEKS, 1, 0, 0),  // R1 = R0.Parent
        0,                                  // AUX = K[0] = "Parent"
        insn_abc(OP_RETURN, 1, 2, 0),
    ];
    let constants = vec![Constant::String("Parent".to_string())];
    let proto = make_proto(code, constants, 1);
    let names = infer_main_proto_upval_names(&proto, &[]);
    assert_eq!(names[0], "script",
        ".Parent detection must still produce \"script\"");
}

// ─── Edge: no upvalues → empty result ────────────────────────────────

#[test]
fn b043b_no_upvalues_returns_empty() {
    let code = vec![
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    let proto = make_proto(code, Vec::new(), 0);
    let names = infer_main_proto_upval_names(&proto, &[]);
    assert!(names.is_empty());
}

// ─── Pattern-1 priority: do not clobber stronger inferences ──────────

#[test]
fn b043b_setglobal_does_not_override_getservice_game() {
    // Even if SETGLOBAL happens later, the `:GetService` signal wins
    // because pattern 1 is checked AFTER the service/script/remote/signal
    // heuristics.
    //
    // K[0] = "GetService"
    // K[1] = "MyShadowName"
    //
    // Bytecode:
    //   0: GETUPVAL   R0, U0
    //   1: NAMECALL   R0, R0, ?   -- R0:GetService(...)
    //   2: AUX = K[0]              -- "GetService"
    //   3: CALL       R0, 2, 2
    //   4: GETUPVAL   R1, U0
    //   5: SETGLOBAL  R1, K[1]     -- MyShadowName = R1
    //   6: AUX
    //   7: RETURN R1, 1
    let code = vec![
        insn_abc(OP_GETUPVAL, 0, 0, 0),
        insn_abc(OP_NAMECALL, 0, 0, 0),
        0,
        insn_abc(OP_CALL, 0, 2, 2),
        insn_abc(OP_GETUPVAL, 1, 0, 0),
        insn_ad(OP_SETGLOBAL, 1, 1),
        0,
        insn_abc(OP_RETURN, 1, 1, 0),
    ];
    let constants = vec![
        Constant::String("GetService".to_string()),
        Constant::String("MyShadowName".to_string()),
    ];
    let proto = make_proto(code, constants, 1);
    let names = infer_main_proto_upval_names(&proto, &[]);
    assert_eq!(names[0], "game",
        "`:GetService` must outrank pattern-1 SETGLOBAL; got {:?}", names[0]);
}

// ─── Multi-upval disambiguation ──────────────────────────────────────

#[test]
fn b043b_require_names_only_the_argument_upval() {
    // Two upvals: require(upval_1) but NOT upval_0.
    // K[0] = "require"
    //
    // Bytecode:
    //   0: GETGLOBAL R0, K[0]     -- R0 = require
    //   1: AUX
    //   2: GETUPVAL  R1, U1       -- R1 = upval_1 (the arg)
    //   3: CALL      R0, 2, 2     -- require(R1)
    //   4: RETURN    R0, 1
    let code = vec![
        insn_ad(OP_GETGLOBAL, 0, 0),
        0,
        insn_abc(OP_GETUPVAL, 1, 1, 0),
        insn_abc(OP_CALL, 0, 2, 2),
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    let constants = vec![Constant::String("require".to_string())];
    let proto = make_proto(code, constants, 2);
    let names = infer_main_proto_upval_names(&proto, &[]);
    assert_eq!(names.len(), 2);
    assert_eq!(names[1], "module",
        "upval_1 (require arg) must be named \"module\"; got {:?}", names[1]);
    // upval_0 was never touched → must remain unnamed.
    assert!(names[0].is_empty(),
        "upval_0 (not referenced) must remain unnamed; got {:?}", names[0]);
}

// ─── DupClosure does not leak an upval signal into pattern 1 ─────────

#[test]
fn b043b_newclosure_clears_reg_state() {
    // If a GETUPVAL is followed by NEWCLOSURE into the same reg, the
    // register no longer holds the upval — a later SETGLOBAL must NOT
    // treat R0 as still holding upval_0.
    //
    // K[0] = "WouldBeWrongName"
    //
    // Bytecode:
    //   0: GETUPVAL   R0, U0
    //   1: NEWCLOSURE R0, P0    -- R0 = closure (overwrite)
    //   2: SETGLOBAL  R0, K[0]  -- Global = closure, NOT upval_0
    //   3: AUX
    //   4: RETURN
    let code = vec![
        insn_abc(OP_GETUPVAL, 0, 0, 0),
        insn_ad(OP_NEWCLOSURE, 0, 0),
        insn_ad(OP_SETGLOBAL, 0, 0),
        0,
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    let constants = vec![Constant::String("WouldBeWrongName".to_string())];
    let proto = make_proto(code, constants, 1);
    let names = infer_main_proto_upval_names(&proto, &[]);
    assert!(names[0].is_empty() || names[0] != "WouldBeWrongName",
        "NEWCLOSURE must invalidate pattern-1 tracking; got {:?}", names[0]);
}
