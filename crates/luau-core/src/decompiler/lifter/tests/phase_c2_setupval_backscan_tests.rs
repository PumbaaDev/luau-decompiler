//! Phase C2 pass #1 — SETUPVAL upval backscan tests.
//!
//! These tests drive the full lifter end-to-end so that the SETUPVAL
//! opcode handler actually runs, then inspect `ctx.inferred_upvalue_names`
//! to prove that `SETUPVAL U(i) = R(a)` — where R(a) carries a meaningful
//! `Expr::Name` — propagates the name into the per-proto upvalue-name map
//! (and into child closures via `upval_parent_links`).
//!
//! Test shapes:
//!   1. `c2_single_setupval_names_the_upvalue` — one GETIMPORT + SETUPVAL
//!      installs the import's name onto the upval slot.
//!   2. `c2_nested_closure_inherits_upval_name_via_parent_link` — parent
//!      SETUPVALs a name, the child's `upval_parent_links` entry
//!      propagates the name onto the child's slot.
//!   3. `c2_setupval_first_name_wins_on_collision` — two SETUPVALs store
//!      different Names into the same upval slot; the first is retained.
//!
//! Note: a separate pre-scan pass (`infer_upval_names_from_setupval`) also
//! populates these names; these tests still hold because the lifter-time
//! handler path is idempotent with (and strictly first-wins vs) that pass.

use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, Constant, Proto};

// Canonical Luau v6 opcode bytes (identity opmap, matching Chunk.version = 6).
const OP_LOADN: u8     = 4;
const OP_LOADK: u8     = 5;
const OP_MOVE: u8      = 6;
const OP_GETUPVAL: u8  = 9;
const OP_SETUPVAL: u8  = 10;
const OP_GETIMPORT: u8 = 12;
const OP_NEWCLOSURE: u8 = 19;
const OP_RETURN: u8    = 22;
const OP_CAPTURE: u8   = 70;

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
        debug_name: Some("c2_test".to_string()),
        line_info: None,
        debug_info: None,
    }
}

/// Build a trivial child proto that captures `num_upvalues` upvalues and
/// just returns. We don't care what the child emits — we only need it to
/// register `upval_parent_links` via its CAPTUREs in the parent.
fn make_child(num_upvalues: u8) -> Proto {
    Proto {
        max_stack_size: 2,
        num_params: 0,
        num_upvalues,
        is_vararg: false,
        flags: 0,
        typeinfo: None,
        code: vec![insn_abc(OP_RETURN, 0, 1, 0)],
        constants: Vec::new(),
        child_protos: Vec::new(),
        line_defined: 1,
        debug_name: Some("child".to_string()),
        line_info: None,
        debug_info: None,
    }
}

// ─── Test 1: single SETUPVAL names the upvalue ───────────────────────

