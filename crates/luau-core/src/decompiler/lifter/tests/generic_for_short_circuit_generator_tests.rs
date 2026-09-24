//! A short-circuit `a and b` is a legal generic-for iterable.
//!
//! `absorb_iterator_setup` whitelists the expression shapes it will put in a
//! `for ... in <generator>` header and replaces anything else with a minted
//! `vN`. `BinOp::And`/`Or` were outside the whitelist, but the guarded
//! iterable idiom
//!
//! ```lua
//! local d = self and self.Damages
//! if not d then return 1 end
//! for k in d do n += 1 end
//! ```
//!
//! parks exactly `self and self.Damages` in the generator register, so the
//! loop was emitted as `for k in v3 do` over a never-assigned `v3`
//! (`declared_never_assigned`; BloxFruits DamageScalingHelperUtil
//! `CountDealers`, AdoptMe dotrick / DynamicNarrative2026Client, BloxFruits
//! JuiceWindow, StealABrainrot Shared_Index).

use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, Constant, Proto};

// Canonical Luau opcode bytes (identity opmap, Chunk.version = 6).
const OP_LOADNIL: u8 = 2;
const OP_LOADN: u8 = 4;
const OP_MOVE: u8 = 6;
const OP_GETTABLEKS: u8 = 15;
const OP_RETURN: u8 = 22;
const OP_JUMPIF: u8 = 25;
const OP_JUMPIFNOT: u8 = 26;
const OP_ADDK: u8 = 39;
const OP_FORGPREP: u8 = 58;
const OP_FORGLOOP: u8 = 59;

fn insn_ad(op: u8, a: u8, d: i16) -> u32 {
    let du = d as u16 as u32;
    (op as u32) | ((a as u32) << 8) | (du << 16)
}

fn insn_abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
}

/// `CountDealers(self)`, hand-assembled from the measured bytecode:
///
/// ```text
///  0: MOVE        R1 R0
///  1: JUMPIFNOT   R1 -> 4
///  2: GETTABLEKS  R1 R0 K0 ("Damages")   (+AUX)
///  4: JUMPIF      R1 -> 7
///  5: LOADN       R2 1
///  6: RETURN      R2 2
///  7: LOADN       R2 0
///  8: MOVE        R3 R1                  generator = self and self.Damages
///  9: LOADNIL     R4
/// 10: LOADNIL     R5
/// 11: FORGPREP    R3 -> 13
/// 12: ADDK        R2 R2 K1 (1)
/// 13: FORGLOOP    R3 -> 12               (+AUX: 1 loop variable)
/// 15: RETURN      R2 2
/// ```
fn count_dealers_chunk() -> Chunk {
    let code = vec![
        insn_abc(OP_MOVE, 1, 0, 0),
        insn_ad(OP_JUMPIFNOT, 1, 2),
        insn_abc(OP_GETTABLEKS, 1, 0, 0),
        0u32, // AUX: constant index 0 ("Damages")
        insn_ad(OP_JUMPIF, 1, 2),
        insn_ad(OP_LOADN, 2, 1),
        insn_abc(OP_RETURN, 2, 2, 0),
        insn_ad(OP_LOADN, 2, 0),
        insn_abc(OP_MOVE, 3, 1, 0),
        insn_abc(OP_LOADNIL, 4, 0, 0),
        insn_abc(OP_LOADNIL, 5, 0, 0),
        insn_ad(OP_FORGPREP, 3, 1),
        insn_abc(OP_ADDK, 2, 2, 1),
        insn_ad(OP_FORGLOOP, 3, -2),
        1u32, // AUX: one loop variable
        insn_abc(OP_RETURN, 2, 2, 0),
    ];
    let proto = Proto {
        max_stack_size: 8,
        num_params: 1,
        num_upvalues: 0,
        is_vararg: false,
        flags: 0,
        typeinfo: None,
        code,
        constants: vec![Constant::String("Damages".to_string()), Constant::Number(1.0)],
        child_protos: Vec::new(),
        line_defined: 1,
        debug_name: Some("CountDealers".to_string()),
        line_info: None,
        debug_info: None,
    };
    Chunk {
        version: 6,
        types_version: 0,
        strings: vec!["Damages".to_string()],
        protos: vec![proto],
        main_proto: 0,
    }
}

#[test]
fn short_circuit_and_is_kept_as_the_generic_for_generator() {
    let chunk = count_dealers_chunk();
    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);
    eprintln!("=== emitted source ===\n{out}\n======================");

    let header = out
        .lines()
        .find(|l| l.trim_start().starts_with("for ") && l.contains(" in "))
        .unwrap_or_else(|| panic!("the generic-for was not emitted:\n{out}"));
    assert!(
        header.contains(" and ") && header.contains("Damages"),
        "the generator must be the parked `<self> and <self>.Damages`, not a \
         minted register name:\n{out}"
    );
    assert!(
        !header.contains(" in v3 ") && !header.contains(" in v3\n"),
        "the generator was replaced by the unbound placeholder `v3`:\n{out}"
    );
}
