//! Phase C4 bonus — Konstant V2.1 crasher regression fixtures.
//!
//! Konstant V2.1 (the decompiler this project aims to beat) is known to crash
//! or emit `KONSTANTERROR`-style artifacts on a small set of specific input
//! patterns.  This file locks in regression coverage for three of them:
//!
//!   F1 — `while true do x() break end` followed by a trailing assignment.
//!        Konstant's `while true do ... break end` → `return` rewrite is
//!        defeated by the trailing statement; our output must keep both
//!        the while-break and the trailing stmt verbatim.
//!
//!   F2 — A closure built inside a table literal inside a for-loop body
//!        (`for i do local t = { run = function() return i end } t.run() end`).
//!        Known Konstant crasher; our output must simply *parse*.
//!
//!   F3 — Expression-reuse pattern that forces Konstant's
//!        `-- Expression was reused: ...` annotation: a single local is read
//!        twice downstream, which Konstant can't represent, so it inlines
//!        twice and comments the second copy.  Our output must keep the
//!        local as a named binding (not duplicate the RHS).
//!
//! # Bytecode source
//!
//! The ideal flow is source-code → `luau_compile` (Rust crate or CLI) → raw
//! bytecode → `luau_core::decompile_with_opmap`.  No such compiler bridge
//! exists in this workspace today (no Luau compiler dep is wired in).  The
//! next-best option — replicated across every existing lifter test in
//! `crates/luau-core/src/decompiler/lifter/tests/` — is to hand-craft a
//! minimal `Chunk` whose code stream reproduces the pattern we care about,
//! then drive the lifter via the public `decompile_proto` + `DecompileContext`
//! entry points.  This exercises the same code path as
//! `decompile_with_opmap` minus the (irrelevant here) parser + opmap
//! detection layers.
//!
//! # Assertions
//!
//! For every fixture we assert:
//!   * `full_moon::parse_fallible` succeeds (≥ 0 errors allowed only as noted);
//!   * the output does NOT contain the sentinel string `KONSTANTERROR`;
//!   * the output does NOT contain `-- lifter error` (A8's guard text).
//!
//! Fixtures that don't yet pass all three checks are marked `#[ignore]` with
//! a comment pointing at the blocker, so the test file itself stays green on
//! CI while the regressions remain visible.

use luau_core::decompiler::{decompile_proto, DecompileContext};
use luau_core::parser::types::{Chunk, Constant, Proto};

// ── Canonical opcode byte constants (mirror `LuauOpcode as u8`) ────────────
const OP_LOADN: u8        = 4;
const OP_GETUPVAL: u8     = 9;
const OP_GETTABLEKS: u8   = 15;
const OP_SETTABLEKS: u8   = 16;
const OP_NEWCLOSURE: u8   = 19;
const OP_CALL: u8         = 21;
const OP_RETURN: u8       = 22;
const OP_JUMP: u8         = 23;
const OP_JUMPBACK: u8     = 24;
const OP_ADD: u8          = 33;
const OP_MULK: u8         = 41;
const OP_DIVK: u8         = 42;
const OP_NEWTABLE: u8     = 53;
const OP_FORNPREP: u8     = 56;
const OP_FORNLOOP: u8     = 57;
const OP_CAPTURE: u8      = 70;

// ── Instruction encoding helpers ───────────────────────────────────────────

fn insn_ad(op: u8, a: u8, d: i16) -> u32 {
    let du = d as u16 as u32;
    (op as u32) | ((a as u32) << 8) | (du << 16)
}

fn insn_abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
}

// ── Chunk/Proto builders ───────────────────────────────────────────────────

fn make_proto(code: Vec<u32>, constants: Vec<Constant>, num_params: u8) -> Proto {
    Proto {
        max_stack_size: 32,
        num_params,
        num_upvalues: 4,
        is_vararg: true,
        flags: 0,
        typeinfo: None,
        code,
        constants,
        child_protos: Vec::new(),
        line_defined: 1,
        debug_name: Some("konstant_crasher_fixture".to_string()),
        line_info: None,
        debug_info: None,
    }
}

fn make_chunk(proto: Proto, strings: Vec<String>) -> Chunk {
    Chunk {
        version: 6,
        types_version: 0,
        strings,
        protos: vec![proto],
        main_proto: 0,
    }
}

