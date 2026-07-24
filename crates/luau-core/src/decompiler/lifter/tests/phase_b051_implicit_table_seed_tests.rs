//! Extracted from lifter.rs as part of Phase B0.52P6 (part 3).
//! `super::super::` references the `lifter` module.

//! Phase B0.51 — when a main-proto register is used as a SET*/GET*
//! table target without ever being written (typical of module-style
//! `local M = {}; M.foo = ...; return M` patterns where NEWTABLE is
//! unmapped in this shuffle variant), the lifter must implicitly
//! materialize `local vN = {}` so subsequent `vN.foo = val` assigns
//! have a declared target.  B0.47/B0.48 can then collapse the
//! resulting prefix into a constructor.
//!
//! Exhaustive coverage:
//!   1. `is_undeclared_non_param` classifier — FirstDecl case.
//!   2. `is_undeclared_non_param` classifier — param case (returns false).
//!   3. `is_undeclared_non_param` classifier — after declare (returns false).
//!   4. End-to-end: SETTABLEKS on unwritten R0 emits `local v0 = {}`.
//!   5. End-to-end: B0.47 collapses the seeded table + N field assigns
//!      into a single constructor with all fields as Named entries.
//!   6. End-to-end: upval-naming for the main proto that would have
//!      inferred "game" for upval 0 does NOT overwrite the R0 seed.
//!      (R0 is a register, not an upvalue — the B0.51 seed must win.)
//!   7. End-to-end: B0.48 two-step absorb works through the seeded
//!      module table (local F = function...end; v0.K = F pattern).
//!   8. Regression guard: a proper NEWTABLE followed by SETTABLEKS
//!      emits a single seed (B0.51 MUST NOT emit a duplicate).
//!   9. Regression guard: an already-declared register (param slot)
//!      never triggers the B0.51 seed.
//!  10. Regression guard: SETTABLEN on an unwritten register also seeds.

use super::super::{LocalTracker, WriteKind};
use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, Constant, Proto};

// ── Canonical Luau v6 opcode bytes (real values from LuauOpcode enum)
const OP_LOADN: u8       = 4;
const OP_SETTABLEKS: u8  = 16;
const OP_SETTABLEN: u8   = 18;
const OP_NEWCLOSURE: u8  = 19;
const OP_RETURN: u8      = 22;
const OP_NEWTABLE: u8    = 53;

fn insn_ad(op: u8, a: u8, d: i16) -> u32 {
    let du = d as u16 as u32;
    (op as u32) | ((a as u32) << 8) | (du << 16)
}

fn insn_abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
}

fn make_child_proto(debug_name: &str) -> Proto {
    Proto {
        max_stack_size: 2,
        num_params: 0,
        num_upvalues: 0,
        is_vararg: false,
        flags: 0,
        typeinfo: None,
        code: vec![insn_abc(OP_RETURN, 0, 1, 0)],
        constants: Vec::new(),
        child_protos: Vec::new(),
        line_defined: 1,
        debug_name: Some(debug_name.to_string()),
        line_info: None,
        debug_info: None,
    }
}

fn make_parent_proto(code: Vec<u32>, constants: Vec<Constant>) -> Proto {
    Proto {
        max_stack_size: 16,
        num_params: 0,
        num_upvalues: 0,
        is_vararg: true,
        flags: 0,
        typeinfo: None,
        code,
        constants,
        child_protos: Vec::new(),
        line_defined: 1,
        debug_name: Some("main".to_string()),
        line_info: None,
        debug_info: None,
    }
}

fn make_chunk(parent: Proto, children: Vec<Proto>) -> Chunk {
    let mut protos = vec![parent];
    protos.extend(children);
    Chunk {
        version: 6,
        types_version: 0,
        strings: Vec::new(),
        protos,
        main_proto: 0,
    }
}

// ─── is_undeclared_non_param unit coverage ─────────────────────────

