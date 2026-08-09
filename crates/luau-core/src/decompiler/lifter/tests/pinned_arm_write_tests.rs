//! A register the premateralizer PINNED across if/else arm spans must have
//! EVERY arm write emitted as an assignment to the pinned binding — including
//! writes from handlers that park into the register file directly (GETUPVAL /
//! LOADNIL / GETGLOBAL) instead of routing through `store_complex`.
//!
//! Found via the round-2 census of the 117 remaining defects (110 of them are
//! this umbrella). Bytecode exhibit `538_ReplicatedStorage_LocalFX_RingPulse`,
//! child proto 0 "OnUpdate":
//!
//! ```text
//!   0-5: LOADNIL R1..R6              ; six locals, nil-seeded
//!     7: JUMPIFNOTLT R7 >= R0 → else
//!     9: GETUPVAL R3 U1              ; then-arm seed writes, ALL via GETUPVAL
//!    ..: SUB/DIV → R0                ; then-arm ALSO emits statements
//!    21: GETUPVAL R3 U2              ; else-arm writes (swapped pairing)
//!    30: SUB R10 R4 - R3             ; post-join reads
//! ```
//!
//! The premateralizer correctly seeded the six locals before the `if` and
//! pinned R1..R6 across both arm spans — traced live: the parent's CAPTURE
//! inference and the child's GETUPVAL name resolution were BOTH correct at
//! lift time. The loss is purely the write path: the GETUPVAL handler parks
//! `regs[a] = Name(upval)` without emitting, the join annihilates the
//! disagreeing parks, and every post-join read mints an unbound `vN`
//! (hoisted by free_var_decls, flagged `declared_never_assigned`).
//!
//! The one existing rescue — the empty-arm phi reconstruction — deliberately
//! bails when either arm emits ANY statement, which is exactly the RingPulse
//! shape (the arms also rewrite the parameter through `store_complex`, which
//! emits). These tests pin the statement-bearing shape; the phi-path and
//! straight-line shapes are locked as behavioral guards.

use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, Constant, Proto};

// Canonical Luau opcode bytes (identity opmap, Chunk.version = 6).
const OP_LOADNIL: u8 = 2;
const OP_MOVE: u8 = 6;
const OP_GETGLOBAL: u8 = 7;
const OP_GETUPVAL: u8 = 9;
const OP_GETIMPORT: u8 = 12;
const OP_NEWCLOSURE: u8 = 19;
const OP_RETURN: u8 = 22;
const OP_JUMP: u8 = 23;
const OP_JUMPIFNOT: u8 = 26;
const OP_DIV: u8 = 36;
const OP_CAPTURE: u8 = 70;

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
    if !ids.is_empty() {
        v |= (ids[0] & 0x3FF) << 20;
    }
    v
}

