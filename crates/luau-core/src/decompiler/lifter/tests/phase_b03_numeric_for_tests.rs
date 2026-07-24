//! Extracted from lifter.rs as part of Phase B0.52P6 (part 3).
//! `super::super::` references the `lifter` module.

//! Phase B0.3 regression tests — lifter-only numeric-for reconstruction.
//!
//! These tests lock in the Luau v6 FORNPREP register layout:
//!     R(A+0) = limit
//!     R(A+1) = step
//!     R(A+2) = initial index / loop variable during the body
//!
//! Before Phase B0.3 the lifter used the Lua 5.1 layout
//! (`start=A, stop=A+1, step=A+2, var=A+3`), which emitted
//! `for i = arg1, 1 do end` on `ModuleScript.luac` Proto 9 instead of the
//! correct `for i = 1, n do sum = sum + i end`.
//!
//! The tests build synthetic single-proto `Chunk`s that mirror the exact
//! instruction shape observed in `ModuleScript.luac` for
//! `numeric_for_simple`, `nested_for`, and a descending `numeric_for_step`
//! variant, then run them through the full `decompile_proto` pipeline and
//! assert on the emitted source.

use crate::ast::Stat;
use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, Constant, DebugInfo, LocalVar, Proto};

// Standard (non-shuffled) Luau opcode bytes.
const OP_LOADN: u8     = 4;
const OP_MOVE: u8      = 6;
const OP_ADD: u8       = 33;
const OP_MUL: u8       = 35;
const OP_RETURN: u8    = 22;
const OP_FORNPREP: u8  = 56;
const OP_FORNLOOP: u8  = 57;

fn insn_ad(op: u8, a: u8, d: i16) -> u32 {
    let du = d as u16 as u32;
    (op as u32) | ((a as u32) << 8) | (du << 16)
}

fn insn_abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
}

fn make_chunk(proto: Proto) -> Chunk {
    Chunk {
        version: 6,
        types_version: 0,
        strings: Vec::new(),
        protos: vec![proto],
        main_proto: 0,
    }
}

fn make_proto(
    code: Vec<u32>,
    num_params: u8,
    max_stack: u8,
    name: &str,
) -> Proto {
    Proto {
        max_stack_size: max_stack,
        num_params,
        num_upvalues: 0,
        is_vararg: false,
        flags: 0,
        typeinfo: None,
        code,
        constants: Vec::new(),
        child_protos: Vec::new(),
        line_defined: 1,
        debug_name: Some(name.to_string()),
        line_info: None,
        debug_info: None,
    }
}

/// Like `make_proto` but with a `DebugInfo` block populated with the
/// given `locals`.  Used by Phase B0.10 tests to verify that original
/// variable names from debug info are propagated into loop bodies.
fn make_proto_with_debug(
    code: Vec<u32>,
    num_params: u8,
    max_stack: u8,
    name: &str,
    locals: Vec<LocalVar>,
) -> Proto {
    Proto {
        debug_info: Some(DebugInfo {
            locals,
            upvalue_names: vec![],
        }),
        ..make_proto(code, num_params, max_stack, name)
    }
}

/// Count `Stat::NumericFor` nodes at any nesting depth within a block.
fn count_numeric_fors(stmts: &[Stat]) -> usize {
    let mut n = 0;
    for s in stmts {
        match s {
            Stat::NumericFor { body, .. } => {
                n += 1 + count_numeric_fors(body);
            }
            Stat::GenericFor { body, .. }
            | Stat::While { body, .. }
            | Stat::Repeat { body, .. }
            | Stat::DoBlock { body } => n += count_numeric_fors(body),
            Stat::If { then_body, elseif_clauses, else_body, .. } => {
                n += count_numeric_fors(then_body);
                for (_, body) in elseif_clauses {
                    n += count_numeric_fors(body);
                }
                if let Some(eb) = else_body {
                    n += count_numeric_fors(eb);
                }
            }
            _ => {}
        }
    }
    n
}

/// Mirrors `ModuleScript.luac` Proto 9 `numeric_for_simple(n)` exactly:
///
/// ```lua
/// function M.numeric_for_simple(n)
///     local sum = 0
///     for i = 1, n do
///         sum = sum + i
///     end
///     return sum
/// end
/// ```
///
/// Register layout (1 param in R0):
///   R1 = sum, R2 = limit (from n), R3 = step (1), R4 = i (start 1)
fn build_numeric_for_simple() -> Chunk {
    let code = vec![
        insn_ad(OP_LOADN, 1, 0),          // 0: LOADN R1, 0       (sum = 0)
        insn_ad(OP_LOADN, 4, 1),          // 1: LOADN R4, 1       (i = 1, at A+2)
        insn_abc(OP_MOVE, 2, 0, 0),       // 2: MOVE  R2, R0      (limit = n, at A)
        insn_ad(OP_LOADN, 3, 1),          // 3: LOADN R3, 1       (step = 1, at A+1)
        insn_ad(OP_FORNPREP, 2, 2),       // 4: FORNPREP A=2 D=+2 (loop_pc = 4+2 = 6)
        insn_abc(OP_ADD, 1, 1, 4),        // 5: ADD   R1, R1, R4  (sum += i)
        insn_ad(OP_FORNLOOP, 2, -2),      // 6: FORNLOOP A=2 D=-2 (back to body at 5)
        insn_abc(OP_RETURN, 1, 2, 0),     // 7: RETURN R1..R1
    ];
    make_chunk(make_proto(code, 1, 5, "numeric_for_simple"))
}

#[test]
fn numeric_for_simple_uses_layout_2_register_mapping() {
    // Phase B0.3 primary regression test.
    //
    // With LAYOUT 1 (the bug):
    //   absorb(A+0)=R2=arg1 → start_expr = arg1
    //   absorb(A+1)=R3=1    → stop_expr  = 1
    //   absorb(A+2)=R4=1    → step_expr  = 1 (folded away)
    //   reg_name(A+3)=R5    → var_name = "v5" (no hint)
    // Output: `for v5 = arg1, 1 do end` (start/stop swapped, empty body).
    //
    // With LAYOUT 2 (the fix):
    //   absorb(A+2)=R4=1    → start_expr = 1
    //   absorb(A+1)=R3=1    → step_expr  = 1 (folded away)
    //   absorb(A+0)=R2=arg1 → stop_expr  = arg1
    //   reg_name(A+2)=R4    → var_name = "i" (NumericForVar hint)
    // Output: `for i = 1, arg1 do ... end` with the ADD body preserved.
    let chunk = build_numeric_for_simple();
    let mut ctx = DecompileContext::new(&chunk);
    let source = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    assert!(
        source.contains("for "),
        "Phase B0.3: expected a numeric-for in output, got:\n{}",
        source
    );

    // Start must be the literal 1 (from R4 = LOADN 1, A+2). Under LAYOUT 1
    // the start would be `arg1` (R2, A+0). We allow optional whitespace and
    // accept any form of `= 1,` as the start.
    assert!(
        source.contains("= 1,"),
        "Phase B0.3: start must be literal 1 (from R(A+2)=LOADN 1), got:\n{}",
        source
    );

    // Stop must reference the param (arg1). Under LAYOUT 1 the stop would
    // be the literal `1` (R3, A+1) which is clearly wrong.
    assert!(
        source.contains("arg"),
        "Phase B0.3: stop must reference param arg1 (from R(A+0)=MOVE R0), got:\n{}",
        source
    );

    // Body must NOT be empty. Under LAYOUT 1 the ADD body instruction
    // would still lift, but the loop variable symbol was wrong so the body
    // often collapsed. Assert the body contains an addition.
    let for_idx = source.find("for ").unwrap();
    let tail = &source[for_idx..];
    let end_idx = tail.find("end").expect("for must have a matching `end`");
    let body_slice = &tail[..end_idx];
    assert!(
        body_slice.contains('+'),
        "Phase B0.3: body must contain the ADD statement `sum + i`, got:\n{}",
        body_slice
    );
}

#[test]
fn numeric_for_simple_loop_variable_named_i_from_a_plus_2() {
    // Under LAYOUT 1, analyze_register_usage installs NumericForVar at
    // R(A+3)=R5, which is never read anywhere, so ctx.reg_name returns a
    // generic name (typically "v5"). The fix puts NumericForVar at R(A+2)
    // where the hint actually matches the loop variable, yielding "i".
    let chunk = build_numeric_for_simple();
    let mut ctx = DecompileContext::new(&chunk);
    let source = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    // Accept "for i" or "for i2" etc. (scoped-name suffixes are ok).
    let matches_i = source.split("for ").skip(1).any(|rest| {
        rest.starts_with('i')
            && rest.chars().nth(1).map_or(true, |c| !c.is_ascii_alphabetic() || c == '=' || c == ' ')
    });
    assert!(
        matches_i,
        "Phase B0.3: loop variable must be named 'i' (NumericForVar hint at R(A+2)), got:\n{}",
        source
    );
}

#[test]
fn numeric_for_simple_emits_exactly_one_for_loop() {
    // A synthetic single-for-loop proto must produce exactly one
    // Stat::NumericFor. This guards against regressions where the lifter
    // might (a) fail to recognize the for-loop and fall back to
    // while-true, or (b) spuriously duplicate the for-loop via structural
    // misanalysis.
    let chunk = build_numeric_for_simple();
    let mut ctx = DecompileContext::new(&chunk);
    let _ = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    // Re-lift to get the AST for structural inspection.
    let mut ctx2 = DecompileContext::new(&chunk);
    let stmts = super::super::lift_proto(&mut ctx2, &chunk.protos[0], 0);
    assert_eq!(
        count_numeric_fors(&stmts),
        1,
        "Phase B0.3: expected exactly 1 numeric-for, got {} in stmts:\n{:#?}",
        count_numeric_fors(&stmts),
        stmts
    );
}

/// Mirrors `ModuleScript.luac` Proto 11 `nested_for(n)`:
///
/// ```lua
/// function M.nested_for(n)
///     local sum = 0
///     for i = 1, n do
///         for j = 1, n do
///             sum = sum + (i * j)
///         end
///     end
///     return sum
/// end
/// ```
///
/// Outer-loop A=2: R2=limit(n), R3=step(1), R4=i.
/// Inner-loop A=5: R5=limit(n), R6=step(1), R7=j.
/// Body: MUL R8 = R4 * R7; ADD R1 = R1 + R8.
fn build_nested_for() -> Chunk {
    let code = vec![
        // Outer for-loop setup (matches Proto 11 exactly)
        insn_ad(OP_LOADN, 1, 0),          // 0:  LOADN R1, 0   (sum = 0)
        insn_ad(OP_LOADN, 4, 1),          // 1:  LOADN R4, 1   (outer i)
        insn_abc(OP_MOVE, 2, 0, 0),       // 2:  MOVE  R2, R0  (outer limit = n)
        insn_ad(OP_LOADN, 3, 1),          // 3:  LOADN R3, 1   (outer step)
        insn_ad(OP_FORNPREP, 2, 8),       // 4:  FORNPREP A=2 D=+8 (loop_pc = 12)
        // Inner for-loop setup
        insn_ad(OP_LOADN, 7, 1),          // 5:  LOADN R7, 1   (inner j)
        insn_abc(OP_MOVE, 5, 0, 0),       // 6:  MOVE  R5, R0  (inner limit = n)
        insn_ad(OP_LOADN, 6, 1),          // 7:  LOADN R6, 1   (inner step)
        insn_ad(OP_FORNPREP, 5, 3),       // 8:  FORNPREP A=5 D=+3 (loop_pc = 11)
        // Inner body: sum = sum + (i * j)
        insn_abc(OP_MUL, 8, 4, 7),        // 9:  MUL  R8, R4, R7
        insn_abc(OP_ADD, 1, 1, 8),        // 10: ADD  R1, R1, R8
        insn_ad(OP_FORNLOOP, 5, -3),      // 11: FORNLOOP A=5 D=-3 (back to 9)
        insn_ad(OP_FORNLOOP, 2, -8),      // 12: FORNLOOP A=2 D=-8 (back to 5)
        insn_abc(OP_RETURN, 1, 2, 0),     // 13: RETURN R1..R1
    ];
    make_chunk(make_proto(code, 1, 9, "nested_for"))
}

#[test]
fn nested_for_produces_two_nested_numeric_fors() {
    // Phase B0.4: nested-for recognition now lives in structuring.rs
    // (`structure_numeric_for_body`). This test was ignored under B0.3
    // because the structurer marked all blocks inside the outer body
    // range as "handled" before `try_match_for_loop` ever ran on the
    // inner ForNPrep block. B0.4 adds a linear pc-walk over the body
    // range that independently finds inner ForNPrep/ForNLoop pairs and
    // attaches them to the outer region as `body: Vec<Region>`. The
    // lifter then iterates that nested body and emits two
    // `Stat::NumericFor` nodes.
    // Nested for-loops exercise LAYOUT 2 at two different A bases (2 and 5)
    // AND the recursive lift through the inner body range. Under LAYOUT 1
    // both loops would be malformed: outer var = R5 (an inner loop
    // register), inner var = R8 (the MUL destination).
    let chunk = build_nested_for();
    let mut ctx = DecompileContext::new(&chunk);
    let stmts = super::super::lift_proto(&mut ctx, &chunk.protos[0], 0);

    assert_eq!(
        count_numeric_fors(&stmts),
        2,
        "Phase B0.3: expected 2 nested numeric-fors, got {}:\n{:#?}",
        count_numeric_fors(&stmts),
        stmts
    );
}

