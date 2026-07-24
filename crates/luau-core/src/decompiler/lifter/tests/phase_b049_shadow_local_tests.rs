//! Extracted from lifter.rs as part of Phase B0.52P6 (part 3).
//! `super::super::` references the `lifter` module.

//! Phase B0.49 — shadow-declare a new `local` when a register is
//! reassigned with a NEW semantic name.
//!
//! Problem: Luau bytecode reuses registers across a proto.  Example from
//! `ModuleScript.lua`'s main proto:
//!
//!     pc=X  : R1 = NEWCLOSURE (debug_name "arithmetic")
//!     pc=X+1: SETTABLEKS R0, R1, "arithmetic"
//!     pc=X+2: R1 = NEWCLOSURE (debug_name "reverse_k_arith")
//!     pc=X+3: SETTABLEKS R0, R1, "reverse_k_arith"
//!
//! Before B0.49, the second write produced
//!     reverse_k_arith = function()...end
//! which Luau parses as a global assignment because `reverse_k_arith`
//! was never declared as a local.  B0.49 introduces `classify_write`,
//! which notices the NEW semantic name differs from the register's
//! currently-tracked local name and emits a shadowing `local` instead.
//!
//! Positive cases (must emit Shadow → `Stat::Local`):
//!   1. Two NEWCLOSUREs to the same register with distinct debug_names.
//!   2. Direct `classify_write` on a rename produces `Shadow`.
//!
//! Negative cases (must emit Reassign → `Stat::Assign`):
//!   3. Same name on both writes → Reassign (no extra local).
//!   4. Arithmetic self-mutation `count = count + 1` stays as Assign.
//!   5. Generic `v\d+` renames do NOT trigger Shadow (noise suppression).
//!   6. Unchanged register (no second write) stays a single Local.
//!   7. Parameter registers never shadow (classify always Reassign).
//!
//! Regression guards:
//!   8. B0.47/B0.48 table-constructor still folds after shadowing.
//!   9. Shadow local in B0.45A single-use inline path doesn't regress.
//!  10. `is_semantic_local_name` classifies generic vs semantic correctly.
//!  11. First-write on a fresh register is still FirstDecl.

use super::super::{LocalTracker, WriteKind, is_semantic_local_name};
use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, Constant, Proto};

// ── Canonical (non-shuffled) Luau v6 opcode bytes used by the end-to-end
//    integration tests below.  Mirrors the constants in Phase B0.3 and
//    B0.39B test modules.  Phase B0.51 corrected `OP_NEWTABLE` from 18
//    (which is actually SETTABLEN) to 53 (the real NewTable value in
//    `LuauOpcode`).  This means these end-to-end tests now genuinely
//    exercise the NEWTABLE → seed → SETTABLEKS pipeline, in addition to
//    the B0.51 "R(b) is Unknown, materialize it" safety net.
const OP_NEWTABLE: u8   = 53;
const OP_NEWCLOSURE: u8 = 19;
const OP_RETURN: u8     = 22;
const OP_ADDK: u8       = 39;
const OP_LOADN: u8      = 4;
const OP_SETTABLEKS: u8 = 16;

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

// ─── helper: is_semantic_local_name classifier ──────────────────────

#[test]
fn b049_is_semantic_local_name_classifies_generic_v_names() {
    // Pure `v\d+` is generic.
    assert!(!is_semantic_local_name("v0"));
    assert!(!is_semantic_local_name("v1"));
    assert!(!is_semantic_local_name("v12"));
    assert!(!is_semantic_local_name("v999"));
}

#[test]
fn b049_is_semantic_local_name_classifies_semantic_names() {
    // Anything that isn't strictly `v\d+` is considered semantic.
    assert!(is_semantic_local_name("arithmetic"));
    assert!(is_semantic_local_name("reverse_k_arith"));
    assert!(is_semantic_local_name("UserSettings"));
    assert!(is_semantic_local_name("count"));
    assert!(is_semantic_local_name("i"));
    assert!(is_semantic_local_name("self"));
    assert!(is_semantic_local_name("arg1"));
    assert!(is_semantic_local_name("tbl"));
    assert!(is_semantic_local_name("fn"));
    // Edge: "v" alone (no digits) is semantic — it's a valid id and
    // wasn't necessarily produced by the `v\d+` fallback.
    assert!(is_semantic_local_name("v"));
    // Edge: letter after digits → semantic
    assert!(is_semantic_local_name("v1x"));
    assert!(is_semantic_local_name("value"));
    // Empty → not semantic (defensive)
    assert!(!is_semantic_local_name(""));
}

