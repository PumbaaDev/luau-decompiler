//! In-loop out-of-range conditional that skips a lone `JUMP` to the loop latch.
//!
//! `if idle < 60 then continue end` compiles to `JUMPIFNOTLT .. -> +2; JUMP
//! -> latch`. When the lift span ended on the conditional, the target counted
//! as out of range and the lifter emitted `if <taken> then continue end` (the
//! wrong polarity, and a `continue` to a mid-body pc) and then minted a second,
//! UNCONDITIONAL `continue` from the lone `JUMP`. Dead-code elimination
//! truncated the loop body at that `continue`: `AntiAFKController` emitted 3 of
//! its 8 calls. The fixture is the real script captured from a v14 client.

use crate::decompiler::lifter::opcode_handlers::lone_exit_jump_after;
use crate::decompiler::lifter::oor_guard::{note_consumed_jump, take_consumed_jump};
use crate::parser::types::Proto;

const ANTI_AFK: &[u8] = include_bytes!("fixtures/loop_skip_jump_antiafk.luac");

const OP_JUMP: u32 = 0x17;

fn jump(d: i16) -> u32 {
    OP_JUMP | ((d as u16 as u32) << 16)
}

#[test]
fn lone_forward_jump_after_a_skipping_conditional_is_recognised() {
    // pc0: <cond> (skips pc1), pc1: JUMP -> 5, pc2: body.
    let code = [0u32, jump(3), 0, 0, 0, 0];
    assert_eq!(lone_exit_jump_after(&code, 1, 2, Some(6)), Some(true));
    // The JUMP leaves the loop: that is a `break`.
    assert_eq!(lone_exit_jump_after(&code, 1, 2, Some(5)), Some(false));
}

#[test]
fn skipping_more_than_the_lone_jump_is_not_recognised() {
    let code = [0u32, jump(3), 0, 0, 0, 0];
    assert_eq!(lone_exit_jump_after(&code, 1, 3, Some(6)), None);
}

#[test]
fn backward_jump_is_not_recognised() {
    let code = [0u32, 0, jump(-3), 0];
    assert_eq!(lone_exit_jump_after(&code, 2, 3, Some(6)), None);
}

#[test]
fn consumed_jump_note_is_taken_exactly_once() {
    let proto = Proto {
        max_stack_size: 1,
        num_params: 0,
        num_upvalues: 0,
        is_vararg: false,
        flags: 0,
        typeinfo: None,
        code: Vec::new(),
        constants: Vec::new(),
        child_protos: Vec::new(),
        line_defined: 1,
        debug_name: None,
        line_info: None,
        debug_info: None,
    };
    assert!(!take_consumed_jump(&proto, 20));
    note_consumed_jump(&proto, 20);
    assert!(take_consumed_jump(&proto, 20));
    assert!(!take_consumed_jump(&proto, 20));
}

#[test]
fn anti_afk_loop_body_keeps_all_its_calls() {
    let out = crate::decompile(ANTI_AFK).expect("fixture decompiles");
    assert!(!out.contains("call recovery"), "the loop body was truncated:\n{out}");
    assert!(out.contains("AskRescueHop3:FireServer()"), "{out}");
    assert!(out.contains("fallbackTeleport()"), "{out}");
}