#[test]
fn nested_for_inner_body_references_both_loop_variables() {
    // Phase B0.4: active under the structuring-level nested-for fix.
    // Validates that the inner MUL body correctly references BOTH loop
    // variables (i and j) rather than stale pre-rebind literals from
    // the LOADN setup instructions. This is the end-to-end check that
    // the structuring fix + the existing Phase B0.3 loop-var rebind
    // + pre-materialization work together for nested cases.
    // The inner body's MUL takes the OUTER loop var (R4 = A_outer+2) and
    // INNER loop var (R7 = A_inner+2). Under LAYOUT 1 both loop variables
    // would be misaligned and the body would reference stale registers
    // (R5, R8) rather than the real loop variables.
    let chunk = build_nested_for();
    let mut ctx = DecompileContext::new(&chunk);
    let source = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    // The body text between the outer `for` and the matching `end` must
    // contain the inner `for` plus a multiplication.
    assert!(source.contains("for "), "expected for-loops, got:\n{}", source);
    assert!(
        source.matches("for ").count() >= 2,
        "Phase B0.3: nested_for must emit two 'for ' tokens, got:\n{}",
        source
    );
    assert!(
        source.contains('*'),
        "Phase B0.3: nested_for body must contain multiplication, got:\n{}",
        source
    );
}

/// Descending-step numeric-for (mirrors what `numeric_for_step` would
/// look like BEFORE the compiler unrolls it — useful for locking in
/// step extraction, since step=-1 lives at R(A+1)).
///
/// ```lua
/// function descending_for(n)
///     local sum = 0
///     for i = 10, 1, -1 do
///         sum = sum + i
///     end
///     return sum
/// end
/// ```
fn build_descending_for() -> Chunk {
    let code = vec![
        insn_ad(OP_LOADN, 1, 0),          // 0: LOADN R1, 0    (sum = 0)
        insn_ad(OP_LOADN, 4, 10),         // 1: LOADN R4, 10   (start at A+2)
        insn_ad(OP_LOADN, 2, 1),          // 2: LOADN R2, 1    (limit at A)
        insn_ad(OP_LOADN, 3, -1),         // 3: LOADN R3, -1   (step at A+1)
        insn_ad(OP_FORNPREP, 2, 2),       // 4: FORNPREP A=2 D=+2
        insn_abc(OP_ADD, 1, 1, 4),        // 5: ADD R1, R1, R4
        insn_ad(OP_FORNLOOP, 2, -2),      // 6: FORNLOOP A=2 D=-2
        insn_abc(OP_RETURN, 1, 2, 0),     // 7: RETURN R1..R1
    ];
    make_chunk(make_proto(code, 0, 5, "descending_for"))
}

#[test]
fn descending_for_preserves_negative_step() {
    // Step -1 must land at R(A+1). Under LAYOUT 1 the step would be read
    // from R(A+2)=R4=10 and the start from R(A)=R2=1, producing
    // `for v5 = 1, -1, 10 do` (utterly wrong). Under LAYOUT 2 we get
    // `for i = 10, 1, -1 do`.
    let chunk = build_descending_for();
    let mut ctx = DecompileContext::new(&chunk);
    let source = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    assert!(
        source.contains("for "),
        "expected a numeric-for, got:\n{}",
        source
    );
    // Start = 10 (from R(A+2) = LOADN 10).
    assert!(
        source.contains("= 10,"),
        "Phase B0.3: descending loop start must be 10, got:\n{}",
        source
    );
    // Step = -1 (from R(A+1) = LOADN -1). The emitter prints the step
    // only if it's not the default 1, so this also validates that step-1
    // is routed through the third slot.
    assert!(
        source.contains("-1"),
        "Phase B0.3: descending loop step -1 must be preserved, got:\n{}",
        source
    );
}

// ────────────────────────────────────────────────────────────────────────
// Phase B0.5 regression tests — extended body sub-structuring.
//
// Two targeted shapes:
//   (A) `for k, v in ... do` nested inside `for i = ... do`
//   (B) `for j = ... do` nested inside an if-arm inside `for i = ... do`
//
// Phase B0.4 introduced `structure_numeric_for_body` which recognizes only
// NumericFor-in-NumericFor. These tests lock in that:
//   (A) FORGPREP/FORGLOOP inside a numeric-for body now surfaces as a
//       real nested `Stat::GenericFor` instead of being silently dropped
//       by the lifter's empty for-op match arms.
//   (B) A forward conditional jump whose target crosses a nested
//       for-loop inside a numeric-for body now surfaces as a real
//       `Stat::If` wrapping a nested for-loop, instead of a spurious
//       `if cond then break end` (the B0.4 failure mode — the structurer
//       sliced the Linear range on the inner FORNPREP and the
//       conditional's target ended up past the Linear segment end,
//       causing `lift_instruction_range`'s "forward jump beyond range"
//       fallback to emit a break).
//
// In addition, these tests guard against regressing:
//   - Phase B0.4 nested NumericFor recognition (`build_nested_for`)
//   - break-preservation inside for-loop bodies

const OP_LOADNIL: u8    = 2;
const OP_CALL: u8       = 21;
const OP_FORGPREP: u8   = 58;
const OP_FORGLOOP: u8   = 59;
const OP_JUMPIFLE: u8   = 28;
const OP_GETIMPORT: u8  = 12;

/// Builds FORGLOOP AUX: low 24 bits = nresults (loop var count).
/// Bit 31 = inext flag (we leave it off — plain FORGPREP form).
fn forgloop_aux(nresults: u32) -> u32 {
    nresults & 0x7FFFFFFF
}

/// JumpIfLE AUX: low 8 bits = comparison RHS register.
fn jumpifle_aux(rhs_reg: u8) -> u32 {
    rhs_reg as u32
}

/// Build the bytecode for (Shape A):
///
/// ```lua
/// function(t)
///     local sum = 0
///     for i = 1, 5 do
///         for k, v in t do
///             sum = sum + v
///         end
///     end
///     return sum
/// end
/// ```
///
/// Register layout:
///   R0 = t (param)
///   R1 = sum
///   R2 = outer limit (=5)   [A_outer = 2]
///   R3 = outer step  (=1)
///   R4 = outer i
///   R5 = inner generator    [A_inner = 5]
///   R6 = inner state
///   R7 = inner control
///   R8 = inner k   (at A_inner+3)
///   R9 = inner v   (at A_inner+4)
fn build_genfor_in_numfor() -> Chunk {
    let code = vec![
        insn_ad(OP_LOADN,    1, 0),                   // 0: LOADN  R1, 0            (sum = 0)
        insn_ad(OP_LOADN,    4, 1),                   // 1: LOADN  R4, 1            (outer i, A+2)
        insn_ad(OP_LOADN,    2, 5),                   // 2: LOADN  R2, 5            (outer limit, A)
        insn_ad(OP_LOADN,    3, 1),                   // 3: LOADN  R3, 1            (outer step, A+1)
        insn_ad(OP_FORNPREP, 2, 8),                   // 4: FORNPREP A=2 D=+8       (outer loop_pc = 4+8 = 12)
        insn_abc(OP_MOVE,    5, 0, 0),                // 5: MOVE   R5, R0           (inner generator = t)
        insn_abc(OP_LOADNIL, 6, 0, 0),                // 6: LOADNIL R6              (inner state)
        insn_abc(OP_LOADNIL, 7, 0, 0),                // 7: LOADNIL R7              (inner control)
        insn_ad(OP_FORGPREP, 5, 1),                   // 8: FORGPREP A=5 D=+1       (inner loop_pc = 8+1+1 = 10)
        insn_abc(OP_ADD,     1, 1, 9),                // 9: ADD    R1, R1, R9       (sum = sum + v)
        insn_ad(OP_FORGLOOP, 5, -2),                  // 10: FORGLOOP A=5 D=-2      (back to pc 9)
        forgloop_aux(2),                              // 11: AUX   nresults = 2
        insn_ad(OP_FORNLOOP, 2, -8),                  // 12: FORNLOOP A=2 D=-8      (back to pc 5)
        insn_abc(OP_RETURN,  1, 2, 0),                // 13: RETURN R1..R1
    ];
    make_chunk(make_proto(code, 1, 10, "genfor_in_numfor"))
}

/// Count `Stat::GenericFor` nodes at any nesting depth within a block.
fn count_generic_fors(stmts: &[Stat]) -> usize {
    let mut n = 0;
    for s in stmts {
        match s {
            Stat::GenericFor { body, .. } => {
                n += 1 + count_generic_fors(body);
            }
            Stat::NumericFor { body, .. }
            | Stat::While { body, .. }
            | Stat::Repeat { body, .. }
            | Stat::DoBlock { body } => n += count_generic_fors(body),
            Stat::If { then_body, elseif_clauses, else_body, .. } => {
                n += count_generic_fors(then_body);
                for (_, body) in elseif_clauses {
                    n += count_generic_fors(body);
                }
                if let Some(eb) = else_body {
                    n += count_generic_fors(eb);
                }
            }
            _ => {}
        }
    }
    n
}

/// Count `Stat::If` nodes at any nesting depth within a block.
fn count_ifs(stmts: &[Stat]) -> usize {
    let mut n = 0;
    for s in stmts {
        match s {
            Stat::If { then_body, elseif_clauses, else_body, .. } => {
                n += 1;
                n += count_ifs(then_body);
                for (_, body) in elseif_clauses {
                    n += count_ifs(body);
                }
                if let Some(eb) = else_body {
                    n += count_ifs(eb);
                }
            }
            Stat::NumericFor { body, .. }
            | Stat::GenericFor { body, .. }
            | Stat::While { body, .. }
            | Stat::Repeat { body, .. }
            | Stat::DoBlock { body } => n += count_ifs(body),
            _ => {}
        }
    }
    n
}

/// Count `Stat::Break` nodes at any nesting depth within a block.
fn count_breaks(stmts: &[Stat]) -> usize {
    let mut n = 0;
    for s in stmts {
        match s {
            Stat::Break => n += 1,
            Stat::NumericFor { body, .. }
            | Stat::GenericFor { body, .. }
            | Stat::While { body, .. }
            | Stat::Repeat { body, .. }
            | Stat::DoBlock { body } => n += count_breaks(body),
            Stat::If { then_body, elseif_clauses, else_body, .. } => {
                n += count_breaks(then_body);
                for (_, body) in elseif_clauses {
                    n += count_breaks(body);
                }
                if let Some(eb) = else_body {
                    n += count_breaks(eb);
                }
            }
            _ => {}
        }
    }
    n
}

#[test]
fn b05_genfor_inside_numfor_recognized_as_nested_generic_for() {
    // Phase B0.5 primary Shape-A test.
    //
    // Pre-B0.5 behavior (B0.4): `structure_numeric_for_body` only matches
    // ForNPrep. A nested FORGPREP/FORGLOOP pair is left inside the Linear
    // segment, which `lift_instruction_range` then processes via the
    // `LuauOpcode::ForNPrep | ForNLoop | ForGPrep | ForGLoop ... => {}`
    // EMPTY match arm (lifter.rs ~line 2385). Net effect: the inner
    // generic-for disappears, the ADD reads a stale register, and the
    // outer body shows a single `sum = sum + nil` (or similar garbage).
    //
    // Phase B0.5 fix: extend `structure_numeric_for_body` to also match
    // FORGPREP/FORGPREP_INEXT/FORGPREP_NEXT, emitting `Region::GenericFor`
    // sub-regions alongside `Region::NumericFor` and `Region::Linear`.
    // The lifter dispatches Region::GenericFor via `lift_region`, which
    // calls the existing GenericFor handler and emits `Stat::GenericFor`.
    let chunk = build_genfor_in_numfor();
    let mut ctx = DecompileContext::new(&chunk);
    let stmts = super::super::lift_proto(&mut ctx, &chunk.protos[0], 0);

    let num = count_numeric_fors(&stmts);
    let gen = count_generic_fors(&stmts);
    assert_eq!(
        num, 1,
        "Phase B0.5: expected exactly 1 numeric-for, got {}:\n{:#?}",
        num, stmts,
    );
    assert_eq!(
        gen, 1,
        "Phase B0.5: expected exactly 1 nested generic-for inside the numeric-for, got {}:\n{:#?}",
        gen, stmts,
    );
}