// ─── classify_write unit tests ─────────────────────────────────────

#[test]
fn b049_classify_write_first_decl_on_fresh_register() {
    let mut tracker = LocalTracker::new(0);
    let (kind, name) = tracker.classify_write(5, "arithmetic");
    assert_eq!(kind, WriteKind::FirstDecl);
    assert_eq!(name, "arithmetic");
}

#[test]
fn b049_classify_write_shadow_on_semantic_rename() {
    // R1 first written as "arithmetic" then as "reverse_k_arith" —
    // second write must be Shadow, not Reassign.
    let mut tracker = LocalTracker::new(0);
    let _ = tracker.classify_write(1, "arithmetic");
    let (kind, name) = tracker.classify_write(1, "reverse_k_arith");
    assert_eq!(kind, WriteKind::Shadow,
        "semantic-rename must trigger Shadow");
    assert_eq!(name, "reverse_k_arith");
}

#[test]
fn b049_classify_write_reassign_on_same_name() {
    // Same name → Reassign, no extra local.
    let mut tracker = LocalTracker::new(0);
    let _ = tracker.classify_write(1, "count");
    let (kind, name) = tracker.classify_write(1, "count");
    assert_eq!(kind, WriteKind::Reassign,
        "identical-name write must be a plain reassignment");
    assert_eq!(name, "count");
}

#[test]
fn b049_classify_write_suppresses_shadow_on_generic_rename() {
    // `v1` → `v12` (both generic) must NOT shadow — this is the
    // Phase B0.44A regression trap we must avoid.
    let mut tracker = LocalTracker::new(0);
    let (k1, _) = tracker.classify_write(3, "v3");
    assert_eq!(k1, WriteKind::FirstDecl);
    let (k2, n2) = tracker.classify_write(3, "v12");
    assert_eq!(k2, WriteKind::Reassign,
        "generic v\\d+ rename must not shadow");
    assert_eq!(n2, "v3",
        "Reassign must use the EXISTING declared name, not the new generic");
}

#[test]
fn b049_classify_write_suppresses_shadow_when_one_side_generic() {
    // Semantic → generic must stay Reassign using the semantic name
    // (otherwise we'd globally-write to a churned v{N}).
    let mut tracker = LocalTracker::new(0);
    let _ = tracker.classify_write(2, "arithmetic");
    let (kind, name) = tracker.classify_write(2, "v7");
    assert_eq!(kind, WriteKind::Reassign);
    assert_eq!(name, "arithmetic",
        "must keep the semantic name even when new hint is generic");
}

#[test]
fn b049_classify_write_params_never_shadow() {
    // Parameter registers always reassign — the register IS the param.
    let mut tracker = LocalTracker::new(2);
    let (kind, name) = tracker.classify_write(0, "renamed_arg");
    assert_eq!(kind, WriteKind::Reassign,
        "param reg must never shadow");
    assert_eq!(name, "renamed_arg");
    // Second write still Reassign.
    let (kind2, _) = tracker.classify_write(0, "another");
    assert_eq!(kind2, WriteKind::Reassign);
}

#[test]
fn b049_classify_write_tracks_current_name_across_writes() {
    // After a Shadow, current_names must reflect the NEW name so the
    // subsequent Reassign uses it (not the pre-shadow name).
    let mut tracker = LocalTracker::new(0);
    let _ = tracker.classify_write(4, "first");
    let _ = tracker.classify_write(4, "second"); // Shadow
    let (kind, name) = tracker.classify_write(4, "second"); // same as post-shadow
    assert_eq!(kind, WriteKind::Reassign);
    assert_eq!(name, "second");
}