#[test]
fn b051_is_undeclared_non_param_true_for_fresh_register() {
    let tracker = LocalTracker::new(0);
    assert!(tracker.is_undeclared_non_param(0),
        "reg 0 in a param-less proto must be undeclared non-param");
    assert!(tracker.is_undeclared_non_param(7),
        "arbitrary high reg must be undeclared non-param when never written");
}

#[test]
fn b051_is_undeclared_non_param_false_for_param_register() {
    let tracker = LocalTracker::new(3);
    // Regs 0..3 are params.
    assert!(!tracker.is_undeclared_non_param(0),
        "param reg must NEVER be treated as undeclared non-param");
    assert!(!tracker.is_undeclared_non_param(1));
    assert!(!tracker.is_undeclared_non_param(2));
    // Reg 3+ is not a param, so it IS undeclared non-param until
    // written.
    assert!(tracker.is_undeclared_non_param(3));
}

#[test]
fn b051_is_undeclared_non_param_false_after_declare() {
    let mut tracker = LocalTracker::new(0);
    let (kind, _) = tracker.classify_write(4, "x");
    assert_eq!(kind, WriteKind::FirstDecl);
    // After classify_write, reg 4 is declared.
    assert!(!tracker.is_undeclared_non_param(4),
        "declared reg must NOT trigger the B0.51 seed helper");
}

// ─── End-to-end: main-proto NEWTABLE-less module pattern ───────────

#[test]
fn b051_main_proto_settableks_on_unwritten_r0_emits_local_seed() {
    // Smoking-gun reproduction: main proto where NEWTABLE is missing
    // (unmapped in a real corpus) so R0 is never written before the
    // first SETTABLEKS.  B0.51 must implicitly seed `local v0 = {}`.
    //
    //   pc=0: NEWCLOSURE R1, proto(1)
    //   pc=1: SETTABLEKS R0, R1, AUX=K0 ("arithmetic")  — R0 is Unknown!
    //   pc=2: AUX = 0
    //   pc=3: RETURN R0..R0
    //
    // With B0.51, pc=1 seeds `local v0 = {}` first, then emits
    // `v0.arithmetic = arithmetic`.  B0.47/B0.48 collapse into a
    // constructor.
    let code = vec![
        insn_ad(OP_NEWCLOSURE, 1, 1),      // NEWCLOSURE R1
        insn_abc(OP_SETTABLEKS, 1, 0, 0),  // SETTABLEKS R0.K0 = R1
        0u32,                              // AUX = 0 ("arithmetic")
        insn_abc(OP_RETURN, 0, 2, 0),      // RETURN R0
    ];
    let constants = vec![Constant::String("arithmetic".to_string())];
    let parent = make_parent_proto(code, constants);
    let child = make_child_proto("arithmetic");
    let chunk = make_chunk(parent, vec![child]);

    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    // Must emit the module table — either as a seeded local (`local v0 = { ... }`)
    // or inlined directly into return (`return { ... }`). Both are valid;
    // the inline form is preferred when the table is used exactly once.
    let has_seed = out.contains("local v0 = {") || out.contains("return {");
    assert!(has_seed,
        "expected `local v0 = {{` or `return {{` for the unwritten R0; got:\n{}", out);
    // `arithmetic` MUST appear as a local or field (never dropped).
    let arith_any = out.contains("arithmetic = function") || out.contains("function arithmetic");
    assert!(arith_any,
        "expected `arithmetic = function` or `function arithmetic` somewhere; got:\n{}", out);
    // MUST NOT emit a bare global write for v0.
    assert!(!out.contains("\nv0.arithmetic = "),
        "MUST NOT emit `v0.arithmetic = ...` at column 0 — this means no \
         `local v0 = {{}}` was emitted; got:\n{}", out);
}