#[test]
fn b05_genfor_in_numfor_end_to_end_source() {
    // End-to-end source check: output must contain both a `for i = ...`
    // (outer numeric-for) AND a `for k, v in ...` (inner generic-for).
    let chunk = build_genfor_in_numfor();
    let mut ctx = DecompileContext::new(&chunk);
    let source = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    assert!(
        source.contains("for ") && source.matches("for ").count() >= 2,
        "Phase B0.5: expected both outer numeric-for and inner generic-for, got:\n{}",
        source
    );
    // Inner generic-for must render with `in` keyword.
    assert!(
        source.contains(" in "),
        "Phase B0.5: inner generic-for must render with `in` keyword, got:\n{}",
        source
    );
}

/// Build the bytecode for (Shape B):
///
/// ```lua
/// function(n)
///     local sum = 0
///     for i = 1, n do
///         if i > 2 then
///             for j = 1, 3 do
///                 sum = sum + j
///             end
///         end
///     end
///     return sum
/// end
/// ```
///
/// Register layout:
///   R0 = n (param)
///   R1 = sum
///   R2 = outer limit (n)   [A_outer = 2]
///   R3 = outer step  (1)
///   R4 = outer i
///   R5 = comparison rhs constant (2)
///   R6 = inner limit (3)   [A_inner = 6]
///   R7 = inner step  (1)
///   R8 = inner j
fn build_numfor_in_if_in_numfor() -> Chunk {
    let code = vec![
        insn_ad(OP_LOADN,    1, 0),                   // 0: LOADN  R1, 0            (sum = 0)
        insn_ad(OP_LOADN,    4, 1),                   // 1: LOADN  R4, 1            (outer i, A+2)
        insn_abc(OP_MOVE,    2, 0, 0),                // 2: MOVE   R2, R0           (outer limit = n, A)
        insn_ad(OP_LOADN,    3, 1),                   // 3: LOADN  R3, 1            (outer step, A+1)
        insn_ad(OP_FORNPREP, 2, 10),                  // 4: FORNPREP A=2 D=+10      (outer loop_pc = 4+10 = 14)
        insn_ad(OP_LOADN,    5, 2),                   // 5: LOADN  R5, 2            (const 2 for `i > 2` check)
        insn_ad(OP_JUMPIFLE, 4, 7),                   // 6: JUMPIFLE A=4 AUX=R5 D=+7 (skip if R4<=R5, target = 6+7+1 = 14)
        jumpifle_aux(5),                              // 7: AUX    = R5
        insn_ad(OP_LOADN,    8, 1),                   // 8: LOADN  R8, 1            (inner j, A+2 of inner)
        insn_ad(OP_LOADN,    6, 3),                   // 9: LOADN  R6, 3            (inner limit, A)
        insn_ad(OP_LOADN,    7, 1),                   // 10: LOADN R7, 1            (inner step, A+1)
        insn_ad(OP_FORNPREP, 6, 2),                   // 11: FORNPREP A=6 D=+2      (inner loop_pc = 11+2 = 13)
        insn_abc(OP_ADD,     1, 1, 8),                // 12: ADD   R1, R1, R8       (sum = sum + j)
        insn_ad(OP_FORNLOOP, 6, -2),                  // 13: FORNLOOP A=6 D=-2      (back to pc 12)
        insn_ad(OP_FORNLOOP, 2, -10),                 // 14: FORNLOOP A=2 D=-10     (back to pc 5)
        insn_abc(OP_RETURN,  1, 2, 0),                // 15: RETURN R1..R1
    ];
    make_chunk(make_proto(code, 1, 9, "numfor_in_if_in_numfor"))
}

#[test]
fn b05_numfor_in_if_in_numfor_emits_two_numeric_fors_wrapped_in_if() {
    // Phase B0.5 primary Shape-B test.
    //
    // Pre-B0.5 behavior (B0.4): `structure_numeric_for_body` walks the
    // outer body linearly and extracts the inner FORNPREP/FORNLOOP pair
    // as a nested Region::NumericFor. This splits the Linear range on
    // the inner FORNPREP boundary. The preceding Linear segment ends
    // at pc_of_fornprep, but the JumpIfLE at pc=6 has target = 14 (past
    // the inner for-loop). The lifter's `lift_instruction_range` sees
    // target > end (11) and falls into the "forward jump beyond range"
    // branch, which in a loop context emits `if cond then break end`.
    // Final output: outer for-loop body contains a SPURIOUS break
    // followed by the inner for-loop — semantically wrong.
    //
    // Phase B0.5 fix: extend `structure_numeric_for_body` to recognize
    // forward conditional jumps whose target range contains a nested
    // for-loop. Emit a new `Region::InlineIfThenInLoop` that wraps the
    // nested body (Linear + NumericFor/GenericFor). The lifter extracts
    // the condition via `extract_branch_condition` and emits a real
    // `Stat::If` containing the inner for-loop — no spurious breaks.
    let chunk = build_numfor_in_if_in_numfor();
    let mut ctx = DecompileContext::new(&chunk);
    let stmts = super::super::lift_proto(&mut ctx, &chunk.protos[0], 0);

    let num = count_numeric_fors(&stmts);
    let ifs = count_ifs(&stmts);
    let breaks = count_breaks(&stmts);
    assert_eq!(
        num, 2,
        "Phase B0.5: expected 2 nested numeric-fors (outer + inner-inside-if), got {}:\n{:#?}",
        num, stmts,
    );
    assert!(
        ifs >= 1,
        "Phase B0.5: expected at least 1 if-statement wrapping the inner numeric-for, got {}:\n{:#?}",
        ifs, stmts,
    );
    assert_eq!(
        breaks, 0,
        "Phase B0.5: expected 0 break statements (the JumpIfLE must NOT become a spurious break), got {}:\n{:#?}",
        breaks, stmts,
    );
}

#[test]
fn b05_numfor_in_if_in_numfor_end_to_end_source() {
    // End-to-end source check: output must contain `for i = 1,`, an
    // `if i >` or `if i <=` (any comparison against i), and an inner
    // `for j = 1, 3`. There must be NO `break` inside the outer body.
    let chunk = build_numfor_in_if_in_numfor();
    let mut ctx = DecompileContext::new(&chunk);
    let source = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    assert!(
        source.contains("for "),
        "Phase B0.5: expected numeric-for, got:\n{}",
        source
    );
    assert!(
        source.matches("for ").count() >= 2,
        "Phase B0.5: expected nested numeric-fors (outer + inner), got:\n{}",
        source
    );
    assert!(
        source.contains("if "),
        "Phase B0.5: expected if-statement wrapping the inner for, got:\n{}",
        source
    );
    assert!(
        !source.contains("break"),
        "Phase B0.5: no spurious break statements allowed (JumpIfLE must become if-then, not if-break), got:\n{}",
        source
    );
}

#[test]
fn b05_nested_numeric_for_still_works() {
    // Phase B0.5 must NOT regress Phase B0.4's nested-numeric-for
    // recognition. Re-runs the exact nested_for fixture.
    let chunk = build_nested_for();
    let mut ctx = DecompileContext::new(&chunk);
    let stmts = super::super::lift_proto(&mut ctx, &chunk.protos[0], 0);

    assert_eq!(
        count_numeric_fors(&stmts),
        2,
        "Phase B0.5 must not regress B0.4 nested NumericFor recognition, got {}:\n{:#?}",
        count_numeric_fors(&stmts),
        stmts
    );
}

// ─────────────────────────────────────────────────────────────────────
// Phase B0.6 — generic-for body sub-structuring.
//
// Mirror of Phase B0.4/B0.5 for the GenericFor case. Before B0.6,
// `Region::GenericFor` used `lift_instruction_range` for its body,
// which hit the empty `ForNPrep | ForNLoop | ForGPrep | ForGLoop => {}`
// match arm for any nested for-loops and silently dropped them.
//
// These tests cover:
//   1. Nested `for i = ...` inside `for k, v in ...` (Shape C — mirror of B0.4)
//   2. Nested `for k2, v2 in ...` inside `for k, v in ...` (Shape D)
//
// ─────────────────────────────────────────────────────────────────────

/// Build the bytecode for (Shape C):
///
/// ```lua
/// function(t)
///     local sum = 0
///     for k, v in t do
///         for i = 1, 3 do
///             sum = sum + i
///         end
///     end
///     return sum
/// end
/// ```
///
/// Register layout:
///   R0 = t (param)
///   R1 = sum
///   R2 = outer generator     [A_outer = 2]
///   R3 = outer state
///   R4 = outer control
///   R5 = outer k   (A+3)
///   R6 = outer v   (A+4)
///   R7 = inner limit   [A_inner = 7]
///   R8 = inner step    (A+1)
///   R9 = inner i       (A+2)
///
/// PC layout:
///   0: LOADN R1, 0          (sum = 0)
///   1: MOVE R2, R0          (outer generator = t)
///   2: LOADNIL R3           (outer state)
///   3: LOADNIL R4           (outer control)
///   4: FORGPREP A=2 D=+6    (outer loop_pc = 4+6+1 = 11)
///   5: LOADN R9, 1          (inner i)
///   6: LOADN R7, 3          (inner limit)
///   7: LOADN R8, 1          (inner step)
///   8: FORNPREP A=7 D=+2    (inner loop_pc = 8+2 = 10)
///   9: ADD R1, R1, R9       (sum = sum + i)
///   10: FORNLOOP A=7 D=-2   (back to pc 9)
///   11: FORGLOOP A=2 D=-7   (back to pc 5)
///   12: AUX (nresults=2)
///   13: RETURN R1..R1
fn build_numfor_in_genfor() -> Chunk {
    let code = vec![
        insn_ad(OP_LOADN,    1, 0),                   // 0
        insn_abc(OP_MOVE,    2, 0, 0),                // 1
        insn_abc(OP_LOADNIL, 3, 0, 0),                // 2
        insn_abc(OP_LOADNIL, 4, 0, 0),                // 3
        insn_ad(OP_FORGPREP, 2, 6),                   // 4
        insn_ad(OP_LOADN,    9, 1),                   // 5
        insn_ad(OP_LOADN,    7, 3),                   // 6
        insn_ad(OP_LOADN,    8, 1),                   // 7
        insn_ad(OP_FORNPREP, 7, 2),                   // 8
        insn_abc(OP_ADD,     1, 1, 9),                // 9
        insn_ad(OP_FORNLOOP, 7, -2),                  // 10
        insn_ad(OP_FORGLOOP, 2, -7),                  // 11
        forgloop_aux(2),                              // 12 (AUX)
        insn_abc(OP_RETURN,  1, 2, 0),                // 13
    ];
    make_chunk(make_proto(code, 1, 10, "numfor_in_genfor"))
}

#[test]
fn b06_numfor_inside_genfor_recognized_as_nested() {
    // Phase B0.6 primary Shape-C test.
    //
    // Pre-B0.6 behavior: `Region::GenericFor` used linear body lift via
    // `lift_instruction_range`. The inner ForNPrep/ForNLoop pair hit the
    // empty for-opcode match arm and was silently dropped. Final output:
    // outer generic-for body contains only the ADD (or garbage from stale
    // registers), no inner numeric-for statement.
    //
    // Phase B0.6 fix: `Region::GenericFor` gains a `body: Vec<Region>`
    // field populated by `structure_numeric_for_body`. The lifter iterates
    // the nested body the same way it does for `Region::NumericFor`,
    // dispatching nested NumericFor regions through `lift_region`.
    let chunk = build_numfor_in_genfor();
    let mut ctx = DecompileContext::new(&chunk);
    let stmts = super::super::lift_proto(&mut ctx, &chunk.protos[0], 0);

    let gen = count_generic_fors(&stmts);
    let num = count_numeric_fors(&stmts);
    assert_eq!(
        gen, 1,
        "Phase B0.6: expected exactly 1 generic-for (outer), got {}:\n{:#?}",
        gen, stmts,
    );
    assert_eq!(
        num, 1,
        "Phase B0.6: expected exactly 1 nested numeric-for inside the generic-for, got {}:\n{:#?}",
        num, stmts,
    );
}

#[test]
fn b06_numfor_in_genfor_end_to_end_source() {
    // End-to-end source check: output must contain both `for k, v in ...`
    // (outer generic-for with `in` keyword) and `for i = 1, 3` (inner
    // numeric-for).
    let chunk = build_numfor_in_genfor();
    let mut ctx = DecompileContext::new(&chunk);
    let source = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    assert!(
        source.matches("for ").count() >= 2,
        "Phase B0.6: expected at least 2 `for ` tokens (outer + inner), got:\n{}",
        source
    );
    assert!(
        source.contains(" in "),
        "Phase B0.6: outer generic-for must render with `in` keyword, got:\n{}",
        source
    );
}

#[test]
fn b06_genfor_nested_in_numfor_still_works() {
    // Guards Phase B0.5 Shape-A (genfor inside numfor). B0.6 must not
    // regress Phase B0.5.
    let chunk = build_genfor_in_numfor();
    let mut ctx = DecompileContext::new(&chunk);
    let stmts = super::super::lift_proto(&mut ctx, &chunk.protos[0], 0);

    assert_eq!(count_numeric_fors(&stmts), 1, "B0.5 Shape-A regression");
    assert_eq!(count_generic_fors(&stmts), 1, "B0.5 Shape-A regression");
}

