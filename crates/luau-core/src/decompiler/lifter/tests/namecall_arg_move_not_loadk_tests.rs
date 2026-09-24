//! Regression for the `nonbranch_dead_extend` campaign fix (round 3):
//! `Cooldown:SetAttribute("Percentage", Text2)` in
//! BloxFruits/ReplicatedStorage_Controllers_UI_MobileUIController.lua:297
//! decoded as `Cooldown:SetAttribute("Percentage", "CooldownLabel")`.
//!
//! Root cause, confirmed against the disassembled bytecode: the CALL
//! handler's "resolve Name arguments for string-arg methods" backward scan
//! (opcode_handlers.rs, near the B0.128 comment) accepted ANY instruction
//! that wrote the argument register and reinterpreted its raw D-field bits
//! as a constant table index — without checking that instruction was
//! actually a LOADK. Here the writer was `MOVE Rdst Rsrc` (an ABC-format
//! instruction copying a live DIV-result local into the argument
//! register); MOVE's D-field reinterpretation is just its B/C register
//! bytes (`B | C<<8`), which coincidentally equals a VALID string constant
//! slot in the real corpus file (K15="CooldownLabel", unrelated debug
//! text from an earlier `FindFirstChild("CooldownLabel")` in the same
//! function) — so a genuine numeric local got silently replaced by an
//! unrelated string literal.
//!
//! This test reproduces the exact shape with a MOVE from a DIV result
//! landing on the argument register, with a decoy string planted at the
//! constant index MOVE's raw D-bits would produce (source reg 8 → K8).
//! The fix requires the backward-scan's writer to actually decode as
//! LuauOpcode::LoadK before trusting its D-field as a constant index.

use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, Constant, Proto};

const OP_LOADN: u8 = 4;
const OP_LOADK: u8 = 5;
const OP_MOVE: u8 = 6;
const OP_GETGLOBAL: u8 = 7;
const OP_NAMECALL: u8 = 20;
const OP_CALL: u8 = 21;
const OP_RETURN: u8 = 22;
const OP_DIV: u8 = 36;

fn insn_ad(op: u8, a: u8, d: i16) -> u32 {
    let du = d as u16 as u32;
    (op as u32) | ((a as u32) << 8) | (du << 16)
}

fn insn_abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
}

fn make_proto(code: Vec<u32>, constants: Vec<Constant>) -> Proto {
    Proto {
        max_stack_size: 32,
        num_params: 0,
        num_upvalues: 0,
        is_vararg: false,
        flags: 0,
        typeinfo: None,
        code,
        constants,
        child_protos: Vec::new(),
        line_defined: 1,
        debug_name: Some("namecall_arg_move_not_loadk".to_string()),
        line_info: None,
        debug_info: None,
    }
}

fn make_chunk(proto: Proto) -> Chunk {
    Chunk {
        version: 6,
        types_version: 0,
        strings: vec!["s0".to_string(), "s1".to_string(), "s2".to_string()],
        protos: vec![proto],
        main_proto: 0,
    }
}

fn decompile(chunk: &Chunk) -> String {
    let mut ctx = DecompileContext::new(chunk);
    decompile_proto(&mut ctx, &chunk.protos[0], 0, 0)
}

#[test]
fn setattribute_move_arg_from_div_is_not_replaced_by_stale_constant() {
    // R0 = Cooldown (GETGLOBAL)
    // R6 = 10, R7 = 2 (LOADN)
    // R8 = R6 / R7        <- the real value ("Text2"-equivalent), reg 8
    // NAMECALL R1, R0, aux="SetAttribute"  -> R2 = self(Cooldown), R1 pending
    // R3 = "Percentage"   (LOADK, arg1)
    // R4 = R8             (MOVE, arg2 — the DIV result, NOT a LOADK)
    // CALL R1 args=(self,arg1,arg2)=3 results=0
    // RETURN
    //
    // K8 is a decoy: MOVE R4,R8 raw D-field bits equal
    // B(=8) | C(=0)<<8 = 8, so a buggy backward scan misreading MOVE's
    // D-field as a constant index lands exactly on K8. Planting a valid
    // identifier string there means only a real fix (checking the writer
    // is LOADK) keeps it out of the output.
    let code = vec![
        insn_ad(OP_GETGLOBAL, 0, 0),
        0u32, // GETGLOBAL R0 aux=K0 "Cooldown"
        insn_ad(OP_LOADN, 6, 10),
        insn_ad(OP_LOADN, 7, 2),
        insn_abc(OP_DIV, 8, 6, 7), // R8 = R6 / R7
        insn_abc(OP_NAMECALL, 1, 0, 0),
        1u32, // NAMECALL R1,R0 aux=K1 "SetAttribute"
        insn_ad(OP_LOADK, 3, 2), // R3 = K2 "Percentage"
        insn_abc(OP_MOVE, 4, 8, 0), // R4 = R8 (the DIV result)
        insn_abc(OP_CALL, 1, 4, 1), // CALL R1 args=3 results=0
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    let constants = vec![
        Constant::String("Cooldown".to_string()),      // K0
        Constant::String("SetAttribute".to_string()),  // K1
        Constant::String("Percentage".to_string()),    // K2
        Constant::Nil,                                  // K3
        Constant::Nil,                                  // K4
        Constant::Nil,                                  // K5
        Constant::Nil,                                  // K6
        Constant::Nil,                                  // K7
        Constant::String("WRONG_DECOY_VALUE".to_string()), // K8
    ];
    let chunk = make_chunk(make_proto(code, constants));
    let out = decompile(&chunk);
    assert!(
        !out.contains("WRONG_DECOY_VALUE"),
        "MOVE-from-DIV argument was misread as a LOADK constant index: {:?}",
        out
    );
    assert!(
        out.contains("SetAttribute(\"Percentage\""),
        "expected the SetAttribute call with its real first argument to survive: {:?}",
        out
    );
}
