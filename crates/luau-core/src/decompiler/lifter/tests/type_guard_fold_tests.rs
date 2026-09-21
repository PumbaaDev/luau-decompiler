//! Short-circuit type-guard fold.
//!
//! `if x and x:IsA(T) then keep else make end` compiles to two `JUMPIFNOT`s
//! that BOTH skip to the else: the first tests `x`, the second tests the
//! `x:IsA(T)` call result. Lifted one jump at a time the outer jump owns the
//! `if x` and the inner jump — whose target is the else, one past the arm's end
//! — was dropped, parking the call as a dead `local t = x:IsA(T)` and running
//! the arm body unconditionally. The type check was silently lost (measured on
//! a corpus module, and the corpus-wide `dropped_arm_value` shape).
//!
//! The fold folds each such leading guard back into the condition as an `and`
//! term. These tests hand-assemble the exact bytecode shape and assert the
//! guard survives as part of the condition rather than as a dead local.

use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, Constant, Proto};

const OP_LOADNIL: u8 = 2;
const OP_LOADK: u8 = 5;
const OP_MOVE: u8 = 6;
const OP_NAMECALL: u8 = 20;
const OP_CALL: u8 = 21;
const OP_RETURN: u8 = 22;
const OP_JUMP: u8 = 23;
const OP_JUMPIFNOT: u8 = 26;

fn insn_ad(op: u8, a: u8, d: i16) -> u32 {
    let du = d as u16 as u32;
    (op as u32) | ((a as u32) << 8) | (du << 16)
}

fn insn_abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
}

fn chunk_of(code: Vec<u32>, constants: Vec<Constant>, num_params: u8) -> Chunk {
    let proto = Proto {
        max_stack_size: 16,
        num_params,
        num_upvalues: 0,
        is_vararg: false,
        flags: 0,
        typeinfo: None,
        code,
        constants,
        child_protos: Vec::new(),
        line_defined: 1,
        debug_name: Some("main".to_string()),
        line_info: None,
        debug_info: None,
    };
    Chunk {
        version: 6,
        types_version: 0,
        strings: Vec::new(),
        protos: vec![proto],
        main_proto: 0,
    }
}

/// The guard-if must sit inside a larger span that is lifted inline (a loop
/// body, or, as here, an outer arm) — that is the only place the fold runs and
/// the only place the drop happens; a bare top-level if/else is structured as
/// its own region and handled elsewhere. So the shape is:
///
/// ```lua
/// if cond then                       -- outer arm, lifted as one inline span
///     if x and x:IsA("Model") then   -- the guard the fold reconstructs
///         r = x
///     else
///         x()                        -- a side effect keeps the `if` alive
///         r = nil
///     end
///     use(r)
/// end
/// ```
///
/// ```text
/// w0  JUMPIFNOT R0 -> w13  ; outer: if cond (R0)
/// w1  JUMPIFNOT R1 -> w9   ; if x (R1)                  (else at w9)
/// w2  LOADK     R4 K1      ; arg "Model"
/// w3  NAMECALL  R2 R1      ; R3 = x (self), R2 = x.IsA
/// w4  <aux = K0 "IsA">
/// w5  CALL      R2 (2 args, 1 result) ; R2 = x:IsA("Model")
/// w6  JUMPIFNOT R2 -> w9   ; if not the type check      (else at w9)
/// w7  MOVE      R5 R1      ; then: r = x
/// w8  JUMP      -> w12     ; skip else
/// w9  MOVE      R6 R1      ; else: prep the side-effect call
/// w10 CALL      R6 (0 args, 0 results) ; SIDE EFFECT so the `if` survives
/// w11 LOADNIL   R5         ; else: r = nil
/// w12 MOVE      R7 R5      ; use(r)
/// w13 RETURN
/// ```
fn build_isa_guard_chunk() -> Chunk {
    let code = vec![
        insn_ad(OP_JUMPIFNOT, 0, 12), // w0 outer -> w13
        insn_ad(OP_JUMPIFNOT, 1, 7),  // w1 -> w9
        insn_ad(OP_LOADK, 4, 1),      // w2  R4 = K1 "Model"
        insn_abc(OP_NAMECALL, 2, 1, 0), // w3
        0u32,                          // w4  aux = K0 "IsA"
        insn_abc(OP_CALL, 2, 3, 2),   // w5  R2 = x:IsA("Model")
        insn_ad(OP_JUMPIFNOT, 2, 2),  // w6 -> w9
        insn_abc(OP_MOVE, 5, 1, 0),   // w7  R5 = R1
        insn_ad(OP_JUMP, 0, 3),       // w8 -> w12
        insn_abc(OP_MOVE, 6, 1, 0),   // w9  R6 = R1
        insn_abc(OP_CALL, 6, 1, 1),   // w10 R6()  (side effect)
        insn_abc(OP_LOADNIL, 5, 0, 0), // w11 R5 = nil
        insn_abc(OP_MOVE, 7, 5, 0),   // w12 R7 = R5  (use r)
        insn_abc(OP_RETURN, 0, 1, 0), // w13 return
    ];
    chunk_of(
        code,
        vec![
            Constant::String("IsA".to_string()),
            Constant::String("Model".to_string()),
        ],
        2,
    )
}

#[test]
fn isa_guard_folds_into_the_condition() {
    let chunk = build_isa_guard_chunk();
    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);
    eprintln!("=== emitted source ===\n{}\n======================", out);

    // The type check must appear as an `and` term in a condition, not as a
    // dead local. Some `if <x> and <x>:IsA("Model") then` must exist.
    let folded = out.lines().any(|l| {
        let t = l.trim_start();
        t.starts_with("if ") && t.contains(" and ") && t.contains(":IsA(\"Model\")")
    });
    assert!(folded, "type guard was not folded into the condition:\n{out}");

    // And the guard must NOT survive as a parked dead local of the call value,
    // which is the exact dropped-guard defect: `local <name> = <x>:IsA("Model")`
    // with the value never read.
    let dead_local = out.lines().any(|l| {
        let t = l.trim_start();
        t.starts_with("local ") && t.contains("= ") && t.trim_end().ends_with(":IsA(\"Model\")")
    });
    assert!(
        !dead_local,
        "the :IsA guard was parked as a dead local instead of folded:\n{out}"
    );
}
