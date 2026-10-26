//! `while true do <if/else> ... end` whose conditional sits at the end of one
//! basic block and whose arms leave the block by a `JUMP` was reduced to bare
//! `continue`s, dropping the arms and every call in them.
//!
//! Shape A (PetSimulator99 `Exclusive_Shop`): the fall-through arm ends in a
//! `JUMP` to the loop's JUMPBACK; the other arm falls into the JUMPBACK.
//! Shape B (Brookhaven `ShutdownMessageController`): an if/else value join,
//! `R1 = a` / `JUMP join` against `R1 = f()`, followed by more statements.

use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, Constant, Proto};

const OP_LOADK: u8 = 5;
const OP_MOVE: u8 = 6;
const OP_GETUPVAL: u8 = 9;
const OP_CALL: u8 = 21;
const OP_RETURN: u8 = 22;
const OP_JUMP: u8 = 23;
const OP_JUMPBACK: u8 = 24;
const OP_JUMPIF: u8 = 25;
const OP_LOADB: u8 = 3;
const OP_LOADN: u8 = 4;
const OP_JUMPIFEQ: u8 = 27;
const OP_JUMPIFLT: u8 = 29;

fn ad(op: u8, a: u8, d: i16) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((d as u16 as u32) << 16)
}

fn abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
}

fn chunk_of(code: Vec<u32>, name: &str) -> Chunk {
    let p = Proto {
        max_stack_size: 16,
        num_params: 0,
        num_upvalues: 5,
        is_vararg: false,
        flags: 0,
        typeinfo: None,
        code,
        constants: vec![Constant::String("alpha".into()), Constant::String("beta".into())],
        child_protos: Vec::new(),
        line_defined: 1,
        debug_name: Some(name.to_string()),
        line_info: None,
        debug_info: None,
    };
    Chunk { version: 6, types_version: 0, strings: vec![], protos: vec![p], main_proto: 0 }
}

fn lift(code: Vec<u32>, name: &str) -> String {
    let chunk = chunk_of(code, name);
    let mut ctx = DecompileContext::new(&chunk);
    decompile_proto(&mut ctx, &chunk.protos[0], 0, 0)
}

fn call_lines(out: &str) -> usize {
    out.lines().filter(|l| l.contains('(') && !l.trim_start().starts_with("while")).count()
}

#[test]
fn shape_a_then_arm_jumps_to_latch_else_arm_survives() {
    let code = vec![
        abc(OP_GETUPVAL, 0, 0, 0), // 0: cond
        ad(OP_JUMPIF, 0, 3),       // 1: true -> 5 (else arm)
        abc(OP_GETUPVAL, 1, 1, 0), // 2
        abc(OP_CALL, 1, 1, 1),     // 3: call A (then arm)
        ad(OP_JUMP, 0, 4),         // 4: -> 9 (the JUMPBACK)
        abc(OP_GETUPVAL, 1, 2, 0), // 5
        abc(OP_CALL, 1, 1, 1),     // 6: call B (else arm)
        abc(OP_GETUPVAL, 1, 3, 0), // 7
        abc(OP_CALL, 1, 1, 1),     // 8: call C (else arm)
        ad(OP_JUMPBACK, 0, -10),   // 9: -> 0
        abc(OP_RETURN, 0, 1, 0),   // 10
    ];
    let out = lift(code, "shape_a");
    assert_eq!(call_lines(&out), 3, "all three calls must be emitted:\n{}", out);
}

#[test]
fn shape_b_value_join_diamond_then_tail_calls_survive() {
    let code = vec![
        abc(OP_GETUPVAL, 0, 0, 0), // 0: cond
        ad(OP_JUMPIF, 0, 2),       // 1: true -> 4 (else arm)
        ad(OP_LOADK, 1, 0),        // 2: R1 = "alpha"
        ad(OP_JUMP, 0, 3),         // 3: -> 7 (join)
        abc(OP_GETUPVAL, 2, 3, 0), // 4
        abc(OP_CALL, 2, 1, 2),     // 5: R2 = f()
        abc(OP_MOVE, 1, 2, 0),     // 6: R1 = R2
        abc(OP_GETUPVAL, 2, 1, 0), // 7: join
        abc(OP_MOVE, 3, 1, 0),     // 8
        abc(OP_CALL, 2, 2, 1),     // 9: g(R1)
        abc(OP_GETUPVAL, 2, 2, 0), // 10
        abc(OP_CALL, 2, 1, 1),     // 11: h()
        ad(OP_JUMPIF, 1, 1),       // 12: exit -> 14
        ad(OP_JUMPBACK, 0, -14),   // 13: -> 0
        abc(OP_RETURN, 0, 1, 0),   // 14
    ];
    let out = lift(code, "shape_b");
    assert_eq!(call_lines(&out), 3, "f, g and h must all be emitted:\n{}", out);
    assert!(out.contains("alpha"), "then-arm value must survive:\n{}", out);
}