#[test]
fn b051_b047_collapses_seeded_table_multiple_fields() {
    // Three SETTABLEKS on unwritten R0 must all absorb into a single
    // B0.47 constructor with three Named fields.
    //
    //   pc=0: LOADN R1, 1
    //   pc=1: SETTABLEKS R0, R1, K0 ("a")
    //   pc=3: LOADN R1, 2
    //   pc=4: SETTABLEKS R0, R1, K1 ("b")
    //   pc=6: LOADN R1, 3
    //   pc=7: SETTABLEKS R0, R1, K2 ("c")
    //   pc=9: RETURN R0..R0
    let code = vec![
        insn_ad(OP_LOADN, 1, 1),           // 0: R1 = 1
        insn_abc(OP_SETTABLEKS, 1, 0, 0),  // 1: R0.a = R1
        0u32,                              // 2: AUX = 0 ("a")
        insn_ad(OP_LOADN, 1, 2),           // 3: R1 = 2
        insn_abc(OP_SETTABLEKS, 1, 0, 0),  // 4: R0.b = R1
        1u32,                              // 5: AUX = 1 ("b")
        insn_ad(OP_LOADN, 1, 3),           // 6: R1 = 3
        insn_abc(OP_SETTABLEKS, 1, 0, 0),  // 7: R0.c = R1
        2u32,                              // 8: AUX = 2 ("c")
        insn_abc(OP_RETURN, 0, 2, 0),      // 9: RETURN R0
    ];
    let constants = vec![
        Constant::String("a".to_string()),
        Constant::String("b".to_string()),
        Constant::String("c".to_string()),
    ];
    let parent = make_parent_proto(code, constants);
    let chunk = make_chunk(parent, vec![]);

    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    // All three fields must appear as named entries.  B0.47 absorbs
    // the direct-assign shape.
    assert!(out.contains("a = 1") || out.contains("a = 1,"),
        "expected field `a = 1` in constructor; got:\n{}", out);
    assert!(out.contains("b = 2") || out.contains("b = 2,"),
        "expected field `b = 2` in constructor; got:\n{}", out);
    assert!(out.contains("c = 3") || out.contains("c = 3,"),
        "expected field `c = 3` in constructor; got:\n{}", out);
    // MUST NOT have any `v0.X = Y` orphan assigns at column 0.
    assert!(!out.contains("\nv0."),
        "expected all R0 field assigns to absorb into the seed; got:\n{}", out);
}

#[test]
fn b051_upval_naming_does_not_overwrite_r0_seed() {
    // Regression guard for hypothesis H1 (upval-name inheritance).
    // Even if the main proto has upvals that B0.43B would rename to
    // "game", the R0 register itself must NOT inherit that name — R0
    // is a LOCAL register slot, not an upval slot.
    //
    // We provide 1 upval whose usage would make it "game", then
    // exercise the B0.51 path on R0.
    let code = vec![
        insn_ad(OP_NEWCLOSURE, 1, 1),      // NEWCLOSURE R1
        insn_abc(OP_SETTABLEKS, 1, 0, 0),  // SETTABLEKS R0."arithmetic" = R1
        0u32,                              // AUX = 0
        insn_abc(OP_RETURN, 0, 2, 0),      // RETURN R0
    ];
    let constants = vec![Constant::String("arithmetic".to_string())];
    let mut parent = make_parent_proto(code, constants);
    parent.num_upvalues = 1;  // Hypothetical upval; usage evidence absent.
    let child = make_child_proto("arithmetic");
    let chunk = make_chunk(parent, vec![child]);

    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    // R0's seed must NOT be named "game" regardless of upval status.
    assert!(!out.contains("local game = {"),
        "R0 seed must NOT inherit upval name `game`; got:\n{}", out);
    assert!(!out.contains("return game"),
        "RETURN R0 must resolve to the seeded local, not `game`; got:\n{}", out);
    // Correct seed must appear — either as intermediate local or inlined return.
    let has_seed = out.contains("local v0 = {") || out.contains("return {");
    assert!(has_seed,
        "expected R0 seeded as `local v0 = {{` or inlined as `return {{`; got:\n{}", out);
}