#[test]
fn c2_single_setupval_names_the_upvalue() {
    // K[0] = "game"   (Constant::Import with id0=0 referencing K[0]="game")
    // K[1] = "game" (string)
    //
    // Bytecode:
    //   0: GETIMPORT R0, K[0]   AUX=import(id0=1)  → R0 = game
    //   2: SETUPVAL   R0, U0     → upval 0 := R0 (the name "game")
    //   3: RETURN R0, 1
    //
    // "game" is a stdlib shadow name, so use a custom identifier instead.
    // K[1] = "MyConfig" (plain identifier, not shadowed)
    let import_val = pack_import(&[1]);
    let code = vec![
        insn_ad(OP_GETIMPORT, 0, 0),
        import_val,
        insn_abc(OP_SETUPVAL, 0, 0, 0),
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    let constants = vec![
        Constant::Import(import_val),
        Constant::String("MyConfig".to_string()),
    ];
    let proto = make_proto(code, constants, 1);

    let chunk = Chunk {
        version: 6,
        types_version: 0,
        strings: vec!["MyConfig".to_string()],
        protos: vec![proto],
        main_proto: 0,
    };

    let mut ctx = DecompileContext::new(&chunk);
    let _ = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    let names = ctx
        .inferred_upvalue_names
        .get(&0)
        .cloned()
        .unwrap_or_default();
    assert!(!names.is_empty(), "expected inferred_upvalue_names entry for proto 0");
    assert_eq!(
        names[0], "MyConfig",
        "SETUPVAL R0,U0 with R0 holding Name(\"MyConfig\") must install MyConfig on upval slot 0 (got {:?})",
        names
    );
}

// ─── Test 2: nested closure inherits via upval_parent_links ───────────

#[test]
fn c2_nested_closure_inherits_upval_name_via_parent_link() {
    // Parent proto (0) has 1 upvalue (its own "MyConfig" — installed by
    // SETUPVAL from a named register).  Parent creates a child closure
    // via NEWCLOSURE followed by a CAPTURE type 2 (upval) that captures
    // parent's upval 0.  The child has 1 upvalue.  We expect the child's
    // inferred_upvalue_names[0] to become "MyConfig" as well, via the
    // `upval_parent_links` handoff inside the SETUPVAL handler.
    //
    // Critical: SETUPVAL in the parent must happen AFTER the NEWCLOSURE
    // so the child's upval_parent_links entry is already registered when
    // the handler fires.  (Before NEWCLOSURE, the link doesn't exist yet
    // and propagation would no-op — that's fine for correctness, it just
    // means the test shape needs SETUPVAL after NEWCLOSURE.)
    //
    // Bytecode for parent:
    //   0: GETIMPORT R0, K[0]   AUX=import(id0=1=K[1]="MyConfig") → R0 = MyConfig
    //   2: NEWCLOSURE R1, 0     (child idx 0 in child_protos)
    //   3: CAPTURE  type=2, slot=0  (child captures parent's upval 0)
    //   4: SETUPVAL R0, U0      (install "MyConfig" on parent's upval 0)
    //   5: RETURN R1, 2
    //
    // CAPTURE encoding: LOP_CAPTURE A=type, B=slot. Type 2 = upval.
    let import_val = pack_import(&[1]);
    let parent_code = vec![
        insn_ad(OP_GETIMPORT, 0, 0),
        import_val,
        insn_ad(OP_NEWCLOSURE, 1, 0),
        insn_abc(OP_CAPTURE, 2, 0, 0),
        insn_abc(OP_SETUPVAL, 0, 0, 0),
        insn_abc(OP_RETURN, 1, 2, 0),
    ];
    let parent_constants = vec![
        Constant::Import(import_val),
        Constant::String("MyConfig".to_string()),
    ];
    let mut parent = make_proto(parent_code, parent_constants, 1);
    parent.child_protos = vec![1]; // Reference child proto in chunk.protos[1]

    let child = make_child(1);

    let chunk = Chunk {
        version: 6,
        types_version: 0,
        strings: vec!["MyConfig".to_string()],
        protos: vec![parent, child],
        main_proto: 0,
    };

    let mut ctx = DecompileContext::new(&chunk);
    let _ = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    // Parent's upval should be named MyConfig.
    let parent_names = ctx
        .inferred_upvalue_names
        .get(&0)
        .cloned()
        .unwrap_or_default();
    assert!(
        parent_names.get(0).map(|s| s.as_str()) == Some("MyConfig"),
        "parent proto 0 upval 0 should be MyConfig, got {:?}",
        parent_names
    );

    // Child's upval should have inherited the name via upval_parent_links.
    // Note: the child proto is at index 1 in chunk.protos, but its
    // upval_parent_links key is the child's proto index (1).
    let child_names = ctx
        .inferred_upvalue_names
        .get(&1)
        .cloned()
        .unwrap_or_default();
    assert!(
        child_names.get(0).map(|s| s.as_str()) == Some("MyConfig"),
        "child proto 1 upval 0 should have inherited MyConfig via upval_parent_links, got {:?}; \
         parent_links = {:?}",
        child_names,
        ctx.upval_parent_links.get(&1)
    );
}

// ─── Test 3: two conflicting SETUPVALs — first wins ───────────────────

#[test]
fn c2_setupval_first_name_wins_on_collision() {
    // Parent proto has 1 upvalue. Two successive SETUPVALs store DIFFERENT
    // Named registers into the same upval slot. First must win (the store
    // at index 0 writes "FirstName", later store at index 2 would try to
    // write "SecondName" but the slot is already taken).
    //
    // K[0] = Constant::Import(id0=1)
    // K[1] = "FirstName"
    // K[2] = Constant::Import(id0=3)
    // K[3] = "SecondName"
    //
    // Bytecode:
    //   0: GETIMPORT R0, K[0]   AUX=import(id0=1)   → R0 = FirstName
    //   2: SETUPVAL  R0, U0                           → upval 0 := "FirstName"
    //   3: GETIMPORT R0, K[2]   AUX=import(id0=3)   → R0 = SecondName
    //   5: SETUPVAL  R0, U0                           → upval 0 := "SecondName" (IGNORED)
    //   6: RETURN R0, 1
    let import0 = pack_import(&[1]);
    let import1 = pack_import(&[3]);
    let code = vec![
        insn_ad(OP_GETIMPORT, 0, 0),
        import0,
        insn_abc(OP_SETUPVAL, 0, 0, 0),
        insn_ad(OP_GETIMPORT, 0, 2),
        import1,
        insn_abc(OP_SETUPVAL, 0, 0, 0),
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    let constants = vec![
        Constant::Import(import0),
        Constant::String("FirstName".to_string()),
        Constant::Import(import1),
        Constant::String("SecondName".to_string()),
    ];
    let proto = make_proto(code, constants, 1);

    let chunk = Chunk {
        version: 6,
        types_version: 0,
        strings: vec!["FirstName".to_string(), "SecondName".to_string()],
        protos: vec![proto],
        main_proto: 0,
    };

    let mut ctx = DecompileContext::new(&chunk);
    let _ = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    let names = ctx
        .inferred_upvalue_names
        .get(&0)
        .cloned()
        .unwrap_or_default();
    assert!(!names.is_empty(), "expected inferred_upvalue_names entry for proto 0");
    assert_eq!(
        names[0], "FirstName",
        "first SETUPVAL must win on collision (got {:?})",
        names
    );
}

// Silence unused-const warnings in the test module — not every constant
// above is consumed by every test, but they're grouped together for
// documentation.
#[allow(dead_code)]
const _UNUSED_WARN_SILENCER: [u8; 4] = [
    OP_LOADN, OP_LOADK, OP_MOVE, OP_GETUPVAL,
];
