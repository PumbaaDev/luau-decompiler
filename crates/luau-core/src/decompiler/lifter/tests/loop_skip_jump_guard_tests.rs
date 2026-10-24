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
const OP_JUMPBACK: u32 = 24;
const OP_FORNLOOP: u32 = 57;
const OP_FORGLOOP: u32 = 59;

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

// pc0: <cond> (skips pc1), pc1: JUMP -> 4, pc2..3: body, pc4: latch, pc5: after.
fn latch_code(latch_op: u32) -> [u32; 6] {
    [0u32, jump(2), 0, 0, latch_op, 0]
}

#[test]
fn jump_to_a_fornloop_latch_is_continue_whatever_loop_end_says() {
    let code = latch_code(OP_FORNLOOP);
    // A numeric for sets no loop end at all.
    assert_eq!(lone_exit_jump_after(&code, 1, 2, None), Some(true));
    // A structured loop whose loop end is the latch pc itself.
    assert_eq!(lone_exit_jump_after(&code, 1, 2, Some(4)), Some(true));
    // One past the latch (exclusive end).
    assert_eq!(lone_exit_jump_after(&code, 1, 2, Some(5)), Some(true));
}

#[test]
fn jump_to_forgloop_and_jumpback_latches_are_continue() {
    assert_eq!(lone_exit_jump_after(&latch_code(OP_FORGLOOP), 1, 2, Some(4)), Some(true));
    assert_eq!(lone_exit_jump_after(&latch_code(OP_JUMPBACK), 1, 2, Some(4)), Some(true));
}

#[test]
fn jump_past_the_latch_is_break() {
    // pc1: JUMP -> 5, which is the instruction after the FORNLOOP at pc4.
    let code = [0u32, jump(3), 0, 0, OP_FORNLOOP, 0];
    assert_eq!(lone_exit_jump_after(&code, 1, 2, Some(5)), Some(false));
    assert_eq!(lone_exit_jump_after(&code, 1, 2, Some(4)), Some(false));
    assert_eq!(lone_exit_jump_after(&code, 1, 2, None), Some(false));
}

#[test]
fn jump_to_a_latch_beyond_the_loop_end_is_not_continue() {
    // An enclosing loop's end that stops short of the target latch.
    let code = latch_code(OP_FORNLOOP);
    assert_eq!(lone_exit_jump_after(&code, 1, 2, Some(3)), Some(false));
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

// A body-level `v = e` read once in the body and once in `until v ...` must
// survive: the loop condition's read is invisible to the per-block count, and
// deleting the assignment left `until` reading the stale `local v = nil`
// (rendered as `until 39629 == nil`).
#[test]
fn until_condition_read_keeps_a_body_assignment() {
    use crate::ast::{BinOp, Expr, Stat};
    use crate::decompiler::lifter::post_passes::inline_single_use_temps;

    let name = |n: &str| Expr::Name(n.to_string());
    let cmp = |n: &str, k: f64| Expr::BinOp {
        left: Box::new(name(n)),
        op: BinOp::Eq,
        right: Box::new(Expr::Number(k)),
    };
    let mut stmts = vec![
        Stat::Local { names: vec!["v".into()], values: vec![Expr::Nil] },
        Stat::Repeat {
            body: vec![
                Stat::Assign { targets: vec![name("v")], values: vec![name("bb")] },
                Stat::If {
                    condition: cmp("v", 1.0),
                    then_body: vec![Stat::Continue],
                    elseif_clauses: vec![],
                    else_body: None,
                },
            ],
            condition: cmp("v", 2.0),
        },
    ];
    inline_single_use_temps(&mut stmts);
    let Stat::Repeat { body, condition } = &stmts[stmts.len() - 1] else {
        panic!("repeat statement lost: {stmts:?}");
    };
    assert!(
        body.iter().any(|s| matches!(s, Stat::Assign { targets, .. } if format!("{targets:?}") == format!("{:?}", vec![name("v")]))),
        "assignment feeding the until condition was deleted: {body:?}"
    );
    assert_eq!(format!("{condition:?}"), format!("{:?}", cmp("v", 2.0)));
}

// Blend `Gj`: `if v == 30269 then continue end; if v == -2 then return -2, r end`
// inside `while true`. The `return` block is not a loop-body block, so the span
// of the second conditional ends where it starts and the fall-through leaves the
// loop: the guard must be followed by `break` or the return is unreachable.
#[test]
fn fall_through_into_a_block_outside_the_loop_body_leaves_the_loop() {
    use crate::decompiler::lifter::opcode_handlers::fallthrough_leaves_loop;

    // Body blocks [0,3) and [3,5); exit block [5,8) is not a body block; the
    // latch (a lone JUMPBACK) is [8,9).
    let mut code = [0u32; 9];
    code[8] = OP_JUMPBACK;
    let body = [(0usize, 3usize), (3, 5), (8, 9)];
    assert!(fallthrough_leaves_loop(Some(&body), &code, 5));
    // The next code is a body block: the body goes on, no break.
    let body2 = [(0usize, 3usize), (3, 5), (5, 8), (8, 9)];
    assert!(!fallthrough_leaves_loop(Some(&body2), &code, 5));
    // Resuming inside a body block.
    assert!(!fallthrough_leaves_loop(Some(&body), &code, 4));
    // Real body code beyond the exit block would be shadowed by a break.
    let body3 = [(0usize, 3usize), (3, 5), (6, 8), (8, 9)];
    assert!(!fallthrough_leaves_loop(Some(&body3), &code, 5));
    // Not a block-by-block `while true` lift.
    assert!(!fallthrough_leaves_loop(None, &code, 5));
}

// A numeric for sets its loop end to one past FORNLOOP. A plain forward JUMP
// onto the FORNLOOP is `continue` (RigDriver pc 139 -> 144); a jump past it
// breaks; a jump to a non-latch pc inside the body keeps the `break` reading.
#[test]
fn numeric_for_plain_jump_to_fornloop_is_continue() {
    use crate::decompiler::lifter::opcode_handlers::forward_target_is_continue;

    // pc0..3 body, pc4 FORNLOOP, pc5 after, pc6 more.
    let code = [0u32, 0, 0, 0, OP_FORNLOOP, 0, 0];
    // A numeric for sets no loop end.
    assert!(forward_target_is_continue(&code, 4, None));
    assert!(!forward_target_is_continue(&code, 5, None));
    assert!(!forward_target_is_continue(&code, 6, None));
    assert!(!forward_target_is_continue(&code, 2, None));
}