#[test]
fn b049_classify_write_third_shadow_chain() {
    // Three distinct semantic names → FirstDecl, Shadow, Shadow.
    let mut tracker = LocalTracker::new(0);
    let (k1, _) = tracker.classify_write(7, "alpha");
    let (k2, _) = tracker.classify_write(7, "beta");
    let (k3, n3) = tracker.classify_write(7, "gamma");
    assert_eq!(k1, WriteKind::FirstDecl);
    assert_eq!(k2, WriteKind::Shadow);
    assert_eq!(k3, WriteKind::Shadow);
    assert_eq!(n3, "gamma");
}

// ─── End-to-end: two NEWCLOSUREs with distinct debug_names ─────────

#[test]
fn b049_two_newclosures_with_distinct_debug_names_emit_shadow_local() {
    // This is the smoking-gun reproduction of the ModuleScript.lua bug.
    //
    // Parent main proto bytecode (opcode 53 = real NEWTABLE):
    //   0: NEWTABLE   R0, D=0      → local v0 = {}
    //   1: 0u32 (AUX for NEWTABLE)
    //   2: NEWCLOSURE R1, D=1      → local arithmetic = function()...end
    //   3: SETTABLEKS R0, R1 "arithmetic" AUX=K0
    //   4: AUX = 0
    //   5: NEWCLOSURE R1, D=2      → ***must emit local reverse_k_arith***
    //   6: SETTABLEKS R0, R1 "reverse_k_arith" AUX=K1
    //   7: AUX = 1
    //   8: RETURN R0, 2
    //
    // Before B0.49, pc=5's NEWCLOSURE wrote
    //     reverse_k_arith = function()...end
    // (global assignment — `reverse_k_arith` was never a declared local).
    // After B0.49 + B0.47/B0.48, this collapses into:
    //     local v0 = { arithmetic = function()...end,
    //                  reverse_k_arith = function()...end }
    //     return v0
    // which is the END-TO-END post-processed output.  The shadow-local
    // invariant we still guard: NO global write to `reverse_k_arith`.
    let code = vec![
        insn_ad(OP_NEWTABLE, 0, 0),        // NEWTABLE R0
        0u32,                              // NEWTABLE AUX (size hint)
        insn_ad(OP_NEWCLOSURE, 1, 1),      // NEWCLOSURE R1, child_proto[1]
        insn_abc(OP_SETTABLEKS, 1, 0, 0),  // SETTABLEKS R0, R1, K=0
        0u32,                              // AUX = const index 0 ("arithmetic")
        insn_ad(OP_NEWCLOSURE, 1, 2),      // NEWCLOSURE R1, child_proto[2]
        insn_abc(OP_SETTABLEKS, 1, 0, 0),  // SETTABLEKS R0, R1, K=1
        1u32,                              // AUX = const index 1 ("reverse_k_arith")
        insn_abc(OP_RETURN, 0, 2, 0),      // RETURN R0
    ];
    let constants = vec![
        Constant::String("arithmetic".to_string()),
        Constant::String("reverse_k_arith".to_string()),
    ];
    let parent = make_parent_proto(code, constants);
    let child_arith = make_child_proto("arithmetic");
    let child_rev = make_child_proto("reverse_k_arith");
    let chunk = make_chunk(parent, vec![child_arith, child_rev]);

    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    // Both closures must appear, either as separate `local` bindings OR
    // as named fields absorbed into the B0.47/B0.48 constructor.
    let arith_local  = out.contains("local arithmetic = function");
    let arith_field  = out.contains("arithmetic = function");
    let rev_local    = out.contains("local reverse_k_arith = function");
    let rev_field    = out.contains("reverse_k_arith = function");
    assert!(arith_local || arith_field,
        "expected `arithmetic` to appear as local or constructor field; got:\n{}", out);
    assert!(rev_local || rev_field,
        "expected `reverse_k_arith` to appear as local or constructor field; got:\n{}", out);
    // Critical shadow-local invariant: `reverse_k_arith = function` at
    // COLUMN 0 (no leading indentation) would be a global write.  The
    // constructor-field form `    reverse_k_arith = function` has
    // leading whitespace, so a strict newline+identifier match
    // distinguishes the two.
    assert!(!out.contains("\nreverse_k_arith = function"),
        "MUST NOT emit bare `reverse_k_arith = function` (global write); got:\n{}", out);
    // Module-table must be present — either as a seeded local (`local v0 = {` /
    // `local tbl = {`) or inlined directly into return (`return {`).
    // B0.104 allows single-use table inlining.
    let has_table = out.contains("local v0 = {") || out.contains("local tbl = {")
        || out.contains("return {");
    assert!(has_table,
        "expected module-table seed or inlined `return {{`; got:\n{}", out);
    // Return path must resolve to either the seeded name or an inline table.
    assert!(out.contains("return v0") || out.contains("return tbl") || out.contains("return {"),
        "expected `return v0` or `return tbl` or `return {{`; got:\n{}", out);
}

