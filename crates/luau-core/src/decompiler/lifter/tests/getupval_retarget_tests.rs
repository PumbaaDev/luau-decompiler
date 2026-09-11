//! GETUPVAL must retarget a register even when that register is a declared,
//! non-empty local.
//!
//! Found via the compile gate (7/628 failures). In
//! `541_..._RootCamera.lua`, proto 16 does:
//!
//! ```text
//! 12: LOADB    R0 false +1      ; R0 = (X < Y)   [bool idiom]
//! 13: LOADB    R0 true
//! 14: SETUPVAL R0 U2            ; isPortrait = X < Y
//! 15: GETUPVAL R0 U3            ; R0 = callback   ← was DROPPED
//! 16: GETUPVAL R1 U2            ; R1 = isPortrait ← was DROPPED
//! 17: CALL     R0 args=1        ; callback(isPortrait)
//! ```
//!
//! The B0.52 guard skipped the store whenever the destination register was
//! declared, non-empty, and not currently an upvalue alias — leaving stale
//! expressions in R0/R1. The CALL then lifted as `(X < Y)(X)`, a statement
//! beginning with `(`, which the Luau compiler rejects as ambiguous syntax.
//! Same signature in 5 more corpus files (bases like `(new74 + new74)`,
//! `("Sticker")`, `(0)` on calls/namecalls).
//!
//! The guard existed to protect a B0.51-seeded module-table local from a
//! MISDECODED GetUpval. That protection made the common, correctly-decoded
//! case silently wrong: an invisible stale callee instead of a visible
//! upvalue name. Registers are function-scoped and reuse is routine; a real
//! GETUPVAL into a previously-declared scratch register is normal bytecode.

use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, Constant, Proto};

const OP_LOADB: u8      = 3;
const OP_GETUPVAL: u8   = 9;
const OP_SETUPVAL: u8   = 10;
const OP_GETTABLEKS: u8 = 15;
const OP_CALL: u8       = 21;
const OP_RETURN: u8     = 22;
const OP_JUMPIFLE: u8   = 28;

fn insn_ad(op: u8, a: u8, d: i16) -> u32 {
    let du = d as u16 as u32;
    (op as u32) | ((a as u32) << 8) | (du << 16)
}

fn insn_abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
}

/// Full mirror of RootCamera proto 16:
///
/// ```text
///  0: GETUPVAL   R0 U1
///  1: GETTABLEKS R0 R0.K0 "ViewportSize"  ; local ViewportSize2 = <u1>.ViewportSize
///  3: SETUPVAL   R0 U0                    ; ViewportSize = ViewportSize2
///  4: GETUPVAL   R1 U0
///  5: GETTABLEKS R1 R1.K1 "X"             ; local X = ViewportSize.X
///  7: GETUPVAL   R2 U0
///  8: GETTABLEKS R2 R2.K2 "Y"             ; local Y = ViewportSize.Y
/// 10: JUMPIFLE   R1 <= R2 -> +2           ; bool idiom over the LOADB pair
/// 12: LOADB      R0 false +1
/// 13: LOADB      R0 true                  ; R0 = comparison, stays INLINE
/// 14: SETUPVAL   R0 U2
/// 15: GETUPVAL   R0 U3                    ; retarget declared+non-empty R0
/// 16: GETUPVAL   R1 U2                    ; retarget declared+non-empty R1
/// 17: CALL       R0 args=1 results=0
/// 18: RETURN
/// ```
///
/// The comparison reads R1/R2 while landing in R0, so `store_complex` keeps
/// it inline (no self-mutation). If pc 15 is then dropped, the CALL lifts the
/// stale parenthesized comparison as its callee — the compile-gate shape.
fn make_repro_chunk() -> Chunk {
    let code = vec![
        insn_abc(OP_GETUPVAL, 0, 1, 0),
        insn_abc(OP_GETTABLEKS, 0, 0, 0),
        0u32, // AUX: K0 "ViewportSize"
        insn_abc(OP_SETUPVAL, 0, 0, 0),
        insn_abc(OP_GETUPVAL, 1, 0, 0),
        insn_abc(OP_GETTABLEKS, 1, 1, 0),
        1u32, // AUX: K1 "X"
        insn_abc(OP_GETUPVAL, 2, 0, 0),
        insn_abc(OP_GETTABLEKS, 2, 2, 0),
        2u32, // AUX: K2 "Y"
        insn_ad(OP_JUMPIFLE, 1, 2),
        2u32, // AUX: rhs register R2
        insn_abc(OP_LOADB, 0, 0, 1),
        insn_abc(OP_LOADB, 0, 1, 0),
        insn_abc(OP_SETUPVAL, 0, 2, 0),
        insn_abc(OP_GETUPVAL, 0, 3, 0),
        insn_abc(OP_GETUPVAL, 1, 2, 0),
        insn_abc(OP_CALL, 0, 2, 1),
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    let constants = vec![
        Constant::String("ViewportSize".to_string()),
        Constant::String("X".to_string()),
        Constant::String("Y".to_string()),
    ];
    let proto = Proto {
        max_stack_size: 8,
        num_params: 0,
        num_upvalues: 4,
        is_vararg: false,
        flags: 0,
        typeinfo: None,
        code,
        constants,
        child_protos: Vec::new(),
        line_defined: 1,
        debug_name: Some("getupval_retarget".to_string()),
        line_info: None,
        debug_info: None,
    };
    Chunk {
        version: 6,
        types_version: 0,
        strings: vec![
            "ViewportSize".to_string(),
            "X".to_string(),
            "Y".to_string(),
        ],
        protos: vec![proto],
        main_proto: 0,
    }
}

/// No emitted statement may begin with `(` — that is precisely the shape the
/// Luau compiler rejects ("ambiguous syntax") and it can only arise here if a
/// GETUPVAL was dropped and the CALL read a stale parenthesized expression.
#[test]
fn getupval_into_declared_nonempty_register_retargets_it() {
    let chunk = make_repro_chunk();
    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    for line in out.lines() {
        let t = line.trim_start();
        assert!(
            !t.starts_with('('),
            "statement begins with '(': the GETUPVAL at pc 15 was dropped and \
             the stale inline comparison leaked into callee position.\nline: {}\nfull output:\n{}",
            line,
            out
        );
    }

    // The argument register R1 must be retargeted too (pc 16). If it is not,
    // the call's argument is the stale local `X` from pc 5.
    assert!(
        !out.contains("(X)"),
        "call argument is the stale local `X`: the GETUPVAL at pc 16 was \
         dropped.\nfull output:\n{}",
        out
    );

    // Sanity: the CALL must still be present as a statement.
    assert!(
        out.lines().any(|l| {
            let t = l.trim();
            !t.starts_with("local ") && t.contains('(') && t.ends_with(')')
        }),
        "expected a call statement lifted from CALL R0;\nfull output:\n{}",
        out
    );
}
