//! A register nil-seeded before a `while` loop, written inside the loop by
//! `CALL Rt ; MOVE Rcarried Rt`, and READ AFTER the loop must keep that
//! write. The loop's first touch of the register is a MOVE definition, so
//! `reg_dead_on_entry` classed it as a per-iteration scratch and skipped
//! premateralization; but when the header test exits before the body runs,
//! the pre-loop value is what the post-loop read sees. The write then folded
//! into the `if` condition and the post-loop read became an unbound name.
//!
//! Bytecode exhibit: BloxFruits `ReplicatedStorage_FriendFinder`, proto @119:
//! `local friends; while not cancelled do friends = X:Get(id)
//!  if friends then break end; task.wait(1) end; cb(friends or {})`.

use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, Constant, Proto};

const OP_LOADNIL: u8 = 2;
const OP_MOVE: u8 = 6;
const OP_GETUPVAL: u8 = 9;
const OP_CALL: u8 = 21;
const OP_RETURN: u8 = 22;
const OP_JUMPBACK: u8 = 24;
const OP_JUMPIF: u8 = 25;

fn insn_ad(op: u8, a: u8, d: i16) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((d as u16 as u32) << 16)
}

fn insn_abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
}

fn proto(code: Vec<u32>, constants: Vec<Constant>, num_params: u8, upvals: u8, name: &str) -> Proto {
    Proto {
        max_stack_size: 16,
        num_params,
        num_upvalues: upvals,
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

/// params: R0 = arg, R1 = cancelled flag. R4 is the carried register.
fn chunk() -> Chunk {
    let code = vec![
        insn_abc(OP_LOADNIL, 4, 0, 0),  // 0
        insn_ad(OP_JUMPIF, 1, 6),       // 1: header test, exit -> 8
        insn_abc(OP_GETUPVAL, 5, 0, 0), // 2
        insn_abc(OP_MOVE, 6, 0, 0),     // 3
        insn_abc(OP_CALL, 5, 2, 2),     // 4
        insn_abc(OP_MOVE, 4, 5, 0),     // 5: carried write
        insn_ad(OP_JUMPIF, 4, 1),       // 6: break when truthy -> 8
        insn_ad(OP_JUMPBACK, 0, -7),    // 7: -> 1
        insn_abc(OP_MOVE, 7, 4, 0),     // 8: post-loop read
        insn_abc(OP_RETURN, 7, 2, 0),   // 9
    ];
    Chunk {
        version: 6,
        types_version: 0,
        strings: vec![],
        protos: vec![proto(code, vec![], 2, 1, "carried_exit")],
        main_proto: 0,
    }
}

#[test]
fn in_loop_call_write_survives_when_read_after_loop() {
    let chunk = chunk();
    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);
    let has_assign_of_call = out.lines().any(|l| {
        let l = l.trim_start();
        !l.starts_with("local ") && !l.starts_with("if ") && l.contains(" = ") && l.contains('(')
    });
    assert!(
        has_assign_of_call,
        "the loop's `carried = call(..)` write must be emitted:\n{}",
        out
    );
}