// ─── Arithmetic self-mutation (B0.43C LHS propagation) interaction ─

#[test]
fn b049_arithmetic_self_mutation_stays_as_assign() {
    // `count = count + 1` pattern from B0.3 / B0.43C.  This is a
    // two-write sequence to the same register that MUST stay as a
    // plain Reassign — shadowing here would emit
    //     local count = count + 1
    // which re-declares a shadow local and breaks iteration semantics.
    //
    // We drive this via `classify_write` directly (the arithmetic
    // self-mutation path in `store_complex` never reaches
    // classify_write when the register already holds a Name — the
    // function short-circuits to plain Assign.  Test guards that if
    // classify_write IS invoked with the same name, Reassign wins).
    let mut tracker = LocalTracker::new(0);
    let _ = tracker.classify_write(2, "count");
    let (kind, name) = tracker.classify_write(2, "count");
    assert_eq!(kind, WriteKind::Reassign,
        "self-mutation `count = count + 1` must not re-declare");
    assert_eq!(name, "count");
}

// ─── record_name keeps current_names in sync ───────────────────────

#[test]
fn b049_record_name_populates_current_name_for_pre_declared_reg() {
    // Callers that use pre_declare + record_name (CALL multi-return,
    // GETVARARGS, loop vars) must populate current_names so a later
    // single-reg write on the same reg can classify correctly.
    let mut tracker = LocalTracker::new(0);
    tracker.pre_declare(5);
    tracker.record_name(5, "k");

    // Same name → Reassign
    let (k1, n1) = tracker.classify_write(5, "k");
    assert_eq!(k1, WriteKind::Reassign);
    assert_eq!(n1, "k");

    // Different semantic name → Shadow
    let (k2, n2) = tracker.classify_write(5, "newK");
    assert_eq!(k2, WriteKind::Shadow);
    assert_eq!(n2, "newK");
}

#[test]
fn b049_record_name_without_recording_treats_as_generic_reassign() {
    // pre_declare WITHOUT record_name → current_names empty for that
    // reg → first classify_write records the new name and returns
    // Reassign (we can't safely shadow when we don't know the prior
    // semantic name).
    let mut tracker = LocalTracker::new(0);
    tracker.pre_declare(9);
    let (kind, name) = tracker.classify_write(9, "x");
    assert_eq!(kind, WriteKind::Reassign,
        "pre_declare w/o record_name must default to Reassign on first classify");
    assert_eq!(name, "x");
    // Subsequent same-name is still Reassign.
    let (k2, _) = tracker.classify_write(9, "x");
    assert_eq!(k2, WriteKind::Reassign);
}

// ─── Regression guard: B0.45A single-use inline still works ───────

#[test]
fn b049_b045a_single_use_inline_preserved() {
    // After Phase B0.49 edits, a simple single-NEWCLOSURE + return
    // proto must still emit `local fn = function()...end; return fn`
    // (the B0.45A aggressive-inline path may reduce it, but the
    // core shape is preserved).
    let code = vec![
        insn_ad(OP_NEWCLOSURE, 0, 1),    // NEWCLOSURE R0, proto[1]
        insn_abc(OP_RETURN, 0, 2, 0),    // RETURN R0, 1 result
    ];
    let parent = make_parent_proto(code, Vec::new());
    let child = make_child_proto("myFn");
    let chunk = make_chunk(parent, vec![child]);

    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    // Must declare as a local (first write to R0) — NOT a global.
    assert!(out.contains("local myFn = function") || out.contains("local function myFn") || out.contains("return function"),
        "expected `local myFn = function` / `local function myFn` / inlined `return function`; got:\n{}", out);
    // Must NOT emit `myFn = function` as a global write.
    assert!(!out.contains("\nmyFn = function"),
        "MUST NOT emit global `myFn = function`; got:\n{}", out);
}

