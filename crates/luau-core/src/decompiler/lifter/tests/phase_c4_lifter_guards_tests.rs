//! Phase C4 — lifter corruption guards.
//!
//! When opmap detection is wrong the lifter can produce AST shapes that
//! are syntactically valid Luau but semantically nonsensical — e.g.
//! `for k, v in math.huge do`, `#true`, `x = x + x` where `x` is a
//! function literal. Full moon parses these as-is, but downstream
//! analysis (and human review) flags them as garbage.
//!
//! Phase C4 adds guards at three emission sites:
//!   1. GenericFor construction — rejects non-iterable iterator primaries
//!      (`Number`, `Bool`, `Nil`, and known `math.*` non-callable fields).
//!   2. Unary op handlers (Not / Minus / Length / RbxExt96) — rejects
//!      operands that are provably wrong (`#<bool>`, `-<function>`, etc).
//!   3. Self-arithmetic A==B==C handlers — rejects when the register
//!      holds a `Function` or `Nil` value.
//!
//! Each guard emits `Stat::Comment(...)` in place of the garbage so that
//! the surrounding source still parses.
//!
//! These tests build minimal single-proto chunks that exercise each
//! guard and assert that the emitted source contains `-- lifter error:`
//! and does NOT contain the corrupt shape.

use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, Constant, Proto};

// ─── Opcode byte constants (mirror canonical Luau Bytecode.h) ───
const OP_LOADN: u8      = 4;
const OP_LOADB: u8      = 3;
const OP_RETURN: u8     = 22;
const OP_LENGTH: u8     = 52;
const OP_ADD: u8        = 33;
const OP_FORGPREP: u8   = 58;
const OP_FORGLOOP: u8   = 59;
const OP_NEWCLOSURE: u8 = 19;

fn insn_ad(op: u8, a: u8, d: i16) -> u32 {
    let du = d as u16 as u32;
    (op as u32) | ((a as u32) << 8) | (du << 16)
}

fn insn_abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
}

fn make_proto(code: Vec<u32>, constants: Vec<Constant>, num_params: u8) -> Proto {
    Proto {
        max_stack_size: 32,
        num_params,
        num_upvalues: 0,
        is_vararg: true,
        flags: 0,
        typeinfo: None,
        code,
        constants,
        child_protos: Vec::new(),
        line_defined: 1,
        debug_name: Some("phase_c4_test".to_string()),
        line_info: None,
        debug_info: None,
    }
}

fn make_chunk(proto: Proto) -> Chunk {
    Chunk {
        version: 6,
        types_version: 0,
        strings: vec![],
        protos: vec![proto],
        main_proto: 0,
    }
}

fn decompile(chunk: &Chunk) -> String {
    let mut ctx = DecompileContext::new(chunk);
    decompile_proto(&mut ctx, &chunk.protos[0], 0, 0)
}

// ═══════════════════════════════════════════════════════════════════════
// Test 1: GenericFor with literal Number iterator → Comment, no GenericFor
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn c4_generic_for_number_iterator_emits_comment() {
    // Seed reg 0 with Number(5) via LOADN, then FORGPREP r0 → FORGLOOP.
    // A healthy opmap would never produce this shape; Phase C4 guards it.
    //
    // Layout:
    //   pc 0: LOADN r0 = 5        — literal number in iterator slot
    //   pc 1: LOADN r1 = 0        — state slot
    //   pc 2: LOADN r2 = 0        — control slot
    //   pc 3: FORGPREP D=0        — target loop_pc = 3+0+1 = 4
    //   pc 4: FORGLOOP D=-1       — loop_pc=4, D=-1 → body back at pc 3+1=4
    //                               (minimal shape; the Phase C4 guard
    //                               intercepts at GenericFor emission)
    //   pc 5: AUX (var count = 2)
    //   pc 6: RETURN
    let code = vec![
        insn_ad(OP_LOADN, 0, 5),         // R0 = 5
        insn_ad(OP_LOADN, 1, 0),         // R1 = 0
        insn_ad(OP_LOADN, 2, 0),         // R2 = 0
        insn_ad(OP_FORGPREP, 0, 0),      // prep, D=0 → target pc 4 (FORGLOOP)
        insn_ad(OP_FORGLOOP, 0, -1),     // loop back
        2u32,                            // AUX: var count = 2
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    let chunk = make_chunk(make_proto(code, vec![], 0));
    let src = decompile(&chunk);
    assert!(
        src.contains("lifter error"),
        "expected `-- lifter error:` Comment, got:\n{}",
        src,
    );
    assert!(
        !src.contains("for k, v in") || src.contains("lifter error"),
        "Comment should appear in place of GenericFor, got:\n{}",
        src,
    );
    // The number literal 5 should be called out in the reason.
    assert!(
        src.contains("number literal"),
        "expected reason to mention `number literal`, got:\n{}",
        src,
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Test 2: LENGTH of Expr::Bool emits Comment
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn c4_length_of_bool_emits_comment() {
    // Seed reg 1 with Bool(true), then apply LENGTH r0 = #r1.
    // The current Length handler is passthrough, but Phase C4 inserts a
    // Comment when the operand is Bool / Number / Function (all runtime
    // errors for `#`, and strong signals of opmap corruption).
    let code = vec![
        insn_abc(OP_LOADB, 1, 1, 0),     // R1 = true
        insn_abc(OP_LENGTH, 0, 1, 0),    // R0 = #R1  → guard fires
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    let chunk = make_chunk(make_proto(code, vec![], 0));
    let src = decompile(&chunk);
    assert!(
        src.contains("lifter error"),
        "expected `-- lifter error:` Comment, got:\n{}",
        src,
    );
    assert!(
        src.contains("LENGTH on Bool"),
        "expected reason to mention `LENGTH on Bool`, got:\n{}",
        src,
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Test 3: Self-arithmetic A==B==C where reg A holds Expr::Function
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn c4_self_arith_on_function_emits_comment() {
    // NEWCLOSURE with an out-of-range D takes the placeholder-closure path
    // (opcode_handlers.rs line ~1310) which puts a raw `Expr::Function` literal
    // into the register. The Add A==B==C passthrough (B0.73) then sees a
    // Function in regs[a] and the Phase C4 guard fires a Comment.
    //
    // With no child protos and D=100 (out of bounds), proto lookup fails:
    // child_protos is empty, parent_idx+1+100 is out of bounds, and direct
    // global index 100 is out of bounds too.
    let code = vec![
        insn_ad(OP_NEWCLOSURE, 0, 100), // R0 = Expr::Function{body:[Comment]}
        insn_abc(OP_ADD, 0, 0, 0),      // R0 = R0 + R0 (A==B==C, self-arith)
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    let chunk = make_chunk(make_proto(code, vec![], 0));
    let src = decompile(&chunk);
    assert!(
        src.contains("lifter error"),
        "expected `-- lifter error:` Comment, got:\n{}",
        src,
    );
    assert!(
        src.contains("self-arith ADD on non-numeric Function"),
        "expected reason to mention `self-arith ADD on non-numeric Function`, got:\n{}",
        src,
    );
    // Raw bytes should be included (raw=...).
    assert!(
        src.contains("raw="),
        "expected raw instruction bytes in Comment, got:\n{}",
        src,
    );
}