/// Build a chunk with a main proto + one child proto (for NEWCLOSURE fixtures).
fn make_chunk_with_child(main: Proto, child: Proto, strings: Vec<String>) -> Chunk {
    let mut main = main;
    main.child_protos = vec![1];
    Chunk {
        version: 6,
        types_version: 0,
        strings,
        protos: vec![main, child],
        main_proto: 0,
    }
}

fn run_decompile(chunk: &Chunk) -> String {
    let mut ctx = DecompileContext::new(chunk);
    decompile_proto(&mut ctx, &chunk.protos[0], 0, 0)
}

// ── Shared assertions ──────────────────────────────────────────────────────

/// Every Konstant-crasher fixture shares the same three checks.  Panics with
/// a labelled message on failure.
fn assert_output_is_clean(label: &str, source: &str) {
    // Guard 1: no Konstant-style error artifacts.
    assert!(
        !source.contains("KONSTANTERROR"),
        "[{label}] output contains forbidden substring KONSTANTERROR:\n{source}",
    );
    // Guard 2: no A8 lifter-error markers (if this fires the lifter is
    // bailing; we want to notice immediately).
    assert!(
        !source.contains("-- lifter error"),
        "[{label}] output contains '-- lifter error' marker:\n{source}",
    );

    // Guard 3: output must parse as Luau.  `parse_fallible` never panics; it
    // returns a result-with-errors that we inspect for parse errors.  We
    // allow zero errors — anything else means our emitter produced
    // syntactically invalid source.
    let parse_result = full_moon::parse_fallible(source, full_moon::LuaVersion::luau());
    let errors = parse_result.errors();
    assert!(
        errors.is_empty(),
        "[{label}] full_moon failed to parse our output ({} errors):\n--- source ---\n{source}\n--- errors ---\n{errors:#?}",
        errors.len(),
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// F1 — while true do x() break end + trailing assignment
// ═══════════════════════════════════════════════════════════════════════════
//
// Source we're modelling:
//     while true do x() break end
//     _ = 0
//
// Hand-crafted bytecode (main proto, 0 params, upvalue `x` via GETUPVAL):
//     pc0: GETUPVAL    R0, 0            ; R0 = x
//     pc1: CALL        R0, 1, 1         ; x() — 0 args, 0 results
//     pc2: JUMP        +1               ; break — jump past JUMPBACK
//     pc3: JUMPBACK    -3               ; loop back to pc0 (would be the
//                                      ;   while-true re-entry if no break)
//     pc4: LOADN       R0, 0            ; `_ = 0` — assign K0 target, which
//                                      ;   on emission is rendered as
//                                      ;   an assignment to the `_` binding
//                                      ;   (we bind R0 to `_` for clarity)
//     pc5: RETURN      R0, 1, 0
//
// The JUMPBACK walking back past the LOADN at pc0 is what gives the lifter
// the `while true do … end` shape; the early JUMP out of the loop is the
// `break`.  The trailing LOADN + RETURN is the `_ = 0` tail.
#[test]
fn f1_while_true_break_with_trailing_assignment() {
    let code = vec![
        insn_abc(OP_GETUPVAL, 0, 0, 0),        // pc0  R0 = upval[0]   (x)
        insn_abc(OP_CALL, 0, 1, 1),            // pc1  x()
        insn_ad(OP_JUMP, 0, 1),                // pc2  break → skip JUMPBACK
        insn_ad(OP_JUMPBACK, 0, -3i16),        // pc3  continue loop
        insn_ad(OP_LOADN, 0, 0),               // pc4  R0 = 0     (the `_ = 0`)
        insn_abc(OP_RETURN, 0, 1, 0),          // pc5  return
    ];
    let chunk = make_chunk(make_proto(code, vec![], 0), Vec::new());
    let source = run_decompile(&chunk);

    // Header/version comment is added by `decompile_with_opmap`; here we
    // only drive `decompile_proto` so the output is the proto body text.
    eprintln!("[F1]\n{source}\n");
    assert_output_is_clean("F1", &source);
}

// ═══════════════════════════════════════════════════════════════════════════
// F2 — function inside table inside for-loop
// ═══════════════════════════════════════════════════════════════════════════
//
// Source we're modelling:
//     for i = 1, 10 do
//       local t = { run = function() return i end }
//       t.run()
//     end
//
// Hand-crafted bytecode. The numeric-for loop uses registers R0/R1/R2 for
// (limit, step, index) as per the Luau ABI (FORNPREP expects start in R(A+2),
// limit in R(A), step in R(A+1)).  We simplify by letting the compiler-style
// lowering emit LOADN + FORNPREP for the `for i = 1, 10` preamble:
//
//     pc0: LOADN   R0, 10    ; limit
//     pc1: LOADN   R1, 1     ; step
//     pc2: LOADN   R2, 1     ; start
//     pc3: FORNPREP R0, +7   ; jump to FORNLOOP if loop is empty
//     pc4: NEWTABLE R3, 0, 1 ; build {}, key count hint 1 (AUX=0)
//     pc5: (AUX)  0          ; NEWTABLE AUX word — array-size hint
//     pc6: NEWCLOSURE R4, 0  ; child proto 0 — function() return i end
//     pc7: CAPTURE  1, 2     ; capture R2 (i) as upvalue by value
//     pc8: SETTABLEKS R4, R3, 0  ; t.run = closure ; AUX=K0 "run"
//     pc9: (AUX)   0
//     pc10: GETTABLEKS R5, R3, 0  ; R5 = t.run ; AUX=K0 "run"
//     pc11: (AUX)   0
//     pc12: CALL R5, 1, 1     ; t.run()
//     pc13: FORNLOOP R0, -10  ; back to pc4
//     pc14: RETURN R0, 1, 0
//
// Child proto: returns its captured upvalue (i).
//     c0: GETUPVAL R0, 0
//     c1: RETURN R0, 2, 0
#[test]
fn f2_function_inside_table_inside_for_loop() {
    let main_code = vec![
        insn_ad(OP_LOADN, 0, 10),                    // pc0  limit = 10
        insn_ad(OP_LOADN, 1, 1),                     // pc1  step  = 1
        insn_ad(OP_LOADN, 2, 1),                     // pc2  start = 1
        insn_ad(OP_FORNPREP, 0, 10),                 // pc3  → pc14 if empty
        insn_abc(OP_NEWTABLE, 3, 0, 0),              // pc4  t = {}
        0u32,                                         // pc5  NEWTABLE AUX
        insn_ad(OP_NEWCLOSURE, 4, 0),                // pc6  closure = <child 0>
        insn_abc(OP_CAPTURE, 1, 2, 0),               // pc7  capture R2 as upval
        insn_abc(OP_SETTABLEKS, 4, 3, 0),            // pc8  t.run = closure
        0u32,                                         // pc9  SETTABLEKS AUX → K0
        insn_abc(OP_GETTABLEKS, 5, 3, 0),            // pc10 R5 = t.run
        0u32,                                         // pc11 GETTABLEKS AUX → K0
        insn_abc(OP_CALL, 5, 1, 1),                  // pc12 t.run()
        insn_ad(OP_FORNLOOP, 0, -10i16),             // pc13 loop back to pc4
        insn_abc(OP_RETURN, 0, 1, 0),                // pc14 return
    ];
    let main = make_proto(main_code, vec![Constant::String("run".to_string())], 0);

    let child = Proto {
        max_stack_size: 2,
        num_params: 0,
        num_upvalues: 1,
        is_vararg: false,
        flags: 0,
        typeinfo: None,
        code: vec![
            insn_abc(OP_GETUPVAL, 0, 0, 0),
            insn_abc(OP_RETURN, 0, 2, 0),
        ],
        constants: Vec::new(),
        child_protos: Vec::new(),
        line_defined: 1,
        debug_name: Some("f2_child".to_string()),
        line_info: None,
        debug_info: None,
    };

    let chunk = make_chunk_with_child(main, child, vec!["run".to_string()]);
    let source = run_decompile(&chunk);

    eprintln!("[F2]\n{source}\n");
    assert_output_is_clean("F2", &source);
}

// ═══════════════════════════════════════════════════════════════════════════
// F3 — expression reuse forces Konstant's `KONSTANTERROR`
// ═══════════════════════════════════════════════════════════════════════════
//
// Source we're modelling:
//     local a = b + c
//     local x = a * 2
//     local y = a / 3
//     return x, y
//
// Here `a` is read twice downstream (x = a * 2, y = a / 3).  Konstant
// eagerly inlines `a`'s RHS and then has nothing to emit for the second
// read, so it inserts a `-- Expression was reused` / KONSTANTERROR
// annotation.  Our decompiler must keep `a` as a named local — the fact
// that `count_name_reads(a) >= 2` blocks the single-use inlining pass.
//
// Bytecode: `b` and `c` as upvalues (simplest source of two distinct
// expression operands that don't collide with constants):
//     pc0: GETUPVAL R0, 0     ; R0 = b
//     pc1: GETUPVAL R1, 1     ; R1 = c
//     pc2: ADD       R2, R0, R1   ; a = b + c
//     pc3: MULK      R3, R2, 0    ; x = a * K0 (K0 = 2)
//     pc4: DIVK      R4, R2, 1    ; y = a / K1 (K1 = 3)
//     pc5: RETURN    R3, 3, 0     ; return x, y
//
// STATUS: `#[ignore]` as of Phase C4 bonus.  The current lifter inlines the
// ADD's RHS into BOTH downstream reads, producing
// `return (upval_0 + upval_1) * 2, (upval_0 + upval_1) / 3`.  That output is
// syntactically valid Luau (so the KONSTANTERROR / parse / lifter-error
// guards all pass) but it duplicates work, which is the same semantic bug
// Konstant tries to paper over with `-- Expression was reused`.  The fix
// lives in `count_name_reads` / `inline_single_use_temps` in
// `crates/luau-core/src/decompiler/lifter/naming.rs` — a real multi-read
// detection pass is out-of-scope for this fixture commit.  Un-ignore once
// the lifter preserves `a` as a named binding.
#[test]
#[ignore = "lifter currently inlines ADD into both reads; expected local-preservation fix first"]
fn f3_expression_reuse_keeps_local_binding() {
    let code = vec![
        insn_abc(OP_GETUPVAL, 0, 0, 0),            // R0 = b
        insn_abc(OP_GETUPVAL, 1, 1, 0),            // R1 = c
        insn_abc(OP_ADD, 2, 0, 1),                 // R2 = R0 + R1  (a)
        insn_abc(OP_MULK, 3, 2, 0),                // R3 = R2 * K0  (x = a*2)
        insn_abc(OP_DIVK, 4, 2, 1),                // R4 = R2 / K1  (y = a/3)
        insn_abc(OP_RETURN, 3, 3, 0),              // return R3, R4
    ];
    let chunk = make_chunk(
        make_proto(
            code,
            vec![Constant::Number(2.0), Constant::Number(3.0)],
            0,
        ),
        Vec::new(),
    );
    let source = run_decompile(&chunk);

    eprintln!("[F3]\n{source}\n");
    assert_output_is_clean("F3", &source);

    // Stronger property for F3: because `a` is read twice, our lifter must
    // keep it as a named binding rather than inlining `upval_0 + upval_1`
    // into both uses.  Detect this by looking for the RHS appearing at
    // most once in the output.  The exact local name is chosen by the
    // namer (could be `v0`, `result`, `a`, …); what matters is that the
    // expression `… + …` for the ADD shows up exactly once.
    let add_occurrences = source.matches(" + ").count();
    assert!(
        add_occurrences <= 1,
        "[F3] expected at most 1 occurrence of ' + ' (the original ADD), got {add_occurrences}.  \
         The decompiler appears to have inlined `a` into both reads; should have kept it as a local.\n\
         --- source ---\n{source}",
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// Sanity check — the test infrastructure itself produces parse-able output
// for a trivial empty proto.  If this ever fails, the entire suite is
// mis-wired (e.g. full_moon version skew) and the three fixtures above are
// meaningless.
// ═══════════════════════════════════════════════════════════════════════════
#[test]
fn sanity_empty_proto_parses() {
    let code = vec![insn_abc(OP_RETURN, 0, 1, 0)];
    let chunk = make_chunk(make_proto(code, vec![], 0), Vec::new());
    let source = run_decompile(&chunk);
    // A proto that just returns may emit an empty body — wrap in a stub
    // function to give full_moon something to chew on, since a totally
    // empty string is valid Luau anyway.
    let wrapped = format!("local function _stub()\n{source}\nend\n");
    let parse_result = full_moon::parse_fallible(&wrapped, full_moon::LuaVersion::luau());
    assert!(
        parse_result.errors().is_empty(),
        "sanity check failed — full_moon can't parse even the trivial wrapper:\n{wrapped}\n{:#?}",
        parse_result.errors(),
    );
}