// ─── Fresh-register FirstDecl preservation ────────────────────────

#[test]
fn b049_fresh_register_always_first_decl_regardless_of_name_kind() {
    // Even `v17` on a never-declared register is FirstDecl — we
    // don't Shadow a register that was never written before.
    let mut tracker = LocalTracker::new(0);
    let (kind, name) = tracker.classify_write(17, "v17");
    assert_eq!(kind, WriteKind::FirstDecl);
    assert_eq!(name, "v17");
}

// ─── Param-count boundary: exact boundary is not shadowed ──────────

#[test]
fn b049_classify_write_at_param_boundary() {
    // param_count = 3 → regs 0,1,2 are params (never shadow),
    // reg 3 is a local (can shadow).
    let mut tracker = LocalTracker::new(3);
    let (kind_param, _) = tracker.classify_write(2, "arg3");
    assert_eq!(kind_param, WriteKind::Reassign);

    let (kind_local, _) = tracker.classify_write(3, "x");
    assert_eq!(kind_local, WriteKind::FirstDecl);
    let (kind_reuse, _) = tracker.classify_write(3, "x");
    assert_eq!(kind_reuse, WriteKind::Reassign);
    let (kind_shadow, _) = tracker.classify_write(3, "y");
    assert_eq!(kind_shadow, WriteKind::Shadow);
}

// ─── End-to-end regression guard: simple closure + field assign ────

#[test]
fn b049_single_newclosure_followed_by_field_assign_no_shadow() {
    // Only ONE NEWCLOSURE to R1 → `arithmetic` must appear exactly once,
    // as either a standalone `local arithmetic = function` binding OR
    // a constructor-field in the B0.47/B0.48 collapsed output.  Guards
    // that we don't over-emit locals when there's nothing to shadow AND
    // don't emit a bare global write.
    let code = vec![
        insn_ad(OP_NEWTABLE, 0, 0),
        0u32,
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

    // `arithmetic = function` must appear exactly ONCE (either as a
    // local binding or a constructor field).  More than one would mean
    // a redundant shadow; zero would mean the closure was dropped.
    let occurrences = out.matches("arithmetic = function").count();
    assert_eq!(occurrences, 1,
        "expected exactly 1 `arithmetic = function` emission; got {}:\n{}",
        occurrences, out);
    // Must not emit a bare global write (newline-prefixed identifier with
    // no `local` and no leading indentation).
    assert!(!out.contains("\narithmetic = function"),
        "MUST NOT emit bare `arithmetic = function` (global); got:\n{}", out);
    // Module-table must be present — either as a seeded local or inlined into return.
    let has_table = out.contains("local v0 = {") || out.contains("local tbl = {")
        || out.contains("return {");
    assert!(has_table,
        "expected module-table seed or inlined `return {{`; got:\n{}", out);
}

// ─── snapshot/new_since invariance under classify_write ───────────

#[test]
fn b049_snapshot_still_works_after_shadow() {
    // snapshot/new_since track `declared` only — Shadow on an
    // already-declared register doesn't add a new entry, so
    // new_since must return empty.  Guards hoist_loop_locals behavior.
    let mut tracker = LocalTracker::new(0);
    let _ = tracker.classify_write(4, "first");
    let snap = tracker.snapshot();
    let (kind, _) = tracker.classify_write(4, "second");
    assert_eq!(kind, WriteKind::Shadow);
    let new = tracker.new_since(&snap);
    assert!(new.is_empty(),
        "Shadow on already-declared reg must NOT appear in new_since; got {:?}", new);
}

#[test]
fn b049_snapshot_captures_first_decl_inside_scope() {
    // Snap before FirstDecl → new_since returns [reg].  Guards
    // hoist_loop_locals when a register is first declared inside
    // the snapshotted scope.
    let mut tracker = LocalTracker::new(0);
    let snap = tracker.snapshot();
    let _ = tracker.classify_write(6, "inside");
    let new = tracker.new_since(&snap);
    assert_eq!(new, vec![6]);
}