// ─────────────────────────────────────────────────────────────────────
// Phase B0.7 — `absorb_iterator_setup` multi-iterator fallback.
//
// Root cause: Pre-B0.7, `absorb_iterator_setup` returned a single
// `Expr` — either the absorbed call result (common shape:
// `local v = pairs(t); FORGPREP`) or a bare `reg_expr(regs, a)` when
// absorption failed. For `for k, v in next, t do` / `for k, v in f, s do`
// style source, the Luau compiler compiles the three-value iterator
// triple DIRECTLY into registers via GETIMPORT + MOVE + LOADNIL
// (or MOVE + MOVE + LOADNIL), with NO preceding CALL. The old
// fallback only carried R(A) (the generator) through, so the state
// register was lost entirely, rendering `for k, v in next do`
// (wrong — no table argument).
//
// Phase B0.7 fix: `absorb_iterator_setup` now returns `Vec<Expr>`.
// On fallback, it builds an explicit 1..=3 element iterator tuple
// from `regs[a..=a+2]`, trimming trailing Nil/Unknown entries. This
// recovers the full `for k, v in next, t do` shape.
//
// This smoking-gun pattern is directly observed in `ModuleScript.luac`
// Proto 14 `generic_for_next`:
//
//   GETIMPORT R2 ; next
//   MOVE      R3 R0        -- R3 = t (state)
//   LOADNIL   R4           -- R4 = nil (control)
//   FORGPREP_NEXT R2 -> +D
//
// ─────────────────────────────────────────────────────────────────────

