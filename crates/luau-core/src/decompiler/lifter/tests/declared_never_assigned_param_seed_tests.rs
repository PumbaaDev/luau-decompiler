//! `declared_never_assigned`, parameter-slot diamond (AdoptMe
//! PlaceableFurnitureUseHelper `get_override_choices`).
//!
//! The compiler reuses a PARAMETER register for the value both arms of a
//! diamond produce (`if f then arg3 = f(.., arg3) else arg3 = g() end`). The
//! register is a declared parameter, so neither seed gate accepted it: the
//! then arm wrote a fresh local, the else arm wrote another name, and the
//! merge read minted an unbound `vN` (hoisted to the top of the file as
//! `local vN` and read as nil forever).

use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, Proto};

const OP_MOVE: u8 = 6;
const OP_CALL: u8 = 21;
const OP_RETURN: u8 = 22;
const OP_JUMP: u8 = 23;
const OP_JUMPIFNOT: u8 = 26;

fn insn_ad(op: u8, a: u8, d: i16) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((d as u16 as u32) << 16)
}

fn insn_abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
}

fn chunk_of(code: Vec<u32>, num_params: u8, max_stack: u8) -> Chunk {
    let proto = Proto {
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
        debug_name: Some("main".to_string()),
        line_info: None,
        debug_info: None,
    };
    Chunk { version: 6, types_version: 0, strings: Vec::new(), protos: vec![proto], main_proto: 0 }
}

/// `function(a, b, c) if a then c = a(c) else c = b() end; a(c) end`
///
/// ```text
/// pc0:  JUMPIFNOT R0, +5      -- -> pc6 (else)
/// pc1:  MOVE R3, R0
/// pc2:  MOVE R4, R2           -- the arm READS the parameter first
/// pc3:  CALL R3, 2, 2
/// pc4:  MOVE R2, R3           -- c = a(c)
/// pc5:  JUMP +3               -- -> pc9 (merge)
/// pc6:  MOVE R3, R1
/// pc7:  CALL R3, 1, 2
/// pc8:  MOVE R2, R3           -- c = b()
/// pc9:  MOVE R3, R0           -- merge reads c
/// pc10: MOVE R4, R2
/// pc11: CALL R3, 2, 1
/// pc12: RETURN R0, 1
/// ```
fn build_param_diamond() -> Chunk {
    let code = vec![
        insn_ad(OP_JUMPIFNOT, 0, 5),
        insn_abc(OP_MOVE, 3, 0, 0),
        insn_abc(OP_MOVE, 4, 2, 0),
        insn_abc(OP_CALL, 3, 2, 2),
        insn_abc(OP_MOVE, 2, 3, 0),
        insn_ad(OP_JUMP, 0, 3),
        insn_abc(OP_MOVE, 3, 1, 0),
        insn_abc(OP_CALL, 3, 1, 2),
        insn_abc(OP_MOVE, 2, 3, 0),
        insn_abc(OP_MOVE, 3, 0, 0),
        insn_abc(OP_MOVE, 4, 2, 0),
        insn_abc(OP_CALL, 3, 2, 1),
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    chunk_of(code, 3, 8)
}

#[test]
fn parameter_slot_rebound_in_both_arms_is_bound_at_the_merge() {
    let chunk = build_param_diamond();
    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);
    eprintln!("=== emitted source ===\n{}\n======================", out);

    // Both arm values reach the join through ONE name: the merge call's
    // argument is assigned in the then arm and in the else arm.
    let arg = {
        let start = out.rfind("arg1(").expect("merge call missing") + "arg1(".len();
        let rest = &out[start..];
        rest[..rest.find(')').expect("unterminated call")].trim().to_string()
    };
    assert!(
        !arg.is_empty() && arg != "arg3",
        "merge reads the stale parameter instead of the diamond value:\n{out}"
    );
    let assigns = out
        .lines()
        .map(|l| l.trim())
        .filter(|l| l.starts_with(&format!("{arg} =")) || l.starts_with(&format!("local {arg} =")))
        .count();
    assert!(assigns >= 3, "`{arg}` is not assigned on both arms (entry copy + 2 arm writes):\n{out}");
    assert!(
        !out.lines().any(|l| l.trim().starts_with("local v") && !l.contains('=')),
        "never-assigned hoisted local:\n{out}"
    );
}