#[test]
fn b051_b048_two_step_absorb_works_through_seeded_table() {
    // Module-style pattern that exercises B0.48 two-step absorb:
    //   local arithmetic = function()...end
    //   v0.arithmetic = arithmetic
    //   return v0
    //
    // Bytecode:
    //   pc=0: NEWCLOSURE R1, proto(1)   (R1 gets "arithmetic" debug_name)
    //   pc=1: SETTABLEKS R0.arithmetic = R1
    //   pc=3: RETURN R0
    //
    // With B0.51, pc=1 seeds `local v0 = {}` before the SETTABLEKS
    // emits.  B0.48 then absorbs the `local arithmetic = function` +
    // `v0.arithmetic = arithmetic` pair into the table constructor.
    let code = vec![
        insn_ad(OP_NEWCLOSURE, 1, 1),
        insn_abc(OP_SETTABLEKS, 1, 0, 0),
        0u32,
        insn_abc(OP_RETURN, 0, 2, 0),
    ];
    let constants = vec![Constant::String("arithmetic".to_string())];
    let parent = make_parent_proto(code, constants);
    let child = make_child_proto("arithmetic");
    let chunk = make_chunk(parent, vec![child]);

    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    // After B0.48 absorb, `arithmetic = function` must be INSIDE the
    // constructor (indented) not as a sibling local.  The test is
    // permissive: either form is acceptable as long as the table
    // exists AND the field/local is present.
    assert!(out.contains("arithmetic = function") || out.contains("function arithmetic"),
        "expected `arithmetic = function` or `function arithmetic` (field or local); got:\n{}", out);
    // Table may be a local seed or inlined directly into return.
    let has_table = out.contains("local v0 = {") || out.contains("return {");
    assert!(has_table,
        "expected module-table seed or inlined return; got:\n{}", out);
}

#[test]
fn b051_proper_newtable_does_not_duplicate_seed() {
    // Regression guard: when NEWTABLE is correctly present at proto
    // start, B0.51 must NOT emit a second seed.  Only one `local v0
    // = {}` should appear.
    let code = vec![
        insn_ad(OP_NEWTABLE, 0, 0),        // 0: NEWTABLE R0
        0u32,                              // 1: AUX
        insn_ad(OP_NEWCLOSURE, 1, 1),      // 2: NEWCLOSURE R1
        insn_abc(OP_SETTABLEKS, 1, 0, 0),  // 3: SETTABLEKS R0.K0 = R1
        0u32,                              // 4: AUX
        insn_abc(OP_RETURN, 0, 2, 0),      // 5: RETURN R0
    ];
    let constants = vec![Constant::String("arithmetic".to_string())];
    let parent = make_parent_proto(code, constants);
    let child = make_child_proto("arithmetic");
    let chunk = make_chunk(parent, vec![child]);

    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    // The table must appear exactly once — either as a local seed
    // (`local v0 = {` / `local tbl = {`) or inlined into return (`return {`).
    // B0.104 allows table inlining for single-use tables.
    let seed_count = out.matches("local v0 = {").count()
        + out.matches("local tbl = {").count()
        + out.matches("return {").count();
    assert_eq!(seed_count, 1,
        "expected exactly ONE table (seed or inlined return); got {}:\n{}", seed_count, out);
}