fn proto(code: Vec<u32>, constants: Vec<Constant>, num_upvalues: u8, name: &str) -> Proto {
    Proto {
        max_stack_size: 16,
        num_params: 1,
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

/// Parent captures two named values into a child that nil-seeds R1, then runs
/// a diamond where EACH arm both (a) rewrites the parameter through
/// `store_complex` — so the arm emits a statement and the empty-arm phi
/// rescue cannot fire — and (b) writes the seed through a direct-park handler
/// (the write under test). R1 is read after the join. This is the RingPulse
/// child in miniature.
///
/// ```text
/// child (params=1: R0; 2 upvalues):
///   0: LOADNIL    R1                 ; the seed
///   1: JUMPIFNOT  R0 → else (pc 6)
///   2: GETUPVAL   R2, U0
///   3: DIV        R0, R0, R2         ; self-mutating param write → statement
///   4: <then_write of R1>
///   5: JUMP       → join (pc 9)
///   6: GETUPVAL   R2, U1
///   7: DIV        R0, R0, R2
///   8: <else_write of R1>
///   9: MOVE       R3, R1             ; post-join read of the seed
///  10: MOVE       R4, R0             ; post-join read of the param
///  11: RETURN     R3, 2
/// ```
fn statement_arm_chunk(
    then_write: u32,
    else_write: u32,
    extra_child_consts: Vec<Constant>,
) -> Chunk {
    let import_a = pack_import(&[2]);
    let import_b = pack_import(&[4]);
    let parent_code = vec![
        insn_ad(OP_GETIMPORT, 1, 0),
        import_a, // AUX
        insn_ad(OP_GETIMPORT, 2, 3),
        import_b, // AUX
        insn_ad(OP_NEWCLOSURE, 3, 0),
        insn_abc(OP_CAPTURE, 0, 1, 0),
        insn_abc(OP_CAPTURE, 0, 2, 0),
        insn_abc(OP_RETURN, 3, 2, 0),
    ];
    let parent_constants = vec![
        Constant::Import(import_a),
        Constant::String("unused0".to_string()),
        Constant::String("Config".to_string()),
        Constant::Import(import_b),
        Constant::String("Backup".to_string()),
    ];
    let child_code = vec![
        insn_abc(OP_LOADNIL, 1, 0, 0),
        insn_ad(OP_JUMPIFNOT, 0, 4),
        insn_abc(OP_GETUPVAL, 2, 0, 0),
        insn_abc(OP_DIV, 0, 0, 2),
        then_write,
        insn_ad(OP_JUMP, 0, 3),
        insn_abc(OP_GETUPVAL, 2, 1, 0),
        insn_abc(OP_DIV, 0, 0, 2),
        else_write,
        insn_abc(OP_MOVE, 3, 1, 0),
        insn_abc(OP_MOVE, 4, 0, 0),
        insn_abc(OP_RETURN, 3, 2, 0),
    ];
    let mut parent = proto(parent_code, parent_constants, 0, "diamond_parent");
    parent.child_protos = vec![1];
    let child = proto(child_code, extra_child_consts, 2, "chooser");
    Chunk {
        version: 6,
        types_version: 0,
        strings: vec![
            "unused0".to_string(),
            "unused1".to_string(),
            "Config".to_string(),
            "SomeGlobal".to_string(),
            "Backup".to_string(),
        ],
        protos: vec![parent, child],
        main_proto: 0,
    }
}

/// Every assignment line `<name> = <rhs>` in the output, trimmed.
fn assign_lines(out: &str) -> Vec<&str> {
    out.lines().map(str::trim_start).collect()
}

/// Find the single name N for which BOTH `N = <then_rhs>` and
/// `N = <else_rhs>` appear — the pinned binding both arm writes landed on.
fn binding_carrying_both(out: &str, then_rhs: &str, else_rhs: &str) -> Option<String> {
    let lines = assign_lines(out);
    for l in &lines {
        let Some((lhs, rhs)) = l.split_once(" = ") else { continue };
        if rhs.trim() != then_rhs || lhs.contains(' ') || lhs.starts_with("local") {
            continue;
        }
        let other = format!("{} = {}", lhs, else_rhs);
        if lines.iter().any(|l2| l2.trim() == other) {
            return Some(lhs.to_string());
        }
    }
    None
}

/// The load-bearing shape: statement-bearing arms whose seed writes go
/// through GETUPVAL. Both writes must survive as assignments to ONE binding.
#[test]
fn getupval_write_in_statement_arm_is_emitted() {
    let chunk = statement_arm_chunk(
        insn_abc(OP_GETUPVAL, 1, 0, 0),
        insn_abc(OP_GETUPVAL, 1, 1, 0),
        Vec::new(),
    );
    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    let bound = binding_carrying_both(&out, "Config", "Backup");
    assert!(
        bound.is_some(),
        "both arm writes must be emitted as assignments to the one pinned \
         binding — a parked GETUPVAL is annihilated at the join and the \
         post-join read goes permanently nil:\n{out}"
    );
}

/// Same shape, else arm clears the seed with LOADNIL: the nil write is as
/// much a write as any other.
#[test]
fn loadnil_write_in_statement_arm_is_emitted() {
    let chunk = statement_arm_chunk(
        insn_abc(OP_GETUPVAL, 1, 0, 0),
        insn_abc(OP_LOADNIL, 1, 0, 0),
        Vec::new(),
    );
    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    let bound = binding_carrying_both(&out, "Config", "nil");
    assert!(
        bound.is_some(),
        "the GETUPVAL then-write and the LOADNIL else-write must both land \
         on the pinned binding:\n{out}"
    );
}

/// Same shape, else arm reads a global. GETGLOBAL parks `Name` directly and
/// must honor the pin like everything else.
#[test]
fn getglobal_write_in_statement_arm_is_emitted() {
    let chunk = statement_arm_chunk(
        insn_abc(OP_GETUPVAL, 1, 0, 0),
        insn_ad(OP_GETGLOBAL, 1, 0),
        vec![Constant::String("SomeGlobal".to_string())],
    );
    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    let bound = binding_carrying_both(&out, "Config", "SomeGlobal");
    assert!(
        bound.is_some(),
        "the GETGLOBAL else-write must land on the pinned binding:\n{out}"
    );
}

/// Behavioral guard: statement-FREE arms stay on the phi-reconstruction path
/// (`local x; if c then x = a else x = b end` — or a ternary), which already
/// preserves both values. The pin fix must not regress it.
#[test]
fn statement_free_arms_still_reconstruct_both_values() {
    let import_a = pack_import(&[2]);
    let import_b = pack_import(&[4]);
    let parent_code = vec![
        insn_ad(OP_GETIMPORT, 1, 0),
        import_a, // AUX
        insn_ad(OP_GETIMPORT, 2, 3),
        import_b, // AUX
        insn_ad(OP_NEWCLOSURE, 3, 0),
        insn_abc(OP_CAPTURE, 0, 1, 0),
        insn_abc(OP_CAPTURE, 0, 2, 0),
        insn_abc(OP_RETURN, 3, 2, 0),
    ];
    let parent_constants = vec![
        Constant::Import(import_a),
        Constant::String("unused0".to_string()),
        Constant::String("Config".to_string()),
        Constant::Import(import_b),
        Constant::String("Backup".to_string()),
    ];
    let child_code = vec![
        insn_abc(OP_LOADNIL, 1, 0, 0),
        insn_ad(OP_JUMPIFNOT, 0, 2),
        insn_abc(OP_GETUPVAL, 1, 0, 0),
        insn_ad(OP_JUMP, 0, 1),
        insn_abc(OP_GETUPVAL, 1, 1, 0),
        insn_abc(OP_MOVE, 2, 1, 0),
        insn_abc(OP_RETURN, 2, 2, 0),
    ];
    let mut parent = proto(parent_code, parent_constants, 0, "diamond_parent");
    parent.child_protos = vec![1];
    let child = proto(child_code, Vec::new(), 2, "chooser");
    let chunk = Chunk {
        version: 6,
        types_version: 0,
        strings: vec![
            "unused0".to_string(),
            "unused1".to_string(),
            "Config".to_string(),
            "SomeGlobal".to_string(),
            "Backup".to_string(),
        ],
        protos: vec![parent, child],
        main_proto: 0,
    };
    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    assert!(
        out.contains("Config") && out.contains("Backup"),
        "both diamond values must survive to the output:\n{out}"
    );
}

/// Outside a pinned span the GETUPVAL alias park is the CORRECT behavior:
/// a straight-line `GETUPVAL; MOVE; RETURN` must keep inlining the upvalue
/// name, not sprout assignments.
#[test]
fn straightline_getupval_still_inlines() {
    let import_a = pack_import(&[2]);
    let parent_code = vec![
        insn_ad(OP_GETIMPORT, 1, 0),
        import_a, // AUX
        insn_ad(OP_NEWCLOSURE, 2, 0),
        insn_abc(OP_CAPTURE, 0, 1, 0),
        insn_abc(OP_RETURN, 2, 2, 0),
    ];
    let parent_constants = vec![
        Constant::Import(import_a),
        Constant::String("unused0".to_string()),
        Constant::String("Config".to_string()),
    ];
    let child_code = vec![
        insn_abc(OP_GETUPVAL, 1, 0, 0),
        insn_abc(OP_MOVE, 2, 1, 0),
        insn_abc(OP_RETURN, 2, 2, 0),
    ];
    let mut parent = proto(parent_code, parent_constants, 0, "straight_parent");
    parent.child_protos = vec![1];
    let child = proto(child_code, Vec::new(), 1, "straight_child");
    let chunk = Chunk {
        version: 6,
        types_version: 0,
        strings: vec![
            "unused0".to_string(),
            "unused1".to_string(),
            "Config".to_string(),
        ],
        protos: vec![parent, child],
        main_proto: 0,
    };

    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    assert!(
        out.contains("return Config"),
        "an unpinned GETUPVAL read stays an inlined upvalue reference:\n{out}"
    );
    assert!(
        !out.lines().any(|l| l.trim_start().ends_with("= Config")),
        "no assignment statements may appear for an unpinned GETUPVAL:\n{out}"
    );
}