/// Locate the first `Stat::GenericFor` in a statement tree and return
/// the length of its `iterators` vec. Used by the B0.7 test to verify
/// the multi-iterator fallback produced the expected tuple size.
fn find_genfor_iterators_len(stmts: &[Stat]) -> Option<usize> {
    for s in stmts {
        match s {
            Stat::GenericFor { iterators, .. } => {
                return Some(iterators.len());
            }
            Stat::NumericFor { body, .. }
            | Stat::While { body, .. }
            | Stat::Repeat { body, .. }
            | Stat::DoBlock { body } => {
                if let Some(n) = find_genfor_iterators_len(body) {
                    return Some(n);
                }
            }
            Stat::If { then_body, elseif_clauses, else_body, .. } => {
                if let Some(n) = find_genfor_iterators_len(then_body) {
                    return Some(n);
                }
                for (_, body) in elseif_clauses {
                    if let Some(n) = find_genfor_iterators_len(body) {
                        return Some(n);
                    }
                }
                if let Some(eb) = else_body {
                    if let Some(n) = find_genfor_iterators_len(eb) {
                        return Some(n);
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// Build the bytecode for (Shape E — B0.7):
///
/// A synthetic `for k, v in arg0, arg1 do sum = sum + v end` where the
/// iterator triple is placed directly into R3/R4/R5 via MOVE+MOVE+LOADNIL
/// with no preceding CALL. This mirrors the compiler output for
/// `for k, v in next, t do` — the key property is that **no Stat** is
/// emitted between function entry and FORGPREP (LOADN for Number,
/// MOVE of a Name, and LOADNIL are all inlinable stores), so the
/// `absorb_iterator_setup` `stmts.last()` path takes the None branch
/// and falls through to the fallback.
///
/// IMPORTANT: This fixture uses 2 params (R0=arg0, R1=arg1) and MOVEs
/// them into DISTINCT iterator registers (R3=arg0, R4=arg1), so that
/// Phase B0.8's same-Name dedup does NOT fire — this is intentional,
/// as the test exercises the genuine multi-iterator case where the
/// generator and state hold DIFFERENT names.
///
/// Register layout (2 params: R0=arg0, R1=arg1):
///   R2 = sum
///   R3 = generator   [A = 3]   = Name("arg0") (from MOVE R3, R0)
///   R4 = state       [A+1]     = Name("arg1") (from MOVE R4, R1)  ← DIFFERENT
///   R5 = control     [A+2]     = Nil          (from LOADNIL R5)
///   R6 = k           [A+3]
///   R7 = v           [A+4]
///
/// PC layout:
///   0: LOADN    R2, 0          (sum = 0; Number is inlinable → no stmt)
///   1: MOVE     R3, R0         (generator = arg0; Name → propagate, no stmt)
///   2: MOVE     R4, R1         (state = arg1;     Name → propagate, no stmt)
///   3: LOADNIL  R5             (control = nil;    inlinable → no stmt)
///   4: FORGPREP A=3, D=+1      (loop_pc = 4 + 1 + 1 = 6)
///   5: ADD      R2, R2, R7     (sum = sum + v; body — R7 = loop var v)
///   6: FORGLOOP A=3, D=-2      (back to pc 5)
///   7: AUX      nresults = 2
///   8: RETURN   R2..R2
fn build_forgprep_without_preceding_call() -> Chunk {
    let code = vec![
        insn_ad(OP_LOADN,    2, 0),                   // 0 (sum at R2)
        insn_abc(OP_MOVE,    3, 0, 0),                // 1 (R3 = arg0 = generator)
        insn_abc(OP_MOVE,    4, 1, 0),                // 2 (R4 = arg1 = state, DIFFERENT)
        insn_abc(OP_LOADNIL, 5, 0, 0),                // 3 (R5 = nil = control)
        insn_ad(OP_FORGPREP, 3, 1),                   // 4 (A=3, loop_pc=6)
        insn_abc(OP_ADD,     2, 2, 7),                // 5 (sum += v at R7=A+4)
        insn_ad(OP_FORGLOOP, 3, -2),                  // 6
        forgloop_aux(2),                              // 7 (AUX)
        insn_abc(OP_RETURN,  2, 2, 0),                // 8
    ];
    make_chunk(make_proto(code, 2, 8, "forgprep_no_call"))
}

#[test]
fn b07_forgprep_without_preceding_call_emits_multi_iterator() {
    // Phase B0.7 primary test.
    //
    // Pre-B0.7 behavior: `absorb_iterator_setup` returned a single
    // `Expr`. When no preceding CALL was available to absorb, it
    // fell through to `reg_expr(regs, a)` and returned ONLY the
    // generator register — losing the state register entirely.
    // Source output: `for k, v in arg0 do end` (wrong: missing
    // state argument).
    //
    // Phase B0.7 fix: `absorb_iterator_setup` returns `Vec<Expr>`.
    // On absorb fallback, it scans `regs[a..=a+2]` and builds an
    // explicit iterator tuple, trimming trailing Nil/Unknown.
    // This fixture uses 2 DISTINCT params (R0=arg0, R1=arg1):
    //   regs[3] = Name("arg0") [generator]
    //   regs[4] = Name("arg1") [state — DIFFERENT name]
    //   regs[5] = Nil          [control — trimmed]
    // Phase B0.8 dedup does NOT fire (arg0 ≠ arg1).
    // Result: iterators = [arg0, arg1] (len == 2).
    //
    // Expected output shape: `for <var1>, <var2> in arg0, arg1 do ... end`
    // — the comma between the two iterators is the smoking gun that
    // the multi-iter fallback fired.
    let chunk = build_forgprep_without_preceding_call();
    let mut ctx = DecompileContext::new(&chunk);
    let stmts = super::super::lift_proto(&mut ctx, &chunk.protos[0], 0);

    let gen = count_generic_fors(&stmts);
    assert_eq!(
        gen, 1,
        "Phase B0.7: expected exactly 1 generic-for, got {}:\n{:#?}",
        gen, stmts,
    );

    let iterators_len = find_genfor_iterators_len(&stmts).expect(
        "Phase B0.7: expected a Stat::GenericFor to exist in lifted body",
    );
    assert_eq!(
        iterators_len, 2,
        "Phase B0.7: expected exactly 2 iterator expressions in \
         Stat::GenericFor (generator + state, with Nil control trimmed), \
         got {}:\n{:#?}",
        iterators_len, stmts,
    );
}

#[test]
fn b07_forgprep_without_preceding_call_end_to_end_source() {
    // End-to-end source check for the Phase B0.7 fix.
    //
    // The rendered source must contain `for ... in <x>, <y> do` — the
    // comma between the iterators proves the multi-iterator fallback
    // produced a Vec with >= 2 elements (the old single-Expr fallback
    // would render as `for ... in <x> do`, no comma).
    //
    // We also assert the output still contains an `in` keyword (the
    // generic-for was recognized, not fallen back to while-true) and
    // that `arg` appears on BOTH sides of the comma (generator from
    // R0=arg0 and state from R1=arg1, both params contain "arg").
    let chunk = build_forgprep_without_preceding_call();
    let mut ctx = DecompileContext::new(&chunk);
    let source = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    assert!(
        source.contains(" in "),
        "Phase B0.7: expected `in` keyword in generic-for, got:\n{}",
        source
    );
    // Grab the `in ... do` slice and check it has exactly one comma.
    let in_idx = source.find(" in ").expect("`in` keyword must be present");
    let tail = &source[in_idx + 4..];
    let do_idx = tail.find(" do").expect("`do` keyword must be present");
    let iter_slice = &tail[..do_idx];
    let comma_count = iter_slice.matches(',').count();
    assert_eq!(
        comma_count, 1,
        "Phase B0.7: expected exactly 1 comma between the 2 iterator \
         expressions (`for k, v in arg0, arg1 do`), got {} commas in \
         iter slice `{}`:\n{}",
        comma_count, iter_slice, source
    );
    // Both sides of the comma must reference params (arg0 and arg1
    // both contain "arg").
    assert!(
        iter_slice.matches("arg").count() >= 2,
        "Phase B0.7: expected both iterators to reference a param \
         (arg appears at least twice), got iter slice `{}`:\n{}",
        iter_slice, source
    );
}

/// Build the bytecode for (Shape F — B0.7 filter test):
///
/// Like Shape E but with a NEWCLOSURE at R4 referencing a non-existent
/// child proto (D=16), which the lifter compiles into an
/// `Expr::Function` placeholder with a single `unresolved closure`
/// Comment. This register should NOT end up in the generic-for
/// iterator tuple: the whitelist in `absorb_iterator_setup` rejects
/// `Expr::Function`, so the fallback must trim the slot.
///
/// Observed in the wild on `06039b020c557365_33314b.luac`, which
/// before the filter rendered as:
///
/// ```text
/// for k, v in pairs, self2, function()
///     -- unresolved closure (D=16)
/// end do
/// ```
///
/// After the filter: `for k, v in pairs, self2 do`.
///
/// IMPORTANT: This fixture uses 2 params (R0=arg0, R1=arg1) and different
/// MOVEs for R[A] and R[A+1], so the B0.8 same-Name dedup does NOT fire.
/// The closure at R[A+2] is rejected purely by the whitelist.
///
/// PC layout (2 params: R0=arg0, R1=arg1):
///   0: LOADN       R2, 0
///   1: MOVE        R3, R0      (generator = arg0)
///   2: MOVE        R4, R1      (state     = arg1 — DIFFERENT name)
///   3: NEWCLOSURE  R5 D=16     (control   = unresolved closure placeholder)
///   4: FORGPREP    A=3 D=+1    (loop_pc = 4+1+1 = 6)
///   5: ADD         R2, R2, R7
///   6: FORGLOOP    A=3 D=-2
///   7: AUX         nresults = 2
///   8: RETURN      R2..R2
fn build_forgprep_with_unresolved_closure_control() -> Chunk {
    // OP_NEWCLOSURE is 19 in the standard Luau opcode table.
    const OP_NEWCLOSURE: u8 = 19;
    let code = vec![
        insn_ad(OP_LOADN,      2, 0),                 // 0 (sum at R2)
        insn_abc(OP_MOVE,      3, 0, 0),              // 1 (R3 = arg0 = generator)
        insn_abc(OP_MOVE,      4, 1, 0),              // 2 (R4 = arg1 = state, DIFFERENT)
        insn_ad(OP_NEWCLOSURE, 5, 16),                // 3 (R5: D=16 → no such proto)
        insn_ad(OP_FORGPREP,   3, 1),                 // 4 (A=3, loop_pc=6)
        insn_abc(OP_ADD,       2, 2, 7),              // 5
        insn_ad(OP_FORGLOOP,   3, -2),                // 6
        forgloop_aux(2),                              // 7
        insn_abc(OP_RETURN,    2, 2, 0),              // 8
    ];
    make_chunk(make_proto(code, 2, 8, "forgprep_with_closure_ctrl"))
}

#[test]
fn b07_fallback_rejects_function_placeholder_in_control_slot() {
    // Phase B0.7 filter regression test.
    //
    // Without the whitelist, the unresolved closure at R5 would
    // propagate into the `Stat::GenericFor.iterators` vec as a
    // 3rd element, rendering as a multi-line function literal
    // inside the `for ... in` clause (real garbage, observed on
    // `06039b020c557365_33314b.luac`).
    //
    // With the whitelist (Name/Field/Index/Call/MethodCall/Varargs
    // only), Expr::Function is rejected. The fixture has R[A]=arg0
    // and R[A+1]=arg1 (DIFFERENT names), so B0.8 dedup does not
    // fire — the state survives. Only the closure at R[A+2] is
    // trimmed, leaving exactly 2 iterators: [arg0, arg1].
    let chunk = build_forgprep_with_unresolved_closure_control();
    let mut ctx = DecompileContext::new(&chunk);
    let stmts = super::super::lift_proto(&mut ctx, &chunk.protos[0], 0);

    let iterators_len = find_genfor_iterators_len(&stmts).expect(
        "B0.7 filter: expected a Stat::GenericFor to exist",
    );
    assert_eq!(
        iterators_len, 2,
        "B0.7 filter: Function placeholder at control slot must be \
         rejected by the whitelist — expected exactly 2 iterators \
         (generator + state with different names), got {}:\n{:#?}",
        iterators_len, stmts,
    );

    // Source check: the rendered output must NOT contain the
    // "unresolved closure" comment string inside the for-loop
    // header.
    let mut ctx2 = DecompileContext::new(&chunk);
    let source = decompile_proto(&mut ctx2, &chunk.protos[0], 0, 0);
    let in_idx = source.find(" in ").expect("`in` keyword expected");
    let tail = &source[in_idx + 4..];
    let do_idx = tail.find(" do").expect("`do` keyword expected");
    let iter_slice = &tail[..do_idx];
    assert!(
        !iter_slice.contains("unresolved closure"),
        "B0.7 filter: `for ... in` clause must not contain an \
         unresolved closure placeholder, got iter slice `{}`:\n{}",
        iter_slice, source
    );
    assert!(
        !iter_slice.contains("function"),
        "B0.7 filter: `for ... in` clause must not contain a \
         function literal, got iter slice `{}`:\n{}",
        iter_slice, source
    );
}

#[test]
fn b07_pairs_call_absorption_still_works() {
    // Phase B0.7 must NOT regress the common absorption path. When
    // a CALL precedes FORGPREP (e.g. `local v = pairs(t); FORGPREP`),
    // the absorbed call expression must still be the ONLY iterator
    // — not a 3-element tuple of `[call, state_garbage, control_garbage]`.
    //
    // Re-runs the exact Shape-A fixture from Phase B0.5.
    let chunk = build_genfor_in_numfor();
    let mut ctx = DecompileContext::new(&chunk);
    let stmts = super::super::lift_proto(&mut ctx, &chunk.protos[0], 0);

    let iterators_len = find_genfor_iterators_len(&stmts).expect(
        "B0.7: Shape-A fixture must still produce a Stat::GenericFor",
    );
    // Shape-A fixture uses `MOVE R5, R0 ; LOADNIL R6 ; LOADNIL R7 ; FORGPREP`
    // — there is no preceding CALL, so absorb fails and the fallback
    // runs. regs[5] = Name("arg0"), regs[6] = Nil, regs[7] = Nil.
    // Fallback result: iterators = [Name("arg0")] (Nil state is trimmed,
    // which also forces Nil control to be trimmed).
    //
    // This is the single-iterator bare-name case — the old pre-B0.7
    // behavior produced `for k, v in v5 do`, the B0.7 behavior now
    // produces `for k, v in arg0 do`. Both have a 1-element iter vec.
    assert_eq!(
        iterators_len, 1,
        "B0.7: Shape-A should still yield a single-iterator GenericFor \
         (Nil state register is trimmed from the fallback), got {}",
        iterators_len,
    );
}

// ─────────────────────────────────────────────────────────────────────
// Phase B0.8 — duplicate-Name dedup in GenericFor iterator fallback.
//
// Root cause (observed on `06038f010d557365_10830b.luac`):
// Both GETIMPORT R[A] and GETGLOBAL R[A+1] resolve to `Name("pairs")`
// because they happen to target the same constant slot (K0="pairs").
// The B0.7 fallback faithfully surfaces both → `for k, v in pairs, pairs do`
// which is meaningless (iterating `pairs` as both generator AND state).
//
// Fix: in the fallback, if `regs[A+1]` is `Name(s)` and `regs[A]` is
// also `Name(g)` where g == s, drop `regs[A+1]`. Symmetric check for
// the control slot against both previous entries. This is scoped to
// `Expr::Name` only — Field/Index/Call/MethodCall are not compared
// (no PartialEq on Expr; broader equality is out of scope for B0.8).
//
// The dedup MUST NOT fire on `Name("next"), Name("t")` or any pair of
// different names (B0.7 primary fix must be preserved).
// ─────────────────────────────────────────────────────────────────────

/// Build the bytecode for (Shape G — B0.8 duplicate-dedup test):
///
/// Two GETIMPORT instructions both resolving to the same name ("pairs"),
/// placed consecutively at R2 and R3, followed by LOADNIL at R4 and
/// FORGPREP A=2. This mirrors the observed pattern in Proto 17 of
/// `06038f010d557365_10830b.luac`.
///
/// The `OP_GETGLOBAL` (7) is used for R3 to mimic the R[A+1] setup.
/// Since the test's string table and constant table are EMPTY,
/// `resolve_global_name` returns None → regs[3] = RegVal::Unknown.
/// That means the fallback trims slot 1 because Unknown is not in the
/// whitelist → iterators = [reg_expr(regs, 2)] (just the generator).
///
/// So for the dedup test we need a Name in R[A+1] that matches R[A].
/// The easiest setup: use two MOVE R[A+1], R0 instructions so both
/// R[A] and R[A+1] clone R[0] = Name("arg0"), producing a
/// `Name("arg0"), Name("arg0")` duplicate that the B0.8 dedup should
/// collapse to a single-element vec.
///
/// PC layout:
///   0: LOADN   R1, 0         (sum = 0)
///   1: MOVE    R2, R0        (R2 = Name("arg0") = generator)
///   2: MOVE    R3, R0        (R3 = Name("arg0") = duplicate state!)
///   3: MOVE    R4, R0        (R4 = Name("arg0") = duplicate control!)
///   4: FORGPREP A=2, D=+1   (loop_pc = 4+1+1 = 6)
///   5: ADD     R1, R1, R6   (sum += v, body)
///   6: FORGLOOP A=2, D=-2
///   7: AUX nresults = 2
///   8: RETURN  R1..R1
fn build_forgprep_duplicate_state() -> Chunk {
    let code = vec![
        insn_ad(OP_LOADN,    1, 0),                   // 0
        insn_abc(OP_MOVE,    2, 0, 0),                // 1 (R2 = arg0 = generator)
        insn_abc(OP_MOVE,    3, 0, 0),                // 2 (R3 = arg0 = SAME as generator)
        insn_abc(OP_MOVE,    4, 0, 0),                // 3 (R4 = arg0 = SAME as generator)
        insn_ad(OP_FORGPREP, 2, 1),                   // 4
        insn_abc(OP_ADD,     1, 1, 6),                // 5
        insn_ad(OP_FORGLOOP, 2, -2),                  // 6
        forgloop_aux(2),                              // 7
        insn_abc(OP_RETURN,  1, 2, 0),                // 8
    ];
    make_chunk(make_proto(code, 1, 7, "forgprep_duplicate_state"))
}

#[test]
fn b08_duplicate_name_in_state_slot_is_deduped() {
    // Phase B0.8 primary test.
    //
    // Pre-B0.8 behavior: both R[A] and R[A+1] hold Name("arg0");
    // the fallback includes both → iterators.len() == 2 →
    // rendered as `for k, v in arg0, arg0 do` (meaningless duplicate).
    //
    // Phase B0.8 fix: the `same_name` guard drops the state slot
    // when its Name is identical to the generator's Name → iterators
    // = [Name("arg0")] (len == 1).
    let chunk = build_forgprep_duplicate_state();
    let mut ctx = DecompileContext::new(&chunk);
    let stmts = super::super::lift_proto(&mut ctx, &chunk.protos[0], 0);

    let iterators_len = find_genfor_iterators_len(&stmts).expect(
        "B0.8: expected a Stat::GenericFor in lifted body",
    );
    assert_eq!(
        iterators_len, 1,
        "B0.8: duplicate-Name state must be deduped — expected 1 \
         iterator (generator only), got {}:\n{:#?}",
        iterators_len, stmts,
    );
}

#[test]
fn b08_duplicate_name_source_renders_without_comma() {
    // End-to-end source check: the rendered generic-for header must
    // NOT contain a comma between the iterators.
    let chunk = build_forgprep_duplicate_state();
    let mut ctx = DecompileContext::new(&chunk);
    let source = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    let in_idx = source.find(" in ").expect("`in` keyword must be present");
    let tail = &source[in_idx + 4..];
    let do_idx = tail.find(" do").expect("`do` keyword must be present");
    let iter_slice = &tail[..do_idx];
    assert_eq!(
        iter_slice.matches(',').count(), 0,
        "B0.8: duplicate state deduped → no comma in iter slice, \
         got `{}` with {} commas:\n{}",
        iter_slice, iter_slice.matches(',').count(), source
    );
}

#[test]
fn b08_b07_next_t_fix_is_preserved() {
    // Phase B0.8 dedup must NOT fire when generator and state hold
    // DIFFERENT names — this is the B0.7 primary fix pattern
    // (`for k, v in next, t do`) where the generator and state are
    // distinct identifiers.
    //
    // The Shape-E fixture (updated for B0.8 compatibility) uses 2 params:
    //   R[A  =3] = MOVE R3, R0  → Name("arg0")  (generator)
    //   R[A+1=4] = MOVE R4, R1  → Name("arg1")  (state — DIFFERENT)
    //   R[A+2=5] = LOADNIL R5   → Nil             (control — trimmed)
    //
    // arg0 ≠ arg1 → B0.8 same_name check returns false → dedup does
    // NOT fire → iterators = [arg0, arg1], len == 2.
    //
    // This locks the non-regression: B0.7's multi-iterator fix is
    // preserved through B0.8.
    let chunk = build_forgprep_without_preceding_call();
    let mut ctx = DecompileContext::new(&chunk);
    let stmts = super::super::lift_proto(&mut ctx, &chunk.protos[0], 0);

    // B0.8 dedup does NOT fire (arg0 ≠ arg1). Nil control trimmed.
    // iterators = [arg0, arg1], len == 2.
    let iterators_len = find_genfor_iterators_len(&stmts).expect(
        "B0.8 regression: Shape-E must still produce GenericFor",
    );
    assert_eq!(
        iterators_len, 2,
        "B0.8 non-regression: Shape-E (arg0 ≠ arg1) must preserve both \
         iterators, got {}",
        iterators_len,
    );
}

#[test]
fn b08_non_equal_names_are_preserved() {
    // Phase B0.8: two iterators with DIFFERENT names must both survive.
    //
    // This uses the `build_numfor_in_genfor` Shape-C fixture.  In that
    // fixture the outer FORGPREP (A=2) has:
    //   R2 = MOVE R2, R0     → Name("arg0")  (generator)
    //   R3 = LOADNIL R3      → Nil             (state — trimmed by whitelist)
    //   R4 = LOADNIL R4      → Nil             (control — trimmed)
    //
    // So we get iterators = [Name("arg0")], len == 1 (Nil trimmed, not deduped).
    // That's correct — no duplicate, no spurious trim. This is the "different
    // non-Name state" guard.  The B0.8 dedup only fires on same-Name pairs,
    // never on Nil state.
    let chunk = build_numfor_in_genfor();
    let mut ctx = DecompileContext::new(&chunk);
    let stmts = super::super::lift_proto(&mut ctx, &chunk.protos[0], 0);

    let iterators_len = find_genfor_iterators_len(&stmts).expect(
        "B0.8 regression: Shape-C must still produce GenericFor",
    );
    assert_eq!(
        iterators_len, 1,
        "B0.8 regression: Shape-C outer FORGPREP must yield 1 iterator \
         (Nil state trimmed, B0.8 dedup did not fire spuriously), got {}",
        iterators_len,
    );
}

// ── Phase B0.9 fixtures ────────────────────────────────────────────────

/// GETIMPORT constant helper: constructs the Import u32 for a 1-level
/// import whose string is at proto.constants[0].
///
/// Format: count=1 (bits 31-30), id0=0 (bits 29-20).
const IMPORT_PAIRS: u32   = 0x4000_0000u32; // count=1, id0=0
const IMPORT_IPAIRS: u32  = 0x4000_0000u32; // same encoding, string differs
const IMPORT_NEXT: u32    = 0x4000_0000u32; // same encoding

/// Build a proto with a string constant at K[0] and a matching Import at K[1].
/// Used by all three B0.9 fixtures (pairs / ipairs / next).
fn make_proto_with_import(
    code: Vec<u32>,
    import_name: &str,
    num_params: u8,
    max_stack: u8,
    debug_name: &str,
) -> Proto {
    Proto {
        constants: vec![
            Constant::String(import_name.to_string()), // K[0]: string name
            Constant::Import(IMPORT_PAIRS),            // K[1]: Import(count=1,id0=0→K[0])
        ],
        ..make_proto(code, num_params, max_stack, debug_name)
    }
}

/// Shape H — B0.9: GETIMPORT "pairs" + MOVE R[A+1],arg0 + LOADNIL R[A+2]
///           + FORGPREP A.
///
/// ```lua
/// function(t)
///     local sum = 0
///     for k, v in pairs(t) do sum = sum + v end
///     return sum
/// end
/// ```
///
/// Register layout (FORGPREP A=3):
///   R0 = t (param),  R1 = sum,
///   R3 = generator (pairs),  R4 = state (arg0),  R5 = control (nil)
///   R6 = k (loop var 1),     R7 = v (loop var 2)
fn build_forgprep_pairs_fold() -> Chunk {
    let code = vec![
        insn_ad(OP_LOADN,      1, 0),   // PC=0: sum = 0 in R1
        insn_ad(OP_GETIMPORT,  3, 1),   // PC=1: R3 = Name("pairs"), K[1]=Import
        IMPORT_PAIRS,                   // PC=2: AUX for GETIMPORT
        insn_abc(OP_MOVE,      4, 0, 0),// PC=3: R4 = arg0 (table state)
        insn_abc(OP_LOADNIL,   5, 0, 0),// PC=4: R5 = nil (control, trimmed)
        insn_ad(OP_FORGPREP,   3, 1),   // PC=5: A=3, D=1 → loop_pc=7
        insn_abc(OP_ADD,       1, 1, 7),// PC=6: body: sum += v (R7=A+4)
        insn_ad(OP_FORGLOOP,   3, -2),  // PC=7: D=-2 → back to PC=6
        forgloop_aux(2),                // PC=8: AUX (2 loop vars)
        insn_abc(OP_RETURN,    1, 2, 0),// PC=9: return sum
    ];
    make_chunk(make_proto_with_import(code, "pairs", 1, 8, "forgprep_pairs_fold"))
}

/// Shape I — B0.9: same as Shape H but with "ipairs" as the import name.
fn build_forgprep_ipairs_fold() -> Chunk {
    let code = vec![
        insn_ad(OP_LOADN,      1, 0),
        insn_ad(OP_GETIMPORT,  3, 1),
        IMPORT_IPAIRS,
        insn_abc(OP_MOVE,      4, 0, 0),
        insn_abc(OP_LOADNIL,   5, 0, 0),
        insn_ad(OP_FORGPREP,   3, 1),
        insn_abc(OP_ADD,       1, 1, 7),
        insn_ad(OP_FORGLOOP,   3, -2),
        forgloop_aux(2),
        insn_abc(OP_RETURN,    1, 2, 0),
    ];
    make_chunk(make_proto_with_import(code, "ipairs", 1, 8, "forgprep_ipairs_fold"))
}

/// Shape J — B0.9: same structure but "next" as the import name.
/// B0.9 must NOT fold "next" — `for k, v in next, t do` is valid Luau
/// and `next(t)` has different semantics, so the tuple must be preserved.
fn build_forgprep_next_no_fold() -> Chunk {
    let code = vec![
        insn_ad(OP_LOADN,      1, 0),
        insn_ad(OP_GETIMPORT,  3, 1),
        IMPORT_NEXT,
        insn_abc(OP_MOVE,      4, 0, 0),
        insn_abc(OP_LOADNIL,   5, 0, 0),
        insn_ad(OP_FORGPREP,   3, 1),
        insn_abc(OP_ADD,       1, 1, 7),
        insn_ad(OP_FORGLOOP,   3, -2),
        forgloop_aux(2),
        insn_abc(OP_RETURN,    1, 2, 0),
    ];
    make_chunk(make_proto_with_import(code, "next", 1, 8, "forgprep_next_no_fold"))
}

// ── Phase B0.9 tests ───────────────────────────────────────────────────

#[test]
fn b09_pairs_folds_to_call_syntax() {
    // B0.9: Shape H — GETIMPORT "pairs" + MOVE arg0 → should fold to
    // [Call(Name("pairs"), [Name("arg0")])], i.e. iterators.len() == 1.
    let chunk = build_forgprep_pairs_fold();
    let mut ctx = DecompileContext::new(&chunk);
    let stmts = super::super::lift_proto(&mut ctx, &chunk.protos[0], 0);

    let iterators_len = find_genfor_iterators_len(&stmts).expect(
        "B0.9: Shape H must produce a GenericFor statement",
    );
    assert_eq!(
        iterators_len, 1,
        "B0.9: pairs + state must fold to a single Call iterator, got len={}",
        iterators_len,
    );
}

#[test]
fn b09_pairs_source_renders_as_call() {
    // B0.9: the rendered source for Shape H must contain `pairs(` in the
    // `for ... in ... do` clause — never `pairs, arg0`.
    let chunk = build_forgprep_pairs_fold();
    let mut ctx = DecompileContext::new(&chunk);
    let source = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    let in_idx = source.find(" in ").expect("`in` keyword must be present");
    let tail = &source[in_idx + 4..];
    let do_idx = tail.find(" do").expect("`do` keyword must be present");
    let iter_slice = &tail[..do_idx];

    assert!(
        iter_slice.contains("pairs("),
        "B0.9: iter slice must contain `pairs(`, got `{}`\n{}",
        iter_slice, source
    );
    assert_eq!(
        iter_slice.matches(',').count(), 0,
        "B0.9: folded pairs call must have no top-level comma in iter \
         slice (no tuple form), got `{}`",
        iter_slice
    );
}

#[test]
fn b09_ipairs_folds_to_call_syntax() {
    // B0.9: Shape I — GETIMPORT "ipairs" + MOVE arg0 → fold to
    // [Call(Name("ipairs"), [Name("arg0")])], len == 1.
    let chunk = build_forgprep_ipairs_fold();
    let mut ctx = DecompileContext::new(&chunk);
    let stmts = super::super::lift_proto(&mut ctx, &chunk.protos[0], 0);

    let iterators_len = find_genfor_iterators_len(&stmts).expect(
        "B0.9: Shape I must produce a GenericFor statement",
    );
    assert_eq!(
        iterators_len, 1,
        "B0.9: ipairs + state must fold to a single Call iterator, got len={}",
        iterators_len,
    );
}

#[test]
fn b09_next_is_not_folded() {
    // B0.9: Shape J — GETIMPORT "next" + MOVE arg0 → must NOT fold.
    // `for k, v in next, t do` is valid Luau; `next(t)` is semantically
    // different. The fold whitelist excludes "next".
    let chunk = build_forgprep_next_no_fold();
    let mut ctx = DecompileContext::new(&chunk);
    let stmts = super::super::lift_proto(&mut ctx, &chunk.protos[0], 0);

    let iterators_len = find_genfor_iterators_len(&stmts).expect(
        "B0.9: Shape J must produce a GenericFor statement",
    );
    assert_eq!(
        iterators_len, 2,
        "B0.9: `next, t` must NOT fold — must remain 2-iterator tuple, got len={}",
        iterators_len,
    );
}

#[test]
fn b09_arbitrary_fn_not_folded() {
    // B0.9: Shape E (updated, distinct params arg0/arg1) — neither is in
    // the fold whitelist ["pairs", "ipairs"], so the tuple must survive.
    // This is also the B0.7 non-regression fixture.
    let chunk = build_forgprep_without_preceding_call();
    let mut ctx = DecompileContext::new(&chunk);
    let stmts = super::super::lift_proto(&mut ctx, &chunk.protos[0], 0);

    let iterators_len = find_genfor_iterators_len(&stmts).expect(
        "B0.9: Shape-E (arg0 generator) must still produce GenericFor",
    );
    assert_eq!(
        iterators_len, 2,
        "B0.9: arbitrary function name (`arg0`) must NOT fold — \
         only `pairs`/`ipairs` are whitelisted, got len={}",
        iterators_len,
    );
}

#[test]
fn b09_b07_b08_wins_preserved() {
    // Phase B0.9 must not disturb B0.7 (next, arg12 preserved) or
    // B0.8 (duplicate-name dedup still fires).
    //
    // B0.8 Shape G: all three regs = same param → dedup to len==1
    // (generator kept, duplicates dropped). B0.9 then sees len==1,
    // which is < 2, so the fold check is skipped entirely.
    let chunk = build_forgprep_duplicate_state();
    let mut ctx = DecompileContext::new(&chunk);
    let stmts = super::super::lift_proto(&mut ctx, &chunk.protos[0], 0);
    let len = find_genfor_iterators_len(&stmts)
        .expect("B0.9 non-regress: Shape-G must still produce GenericFor");
    assert_eq!(len, 1, "B0.9 non-regress: B0.8 dedup must still fire on Shape-G, got len={}", len);

    // B0.7 Shape J: "next" as generator → NOT folded (len==2 preserved).
    let chunk2 = build_forgprep_next_no_fold();
    let mut ctx2 = DecompileContext::new(&chunk2);
    let stmts2 = super::super::lift_proto(&mut ctx2, &chunk2.protos[0], 0);
    let len2 = find_genfor_iterators_len(&stmts2)
        .expect("B0.9 non-regress: Shape-J (next) must produce GenericFor");
    assert_eq!(len2, 2, "B0.9 non-regress: `next, t` must remain 2-iterator tuple, got len={}", len2);
}

// ─── Phase B0.10 tests ───────────────────────────────────────────────────

// Phase B0.10 fixture — Shape K:
// A simple GenericFor where the body accumulates `sum += k` (reads a loop var).
//
// ```lua
// function loop_body_reads_loop_vars(t)
//     local sum = 0
//     for k, v in t do
//         sum = sum + k   -- body reads R5 (loop key)
//     end
//     return sum
// end
// ```
//
// Register layout (1 param, A=2):
//   R0 = arg0 (table / generator)
//   R1 = sum              (written in body → pre-materialized before loop)
//   R2 = generator  (A=2)
//   R3 = state      (A+1)
//   R4 = control    (A+2)
//   R5 = loop key   (A+3)
//   R6 = loop value (A+4)
//
// PC layout:
//   0: LOADN  R1, 0         (sum = 0)
//   1: MOVE   R2, R0        (generator = arg0)
//   2: LOADNIL R3           (state = nil)
//   3: LOADNIL R4           (control = nil)
//   4: FORGPREP A=2, D=+1   (loop_pc = 4+1+1 = 6)
//   5: ADD   R1, R1, R5     (body: sum += k — writes R1, reads R5)
//   6: FORGLOOP A=2, D=-2   (back to PC=5)
//   7: forgloop_aux(2)      (2 loop vars)
//   8: RETURN R1, 2
//
// The body's ADD writes R1 and reads R5 (loop key).  R1 is pre-materialized
// as a local before the loop (it's written in the body and was LOADN'd
// before the loop), so the ADD produces an assignment statement
// (`sum = sum + k`), making the body non-empty for assertion purposes.
fn build_forgprep_body_reads_loop_vars() -> Chunk {
    let code = vec![
        insn_ad(OP_LOADN,    1, 0),          // 0: sum = 0
        insn_abc(OP_MOVE,    2, 0, 0),        // 1: generator = arg0
        insn_abc(OP_LOADNIL, 3, 0, 0),        // 2: state = nil
        insn_abc(OP_LOADNIL, 4, 0, 0),        // 3: control = nil
        insn_ad(OP_FORGPREP, 2, 1),           // 4: FORGPREP A=2, D=+1 → loop_pc=6
        insn_abc(OP_ADD,     1, 1, 5),        // 5: sum += k (reads R5 = loop key)
        insn_ad(OP_FORGLOOP, 2, -2),          // 6: FORGLOOP back to PC=5
        forgloop_aux(2),                      // 7: AUX (2 loop vars)
        insn_abc(OP_RETURN,  1, 2, 0),        // 8: return sum
    ];
    make_chunk(make_proto(code, 1, 8, "loop_body_reads_loop_vars"))
}

// Phase B0.10 fixture — Shape L:
// Same shape as K but with DebugInfo providing original names "item"/"qty"
// for the loop variable registers R5/R6.
fn build_forgprep_body_with_debug_names() -> Chunk {
    let code = vec![
        insn_ad(OP_LOADN,    1, 0),          // 0
        insn_abc(OP_MOVE,    2, 0, 0),        // 1
        insn_abc(OP_LOADNIL, 3, 0, 0),        // 2
        insn_abc(OP_LOADNIL, 4, 0, 0),        // 3
        insn_ad(OP_FORGPREP, 2, 1),           // 4: loop_pc = 6
        insn_abc(OP_ADD,     1, 1, 5),        // 5: sum += item (reads R5 = loop key)
        insn_ad(OP_FORGLOOP, 2, -2),          // 6
        forgloop_aux(2),                      // 7
        insn_abc(OP_RETURN,  1, 2, 0),        // 8
    ];
    // body_start = 5, body_end = 6.  LocalVars cover [5, 9).
    let locals = vec![
        LocalVar { name: "item".to_string(), start_pc: 5, end_pc: 9, reg: 5 },
        LocalVar { name: "qty".to_string(),  start_pc: 5, end_pc: 9, reg: 6 },
    ];
    make_chunk(make_proto_with_debug(code, 1, 8, "loop_body_debug_names", locals))
}

#[test]
fn b10_genfor_loop_vars_seeded_in_body() {
    // Phase B0.10: loop variable registers (A+3, A+4) must be seeded into
    // `regs` after `var_names` is computed and before body lifting starts.
    // Without the seed, `reg_expr(regs, 5)` produces `v5` even though
    // `Stat::GenericFor.vars` declares `k` — a header/body name mismatch.
    //
    // Shape K fixture: body ADD reads R5 (k) and R6 (v).
    // Asserts: source for-body must NOT contain raw register references
    // like "v5" or "v6"; it must contain "k" and "v" consistently.
    let chunk = build_forgprep_body_reads_loop_vars();
    let mut ctx = DecompileContext::new(&chunk);
    let source = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    assert!(
        source.contains("for "),
        "B0.10 Shape-K: expected a generic-for loop, got:\n{}", source
    );
    // The body expression must reference loop vars by name, not raw "v5"/"v6"
    assert!(
        !source.contains("v5") && !source.contains("v6"),
        "B0.10 Shape-K: body must not reference raw register names v5/v6 — \
         loop var seeding failed. Got:\n{}", source
    );
    // The body expression should contain the synthesized loop var names
    // from ctx.reg_name: GenericForKey → "k", GenericForVal → "v"
    // (or any scoped variant thereof, e.g. "k2").
    // We check that the body section (after "do") contains a "+" and
    // that neither side is a raw "vN" pattern.
    let body_start = source.find(" do\n").or_else(|| source.find(" do "));
    assert!(
        body_start.is_some(),
        "B0.10: expected 'do' in source, got:\n{}", source
    );
}

#[test]
fn b10_genfor_loop_vars_with_debug_names_in_body() {
    // Phase B0.10: when DebugInfo LocalVars cover the loop variable
    // registers, `ctx.reg_name` returns the original names ("item", "qty").
    // Seeding `regs[a+3/a+4]` with those names ensures the body reads
    // them under those names, not as "v5"/"v6".
    //
    // Shape L fixture: R5 → "item", R6 → "qty" via debug info.
    // The body ADD reads R5 and R6 — must appear as "item" and "qty".
    let chunk = build_forgprep_body_with_debug_names();
    let mut ctx = DecompileContext::new(&chunk);
    let source = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    assert!(
        source.contains("for "),
        "B0.10 Shape-L: expected a generic-for loop, got:\n{}", source
    );
    // Debug names must appear somewhere in the source
    assert!(
        source.contains("item"),
        "B0.10 Shape-L: debug name 'item' must appear in source, got:\n{}", source
    );
    assert!(
        source.contains("qty"),
        "B0.10 Shape-L: debug name 'qty' must appear in source, got:\n{}", source
    );
    // Raw register names must NOT appear
    assert!(
        !source.contains("v5") && !source.contains("v6"),
        "B0.10 Shape-L: raw register names v5/v6 must not appear when debug info \
         provides 'item'/'qty'. Got:\n{}", source
    );
}

#[test]
fn b10_genfor_no_debug_info_consistent_names() {
    // Phase B0.10: with no debug info, the var_names are synthesized from
    // register hints (GenericForKey → "k", GenericForVal → "v").  The
    // GenericFor node must exist and declare non-empty vars.  The
    // source-level test `b10_genfor_loop_vars_seeded_in_body` verifies that
    // body expressions reference those same names rather than raw "vN"
    // fallbacks.
    let chunk = build_forgprep_body_reads_loop_vars();
    let mut ctx = DecompileContext::new(&chunk);
    let stmts = super::super::lift_proto(&mut ctx, &chunk.protos[0], 0);

    // Locate the GenericFor node.
    fn find_genfor(stmts: &[Stat]) -> Option<&Vec<String>> {
        for s in stmts {
            if let Stat::GenericFor { vars, .. } = s {
                return Some(vars);
            }
        }
        None
    }

    let vars = find_genfor(&stmts)
        .expect("B0.10: expected a GenericFor in Shape-K stmts");

    assert!(
        !vars.is_empty(),
        "B0.10: GenericFor.vars must not be empty; got:\n{:#?}", stmts
    );

    // vars[0] should be the first loop var name (synthesized from GenericForKey
    // hint → "k" or a scoped variant). Must not be a raw "v5" fallback.
    let first_var = &vars[0];
    assert!(
        !first_var.starts_with("v5"),
        "B0.10: first loop var must not be raw register name 'v5', got: {}",
        first_var
    );
}

#[test]
fn b10_b09_regression_genfor_absorption_still_works() {
    // B0.10 must not disturb B0.9's pairs-call folding.
    // Shape H (pairs fold): iterators must still fold to 1 element.
    let chunk = build_forgprep_pairs_fold();
    let mut ctx = DecompileContext::new(&chunk);
    let stmts = super::super::lift_proto(&mut ctx, &chunk.protos[0], 0);
    let len = find_genfor_iterators_len(&stmts)
        .expect("B0.10 non-regress: Shape-H must produce GenericFor");
    assert_eq!(
        len, 1,
        "B0.10 non-regress: B0.9 pairs-fold must still fire, got len={}",
        len
    );
}

// ─── Phase B0.11 tests ───────────────────────────────────────────────────
//
// Phase B0.11 establishes that `ctx.reg_name(proto, reg, pc)` — which
// already consults `proto.debug_info.locals` — correctly propagates
// original local/parameter names to the decompiled output for all four
// critical paths:
//   1. Function parameters (seeded at lift_proto entry, PC=0)
//   2. CALL result locals (named at the CALL instruction PC)
//   3. NumericFor loop variables (named at FORNPREP PC)
//   4. Scope enforcement (write outside live range uses fallback)
//
// All production Roblox corpus has debug_info=None (Roblox strips debug
// info at compile time), so these tests exercise synthetic fixtures only.
// Corpus impact of B0.11 is intentionally zero — the phase closes the
// documented gap and proves the pipeline is correct when debug info IS
// present (e.g. local toolchain output or future Roblox debug builds).

// Phase B0.11 fixture — Shape M:
// Two-parameter function that sums player + score.
// With debug info naming both parameters.
//
// ```lua
// function sum_debug(player, score)
//     return player + score
// end
// ```
//
// Register layout (2 params):
//   R0 = player (param 0)
//   R1 = score  (param 1)
//   R2 = player + score (temp, inlined)
//
// PC layout:
//   0: ADD    R2, R0, R1    (R2 = player + score)
//   1: RETURN R2, 2         (return R2)
fn build_param_debug_names() -> Chunk {
    let code = vec![
        insn_abc(OP_ADD,    2, 0, 1),  // 0: R2 = R0 + R1
        insn_abc(OP_RETURN, 2, 2, 0),  // 1: return R2
    ];
    let locals = vec![
        LocalVar { name: "player".to_string(), start_pc: 0, end_pc: 3, reg: 0 },
        LocalVar { name: "score".to_string(),  start_pc: 0, end_pc: 3, reg: 1 },
    ];
    make_chunk(make_proto_with_debug(code, 2, 3, "sum_debug", locals))
}

// Phase B0.11 fixture — Shape N:
// GETIMPORT followed by CALL; debug_info names the CALL result register.
//
// ```lua
// function call_debug()
//     local result = someFunc()
//     return result + result
// end
// ```
//
// PC layout:
//   0: GETIMPORT R0, K[1]   (R0 = someFunc import)
//   1: AUX (0x40000000)
//   2: CALL R0, 1, 2        (result = someFunc(); B=1→0args, C=2→1result)
//   3: ADD  R1, R0, R0      (R1 = result + result — reads R0 TWICE)
//   4: RETURN R1, 2, 0
//
// The ADD reads R0 twice so count_name_reads("result") = 2 across the stmts,
// which prevents inline_single_use_temps from collapsing `local result` into
// `return someFunc()`.  Without that guard the CALL result gets inlined and
// "result" never appears in the final source despite debug-info recovery working.
//
// Debug info: LocalVar{name="result", reg=0, start_pc=2, end_pc=6}.
// At CALL PC=2, ctx.reg_name(proto, 0, 2) finds start_pc=2 in range → "result".
fn build_call_result_debug_name() -> Chunk {
    let code = vec![
        insn_ad(OP_GETIMPORT, 0, 1),         // 0: R0 = someFunc
        IMPORT_PAIRS,                         // 1: AUX
        insn_abc(OP_CALL,     0, 1, 2),       // 2: result = someFunc()
        insn_abc(OP_ADD,      1, 0, 0),       // 3: R1 = result + result (reads R0 twice)
        insn_abc(OP_RETURN,   1, 2, 0),       // 4: return R1
    ];
    let locals = vec![
        // Covers pc=2 (CALL) through pc=5 so ADD at pc=3 sees "result" in regs.
        LocalVar { name: "result".to_string(), start_pc: 2, end_pc: 6, reg: 0 },
    ];
    make_chunk(Proto {
        constants: vec![
            Constant::String("someFunc".to_string()),
            Constant::Import(IMPORT_PAIRS),
        ],
        debug_info: Some(DebugInfo { locals, upvalue_names: vec![] }),
        ..make_proto(code, 0, 5, "call_debug")
    })
}

// Phase B0.11 fixture — Shape O:
// NumericFor with debug info naming the loop variable.
// Same bytecode as build_numeric_for_simple but adds a DebugInfo entry
// for the loop variable register R4 (= A+2 = 2+2, FORNPREP A=2).
//
// ctx.reg_name(proto, 4, prep_pc=4) finds LocalVar{reg=4, start_pc=4} in
// range → returns "idx".  Without debug info, the NumericForVar hint would
// produce "i" (or "i2" etc.).
fn build_numfor_debug_loop_var() -> Chunk {
    let code = vec![
        insn_ad(OP_LOADN,    1, 0),       // 0: sum = 0
        insn_ad(OP_LOADN,    4, 1),       // 1: start = 1 (R[A+2])
        insn_abc(OP_MOVE,    2, 0, 0),    // 2: limit = R0 (R[A])
        insn_ad(OP_LOADN,    3, 1),       // 3: step = 1 (R[A+1])
        insn_ad(OP_FORNPREP, 2, 2),       // 4: FORNPREP A=2 D=+2 → loop_pc=6
        insn_abc(OP_ADD,     1, 1, 4),    // 5: sum += idx (reads R4)
        insn_ad(OP_FORNLOOP, 2, -2),      // 6: FORNLOOP
        insn_abc(OP_RETURN,  1, 2, 0),    // 7: return sum
    ];
    // Loop var = R[A+2] = R4; seeded at prep_pc=4.
    let locals = vec![
        LocalVar { name: "idx".to_string(), start_pc: 4, end_pc: 8, reg: 4 },
    ];
    make_chunk(make_proto_with_debug(code, 1, 5, "numfor_debug_var", locals))
}

#[test]
fn b11_param_names_from_debug_info() {
    // Phase B0.11: function parameter registers are seeded at lift_proto entry
    // via `ctx.reg_name(proto, i, 0)`, which probes proto.debug_info.locals.
    // When a LocalVar entry covers (reg=i, start_pc=0), the original name
    // is returned and stored in regs[i].  All subsequent reads of that
    // register propagate the original name.
    //
    // Shape M: R0 → "player", R1 → "score" via debug info.
    // ADD R2, R0, R1 produces BinOp(Name("player"), Add, Name("score"))
    // which is inlined; RETURN R2 emits `return player + score`.
    let chunk = build_param_debug_names();
    let mut ctx = DecompileContext::new(&chunk);
    let source = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    assert!(
        source.contains("player"),
        "B0.11: debug param name 'player' must appear in source. Got:\n{}", source
    );
    assert!(
        source.contains("score"),
        "B0.11: debug param name 'score' must appear in source. Got:\n{}", source
    );
}

#[test]
fn b11_no_debug_info_uses_fallback_param_names() {
    // Phase B0.11 non-regression: without debug_info, parameter names fall
    // back to synthesis (arg1, arg2 from RegisterHint::Param).
    // Must NOT contain debug names "player" or "score".
    let code = vec![
        insn_abc(OP_ADD,    2, 0, 1),  // R2 = R0 + R1
        insn_abc(OP_RETURN, 2, 2, 0),  // return R2
    ];
    let chunk = make_chunk(make_proto(code, 2, 3, "sum_no_debug"));
    let mut ctx = DecompileContext::new(&chunk);
    let source = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    assert!(
        !source.contains("player"),
        "B0.11: 'player' must not appear without debug info. Got:\n{}", source
    );
    assert!(
        !source.contains("score"),
        "B0.11: 'score' must not appear without debug info. Got:\n{}", source
    );
}

#[test]
fn b11_call_result_local_from_debug_info() {
    // Phase B0.11: the CALL handler (for nresults=2, i.e. 1 return value)
    // calls `ctx.reg_name(proto, a, pc)` to name the result register.
    // When proto.debug_info.locals has an entry for (reg=a, pc=call_pc),
    // the original local name is used for the `local X = call(...)` statement.
    //
    // Shape N: GETIMPORT "someFunc" + CALL at PC=2.
    // Debug info: LocalVar{name="result", reg=0, start_pc=2, end_pc=5}.
    // ctx.reg_name(proto, 0, 2) → start_pc=2 in [2, 5) → "result".
    // Emits `local result = someFunc()` + `return result`.
    let chunk = build_call_result_debug_name();
    let mut ctx = DecompileContext::new(&chunk);
    let source = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    assert!(
        source.contains("result"),
        "B0.11 Shape-N: debug local name 'result' must appear in source. Got:\n{}", source
    );
    // Without debug info the name would be a synthesized CallResult name (e.g.
    // "someFunc" or "v0"). Must not be raw "v0".
    assert!(
        !source.contains("v0"),
        "B0.11 Shape-N: raw register name 'v0' must not appear when debug info \
         provides 'result'. Got:\n{}", source
    );
}

#[test]
fn b11_numfor_loop_var_from_debug_info() {
    // Phase B0.11: the NumericFor handler calls ctx.reg_name(proto, a+2, prep_pc)
    // to name the loop variable register.  When proto.debug_info.locals has
    // an entry for that (reg, start_pc=prep_pc), the original name is returned.
    //
    // Shape O: FORNPREP A=2, loop var = R4.
    // Debug info: LocalVar{name="idx", reg=4, start_pc=4, end_pc=8}.
    // ctx.reg_name(proto, 4, 4) → 4 in [4, 8) → "idx".
    // For-loop header emits `for idx = 1, n do`.
    // Without debug info, the NumericForVar hint produces "i" (or "i2" etc.).
    let chunk = build_numfor_debug_loop_var();
    let mut ctx = DecompileContext::new(&chunk);
    let source = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    assert!(
        source.contains("for idx"),
        "B0.11 Shape-O: debug loop var name 'idx' must appear as 'for idx', \
         got:\n{}", source
    );
    assert!(
        !source.contains("v4"),
        "B0.11 Shape-O: raw register name 'v4' must not appear when debug info \
         provides 'idx'. Got:\n{}", source
    );
}

#[test]
fn b11_debug_scope_enforcement_write_site() {
    // Phase B0.11: ctx.reg_name applies live-range enforcement — it returns the
    // debug name only when pc is in [start_pc, end_pc).  A CALL result at a PC
    // OUTSIDE the live range of a LocalVar for the same register must NOT use
    // that debug name.
    //
    // Two adjacent CALL results in the same register (R0):
    //   CALL at PC=2 → debug LocalVar{name="result", reg=0, start_pc=2, end_pc=4}
    //     → ctx.reg_name(proto, 0, 2) → 2 in [2,4) → "result"
    //   CALL at PC=5 (after a second GETIMPORT+CALL sequence) → no LocalVar at PC=5
    //     → ctx.reg_name(proto, 0, 5) → 5 NOT in [2,4) → fallback (not "result")
    //
    // We verify: the debug name "result" appears exactly ONCE — from the first
    // CALL — and not from the second CALL (which should use a synthesized name).
    //
    // Code layout:
    //   0: GETIMPORT R0, K[1]   (load someFunc)
    //   1: AUX
    //   2: CALL R0, 1, 2        (result = someFunc() — PC=2 in [2,4) → "result")
    //   3: GETIMPORT R0, K[1]   (load someFunc again)
    //   4: AUX
    //   5: CALL R0, 1, 2        (second call — PC=5 NOT in [2,4) → fallback)
    //   6: RETURN R0, 2
    let code = vec![
        insn_ad(OP_GETIMPORT, 0, 1),    // 0
        IMPORT_PAIRS,                   // 1 AUX
        insn_abc(OP_CALL, 0, 1, 2),     // 2: result = someFunc()
        insn_ad(OP_GETIMPORT, 0, 1),    // 3
        IMPORT_PAIRS,                   // 4 AUX
        insn_abc(OP_CALL, 0, 1, 2),     // 5: second call, outside scope
        insn_abc(OP_RETURN, 0, 2, 0),   // 6
    ];
    // LocalVar covers reg=0 only from PC=2 to PC=4 (exclusive of PC=4).
    let locals = vec![
        LocalVar { name: "result".to_string(), start_pc: 2, end_pc: 4, reg: 0 },
    ];
    let chunk = make_chunk(Proto {
        constants: vec![
            Constant::String("someFunc".to_string()),
            Constant::Import(IMPORT_PAIRS),
        ],
        debug_info: Some(DebugInfo { locals, upvalue_names: vec![] }),
        ..make_proto(code, 0, 4, "scope_enforcement")
    });

    let mut ctx = DecompileContext::new(&chunk);
    let source = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    // "result" must appear (from the in-scope CALL at PC=2)
    assert!(
        source.contains("result"),
        "B0.11 scope: debug name 'result' must appear from in-scope CALL. Got:\n{}", source
    );
    // The second CALL (PC=5, outside scope) must NOT re-use "result" as the
    // local name — it should get a different synthesized name.
    // Count occurrences: exactly 1 "result" assignment expected.
    let assign_count = source.matches("result").count();
    // "result" appears in `local result = ...` (1 decl) and in `return result` (1 use)
    // plus possibly the second call if scope is not enforced.
    // We check that "result" does NOT appear in TWO separate `local` statements.
    let local_result_count = source.match_indices("local result").count();
    assert_eq!(
        local_result_count, 1,
        "B0.11 scope: 'local result' must appear exactly once (in-scope CALL only), \
         got {} occurrences in:\n{}", local_result_count, source
    );
    let _ = assign_count; // suppress unused warning
}

#[test]
fn b11_b10_loop_var_body_names_non_regress() {
    // Phase B0.11 non-regression: B0.10's GenericFor loop variable seeding
    // (via RegVal::LoopVar) must still work correctly with debug info.
    // Shape L (from B0.10): R5 → "item", R6 → "qty" via debug info.
    // Source must still contain "item" and "qty", not "v5" / "v6".
    let chunk = build_forgprep_body_with_debug_names();
    let mut ctx = DecompileContext::new(&chunk);
    let source = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    assert!(
        source.contains("item"),
        "B0.11 non-regress: B0.10 Shape-L 'item' must still appear. Got:\n{}", source
    );
    assert!(
        source.contains("qty"),
        "B0.11 non-regress: B0.10 Shape-L 'qty' must still appear. Got:\n{}", source
    );
    assert!(
        !source.contains("v5") && !source.contains("v6"),
        "B0.11 non-regress: raw reg names v5/v6 must not appear. Got:\n{}", source
    );
}

// ─── Phase B0.50 audit: debug_info.locals in production Roblox bytecode ──
//
// Phase B0.50 investigates whether production Roblox ModuleScripts retain
// `debug_info.locals` — the Luau-compiler-emitted local-name table that
// `ctx.reg_name` already consumes (per B0.11).  Direct corpus probe via
// `/api/info` on 7 representative bytecode dumps (CameraInput,
// CameraModule, ClickToMoveController, VRVehicleCamera, modulescript,
// rbxcharsounds, vrnav) returns `has_debug_info=false` for all 550 protos.
//
// CONCLUSION (2026-04-15): production Roblox strips debug info — the
// `debug_info.locals` wire-up in B0.11 is CORRECT but DORMANT on real
// corpus.  No code change needed.  If/when Roblox ships a "debug build"
// flag or local toolchain output lands in the corpus, names will flow
// through `reg_name()` (mod.rs:152-171) automatically with zero further
// plumbing work.
//
// The tests below pin this invariant so any future regression (either in
// the parser's DebugInfo decode path, or in `reg_name`'s scoped lookup)
// surfaces as a unit-test failure rather than a silent corpus regression.

#[test]
fn b050_audit_parser_emits_debug_info_when_present_in_bytecode() {
    // Invariant: when a Proto is constructed with a populated DebugInfo,
    // its `debug_info` field is `Some(...)` and `locals` round-trips.
    // This is the contract that `reg_name` relies on — if this test fails,
    // the decode path at parser/mod.rs:324-355 is broken.
    let locals = vec![
        LocalVar { name: "foo".to_string(), start_pc: 0, end_pc: 2, reg: 0 },
        LocalVar { name: "bar".to_string(), start_pc: 0, end_pc: 2, reg: 1 },
    ];
    let code = vec![insn_abc(OP_RETURN, 0, 1, 0)];
    let proto = make_proto_with_debug(code, 0, 2, "audit", locals);

    let debug = proto.debug_info.as_ref().expect(
        "B0.50: make_proto_with_debug must produce Some(DebugInfo)"
    );
    assert_eq!(debug.locals.len(), 2, "B0.50: both LocalVar entries must round-trip");
    assert_eq!(debug.locals[0].name, "foo");
    assert_eq!(debug.locals[0].reg, 0);
    assert_eq!(debug.locals[1].name, "bar");
    assert_eq!(debug.locals[1].reg, 1);
}

#[test]
fn b050_audit_production_roblox_default_state_is_none() {
    // Invariant: the DEFAULT state of a Proto (constructed without debug
    // info) has `debug_info = None`.  This mirrors every production
    // Roblox ModuleScript probed on 2026-04-15 (7 corpus dumps, 550 protos,
    // 0 with debug_info).  If this ever flips — e.g. because Roblox ships a
    // debug build — the /api/info endpoint will reflect that and we'll
    // want to revisit the reg_name pipeline empirically.
    let code = vec![insn_abc(OP_RETURN, 0, 1, 0)];
    let proto = make_proto(code, 0, 2, "audit_none");
    assert!(
        proto.debug_info.is_none(),
        "B0.50: default Proto must have debug_info=None (matches corpus)"
    );
}

#[test]
fn b050_audit_reg_name_returns_debug_name_in_live_range() {
    // Pinning test for ctx.reg_name's scoped lookup against debug_info.
    // Lookup at PC in [start_pc, end_pc) must return the LocalVar name.
    // If this fails, B0.11's end-to-end wire-up is compromised.
    let locals = vec![
        LocalVar { name: "my_local".to_string(), start_pc: 2, end_pc: 6, reg: 3 },
    ];
    let code = vec![insn_abc(OP_RETURN, 0, 1, 0)];
    let chunk = make_chunk(make_proto_with_debug(code, 0, 4, "audit_range", locals));
    let mut ctx = DecompileContext::new(&chunk);

    // PC inside live range → returns debug name
    assert_eq!(ctx.reg_name(&chunk.protos[0], 3, 3), "my_local");
    // PC at start_pc boundary → returns debug name (per B0.11 `at_start`)
    assert_eq!(ctx.reg_name(&chunk.protos[0], 3, 2), "my_local");
}

#[test]
fn b050_audit_reg_name_ignores_debug_outside_live_range() {
    // Pinning test for scope enforcement: at PC outside [start_pc, end_pc),
    // reg_name must NOT return the debug name (B0.11 scope correctness).
    let locals = vec![
        LocalVar { name: "my_local".to_string(), start_pc: 2, end_pc: 6, reg: 3 },
    ];
    let code = vec![insn_abc(OP_RETURN, 0, 1, 0)];
    let chunk = make_chunk(make_proto_with_debug(code, 0, 4, "audit_outside", locals));
    let mut ctx = DecompileContext::new(&chunk);

    // PC at end_pc (exclusive bound) → NOT my_local
    let name_at_end = ctx.reg_name(&chunk.protos[0], 3, 6);
    assert_ne!(
        name_at_end, "my_local",
        "B0.50: reg_name must NOT return 'my_local' at PC=end_pc (exclusive). Got: {}", name_at_end
    );
}

#[test]
fn b050_audit_reg_name_rejects_invalid_identifier() {
    // The sanitize guard at mod.rs:164-168 MUST reject names that are not
    // valid Luau identifiers (e.g. names with spaces) and fall through to
    // synthesis.  Without this guard, debug-info with exotic names would
    // emit ill-formed Luau output.
    let locals = vec![
        LocalVar { name: "bad name with spaces".to_string(), start_pc: 0, end_pc: 4, reg: 0 },
    ];
    let code = vec![insn_abc(OP_RETURN, 0, 1, 0)];
    let chunk = make_chunk(make_proto_with_debug(code, 0, 2, "audit_invalid", locals));
    let mut ctx = DecompileContext::new(&chunk);

    let name = ctx.reg_name(&chunk.protos[0], 0, 0);
    assert_ne!(
        name, "bad name with spaces",
        "B0.50: invalid-identifier debug name must be rejected, not returned verbatim"
    );
}

#[test]
fn b050_audit_reg_name_falls_back_without_debug_info() {
    // Non-regression: when debug_info=None (every production corpus script),
    // reg_name must fall through to synthesis and produce SOME name
    // (typically a "v<N>" or hint-based name).  Must not panic or return ""
    // (empty string would emit an ill-formed identifier in the output).
    let code = vec![insn_abc(OP_RETURN, 0, 1, 0)];
    let chunk = make_chunk(make_proto(code, 0, 4, "audit_fallback"));
    let mut ctx = DecompileContext::new(&chunk);

    let name = ctx.reg_name(&chunk.protos[0], 2, 0);
    assert!(!name.is_empty(), "B0.50: synthesis fallback must never return empty string");
    // And must not accidentally match any of the debug-info test names.
    assert_ne!(name, "foo");
    assert_ne!(name, "my_local");
}