#[test]
fn shape_c_value_join_arms_without_calls_feed_a_call_in_the_join() {
    // `while true do wait(c and a or b) if x then break end end`: the arms hold
    // no call, the join block makes it.
    let code = vec![
        abc(OP_GETUPVAL, 0, 3, 0), // 0: R0 = wait
        abc(OP_GETUPVAL, 2, 0, 0), // 1: cond
        ad(OP_JUMPIF, 2, 2),       // 2: true -> 5 (else arm)
        abc(OP_GETUPVAL, 1, 1, 0), // 3: R1 = a
        ad(OP_JUMP, 0, 2),         // 4: -> 7 (join)
        abc(OP_GETUPVAL, 3, 2, 0), // 5
        abc(OP_MOVE, 1, 3, 0),     // 6: R1 = b
        abc(OP_CALL, 0, 2, 1),     // 7: join: wait(R1)
        ad(OP_JUMPIF, 2, 1),       // 8: exit -> 10
        ad(OP_JUMPBACK, 0, -10),   // 9: -> 0
        abc(OP_RETURN, 0, 1, 0),   // 10
    ];
    let out = lift(code, "shape_c");
    assert!(call_lines(&out) >= 1, "the join's call must be emitted:\n{}", out);
    assert!(!out.contains("continue"), "no bare continue may replace the arms:\n{}", out);
}

#[test]
fn shape_d_latch_jump_arm_with_conditional_in_else_part_is_not_lifted_inline() {
    // Brookhaven Jukebox: `if not c then wait() continue end; local near = d < 70;
    // if x ~= near then f() end; wait()`. The compare join and the second
    // conditional (jumping to the shared tail, not the latch) sit in the else
    // part; lifting them block by block printed `if x == near then continue end`
    // and skipped the tail wait.
    let code = vec![
        abc(OP_GETUPVAL, 0, 0, 0), // 0: cond
        ad(OP_JUMPIF, 0, 3),       // 1: true -> 5
        abc(OP_GETUPVAL, 1, 1, 0), // 2
        abc(OP_CALL, 1, 1, 1),     // 3: wait() in the no-cond arm
        ad(OP_JUMP, 0, 13),        // 4: -> 18 (the JUMPBACK)
        abc(OP_GETUPVAL, 2, 2, 0), // 5
        ad(OP_LOADN, 3, 70),       // 6
        ad(OP_JUMPIFLT, 2, 2),     // 7: R2 < R3 -> 10
        3,                         // 8: aux R3
        abc(OP_LOADB, 4, 0, 1),    // 9: R4 = false, skip
        abc(OP_LOADB, 4, 1, 0),    // 10: R4 = true
        abc(OP_GETUPVAL, 5, 3, 0), // 11
        ad(OP_JUMPIFEQ, 5, 3),     // 12: R5 == R4 -> 16
        4,                         // 13: aux R4
        abc(OP_GETUPVAL, 5, 4, 0), // 14
        abc(OP_CALL, 5, 1, 1),     // 15: notify()
        abc(OP_GETUPVAL, 5, 4, 0), // 16: shared tail
        abc(OP_CALL, 5, 1, 1),     // 17: wait()
        ad(OP_JUMPBACK, 0, -19),   // 18: -> 0
        abc(OP_RETURN, 0, 1, 0),   // 19
    ];
    let out = lift(code, "shape_d");
    // The shape stays on the per-block lift (known gutted, same as before the
    // family fix); what it must not do is print the half-lifted compare.
    assert!(!out.contains("== ("), "fake continue over the compare join:
{}", out);
}