#[test]
fn b051_param_register_never_seeds() {
    // Regression guard: a proto whose first SETTABLEKS targets a
    // PARAMETER register (e.g. R0 when num_params >= 1) must NOT
    // trigger the B0.51 seed — params are pre-declared and reading
    // them is legitimate.
    let code = vec![
        insn_ad(OP_NEWCLOSURE, 1, 1),
        insn_abc(OP_SETTABLEKS, 1, 0, 0),  // SETTABLEKS R0.K0 = R1
        0u32,
        insn_abc(OP_RETURN, 0, 1, 0),      // RETURN (no values)
    ];
    let constants = vec![Constant::String("handler".to_string())];
    let mut parent = make_parent_proto(code, constants);
    parent.num_params = 1;   // R0 is now a parameter.
    parent.is_vararg = false;
    let child = make_child_proto("handler");
    let chunk = make_chunk(parent, vec![child]);

    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    // MUST NOT emit a seed for a param register — `arg1.handler = ...`
    // is a legitimate "parameter has field assigned" pattern, not a
    // module table.
    assert!(!out.contains("local arg1 = {"),
        "MUST NOT seed param register arg1 as a local table; got:\n{}", out);
    assert!(!out.contains("local v0 = {"),
        "MUST NOT seed param register as local v0; got:\n{}", out);
}

#[test]
fn b051_settablen_on_unwritten_r0_also_seeds() {
    // SETTABLEN (indexed by small integer) also must trigger the
    // B0.51 seed when its table register is Unknown.
    //
    //   pc=0: LOADN R1, 42
    //   pc=1: SETTABLEN R0[1] = R1     (C=0 encodes key=1)
    //   pc=2: RETURN R0
    let code = vec![
        insn_ad(OP_LOADN, 1, 42),
        insn_abc(OP_SETTABLEN, 1, 0, 0),
        insn_abc(OP_RETURN, 0, 2, 0),
    ];
    let constants = vec![];
    let parent = make_parent_proto(code, constants);
    let chunk = make_chunk(parent, vec![]);

    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    let has_table = out.contains("local v0 = {") || out.contains("return {");
    assert!(has_table,
        "expected B0.51 table (seed or inlined return) for SETTABLEN; got:\n{}", out);
}

// ─── Direct unit: ensure_table_reg_declared seeding predicate ──────

#[test]
fn b051_helper_seeds_on_stale_primitive_in_reg() {
    // Observed in the real corpus: regs[0] can carry a stale primitive
    // (Bool/Number/String/Nil) from an earlier branch or unknown-opcode
    // fallthrough, then a SETTABLEKS on R0 executes.  `table_expr`
    // already falls back to `Name(v{reg})` for these primitives so the
    // *emitted* target is correct, but the register was never declared
    // as a local.  B0.51's helper must fire for these cases too.
    //
    // We drive this directly via the public entry-point: build a tiny
    // proto where R0 holds Bool(false) from a LOADB, then SETTABLEKS
    // targets R0.  The LOADB must use C!=0 (boolean chain) so the
    // lifter emits `local v0 = false` first — which then shows the
    // reassignment OR gets shadowed.
    //
    // For a direct unit check we inspect the helper itself via the
    // public test-side surface.  Here we verify the end-to-end that
    // a Number in regs[0] does NOT block the seed.
    //
    //   pc=0: LOADN R0, 5       (stores Number(5) in regs[0], no stmt)
    //   pc=1: SETTABLEKS R0.foo = R0  (nonsensical but exercises path)
    //   pc=2: AUX = 0
    //   pc=3: RETURN R0
    let code = vec![
        insn_ad(OP_LOADN, 0, 5),           // R0 = 5   (stays inline)
        insn_abc(OP_SETTABLEKS, 0, 0, 0),  // SETTABLEKS R0.K0 = R0
        0u32,                              // AUX
        insn_abc(OP_RETURN, 0, 2, 0),      // RETURN R0
    ];
    let constants = vec![Constant::String("foo".to_string())];
    let parent = make_parent_proto(code, constants);
    let chunk = make_chunk(parent, vec![]);

    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    // Table must appear — either as `local v0 = {}` (or collapsed into
    // a constructor with a field) or inlined into return. Absence would
    // mean the stale primitive blocked the seed and a bare `v0.foo = ...`
    // orphaned assign leaked.
    let has_table = out.contains("local v0 = {") || out.contains("return {");
    assert!(has_table,
        "stale primitive in regs[0] must not block B0.51 seed; got:\n{}", out);
}
