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

use crate::decompiler::EmittedLoop;

fn el(latch: usize) -> Option<EmittedLoop> {
    Some(EmittedLoop::with_latch(Some(latch)))
}

const OP_JUMP: u32 = 0x17;
const OP_JUMPBACK: u32 = 24;
const OP_FORNLOOP: u32 = 57;
const OP_FORGLOOP: u32 = 59;

fn jump(d: i16) -> u32 {
    OP_JUMP | ((d as u16 as u32) << 16)
}

fn edge(op: u32, d: i16) -> u32 {
    op | ((d as u16 as u32) << 16)
}

// pc0: <cond> (skips pc1), pc1: JUMP -> 4, pc2..3: body, pc4: latch (head pc0),
// pc5: after.
fn latch_code(latch_op: u32) -> [u32; 6] {
    [0u32, jump(2), 0, 0, edge(latch_op, -5), 0]
}

#[test]
fn jump_to_the_own_latch_is_continue_whatever_loop_end_says() {
    for op in [OP_FORNLOOP, OP_FORGLOOP, OP_JUMPBACK] {
        let code = latch_code(op);
        // A numeric for sets no loop end; a while sets the latch pc; a generic
        // for sets one past it.
        for le in [None, Some(4), Some(5)] {
            assert_eq!(lone_exit_jump_after(&code, 1, 2, le, None), Some(true), "op {op} le {le:?}");
        }
    }
}

#[test]
fn jump_past_the_latch_is_break() {
    // pc1: JUMP -> 5, the instruction after the FORNLOOP at pc4.
    let code = [0u32, jump(3), 0, 0, edge(OP_FORNLOOP, -5), 0];
    for le in [None, Some(4), Some(5)] {
        assert_eq!(lone_exit_jump_after(&code, 1, 2, le, None), Some(false));
    }
}

#[test]
fn mid_body_forward_skip_is_not_a_latch_jump() {
    // pc1: JUMP -> 3 lands mid-body, before the latch at pc4: neither
    // continue nor break, so the lone path must not fire.
    let code = [0u32, jump(1), 0, 0, edge(OP_FORNLOOP, -5), 0];
    assert_eq!(lone_exit_jump_after(&code, 1, 2, Some(5), None), None);
}

// Nested loops: the inner exit is the outer latch. pc0 outer head, pc1 inner
// head, pc2 cond, pc3 JUMP -> 6, pc4 body, pc5 inner FORNLOOP (head pc1),
// pc6 outer FORNLOOP (head pc0).
#[test]
fn inner_loop_exit_that_equals_the_outer_latch_is_break() {
    use crate::decompiler::lifter::opcode_handlers::forward_target_is_continue;

    let code = [0u32, 0, 0, jump(2), 0, edge(OP_FORNLOOP, -5), edge(OP_FORNLOOP, -7), 0];
    // From inside the inner body, the inner latch is a continue...
    assert!(forward_target_is_continue(&code, 3, 5, None, None));
    // ...but the outer latch (past the inner loop) is a break.
    assert!(!forward_target_is_continue(&code, 3, 6, None, None));
    assert!(!forward_target_is_continue(&code, 3, 6, Some(8), None));
    assert_eq!(lone_exit_jump_after(&code, 3, 4, Some(8), None), Some(false));
    // From the outer body only (pc 0), the outer latch is a continue.
    assert!(forward_target_is_continue(&code, 0, 6, None, None));
}

#[test]
fn skipping_more_than_the_lone_jump_is_not_recognised() {
    let code = [0u32, jump(3), 0, 0, 0, 0];
    assert_eq!(lone_exit_jump_after(&code, 1, 3, Some(6), None), None);
}

#[test]
fn backward_jump_is_not_recognised() {
    let code = [0u32, 0, jump(-3), 0];
    assert_eq!(lone_exit_jump_after(&code, 2, 3, Some(6), None), None);
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
fn anti_afk_idle_guard_jumping_to_the_latch_is_a_continue() {
    // Only the pc 18/20 guard jumps to the loop's own JUMPBACK (pc 76) and is a
    // `continue`. The guards at pc 22/24 and 55/57 skip forward MID-BODY; they
    // are not latch jumps and are deliberately left alone, so the body after
    // them is still truncated (no longer 8/8 calls).
    let out = crate::decompile(ANTI_AFK).expect("fixture decompiles");
    assert!(out.contains("< 0x3C then"), "{out}");
    assert!(!out.contains("if v7 then\n                    continue"), "{out}");
}

// `if not v7 then <set + FireServer> end` in the AntiAFK poll loop compiles to a
// conditional that skips a lone mid-body `JUMP`. Reading every forward target
// inside the loop as `continue` emitted `if not v7 then continue end; continue`
// and dead-code elimination then cut the rest of the body (5 calls lost).
#[test]
fn anti_afk_mid_body_forward_skip_keeps_the_rest_of_the_loop_body() {
    let out = crate::decompile(ANTI_AFK).expect("fixture decompiles");
    assert!(!out.contains("fewer call(s)"), "calls lost after a mid-body skip:\n{out}");
    assert!(!out.contains("if not v7 then\n                    continue"), "{out}");
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

// A plain forward JUMP onto the loop's own FORNLOOP is `continue` (RigDriver
// pc 139 -> 144); a jump past it breaks; a jump to a non-latch pc inside the
// body keeps the old `break` reading.
#[test]
fn numeric_for_plain_jump_to_fornloop_is_continue() {
    use crate::decompiler::lifter::opcode_handlers::forward_target_is_continue;

    // pc0..3 body (head pc0), pc4 FORNLOOP, pc5 after, pc6 more.
    let code = [0u32, 0, 0, 0, edge(OP_FORNLOOP, -5), 0, 0];
    assert!(forward_target_is_continue(&code, 2, 4, None, None));
    assert!(!forward_target_is_continue(&code, 2, 5, None, None));
    assert!(!forward_target_is_continue(&code, 2, 6, None, None));
    assert!(!forward_target_is_continue(&code, 2, 3, None, None));
}

// Two collapsed raw loops under one emitted `while` (PetSimulator99 GIF2 pc45:
// JUMP to the OUTER raw for's FORNLOOP, which exits the inner for = break). Only
// the emitted loop's own back edge (pc7) is a continue.
#[test]
fn jump_to_a_collapsed_outer_loops_latch_stays_break_under_an_emitted_loop() {
    use crate::decompiler::lifter::opcode_handlers::forward_target_is_continue;

    // pc0 head, pc1 inner head, pc3 JUMP -> 6, pc5 inner FORNLOOP (head 1),
    // pc6 outer-raw FORNLOOP (head 0), pc7 emitted JUMPBACK (head 0).
    let code = [0u32, 0, 0, jump(2), 0, edge(OP_FORNLOOP, -5), edge(OP_FORNLOOP, -6),
        edge(OP_JUMPBACK, -8), 0];
    assert!(!forward_target_is_continue(&code, 3, 6, None, el(7)));
    assert_eq!(lone_exit_jump_after(&code, 3, 4, Some(9), el(7)), Some(false));
    assert!(forward_target_is_continue(&code, 3, 7, None, el(7)));
}

// Collapsed inner loop: raw bytecode has an inner loop (latch pc5, head pc1)
// inside the outer one (latch pc6), but the emitted output has no inner loop.
// The innermost EMITTED loop is the outer one, so a jump to pc6 is a continue
// (RigDriver n2 pc29, ContentCatalog lS pc77). With the inner loop emitted
// (emitted latch = pc5) the same jump is a break.
#[test]
fn jump_to_outer_latch_is_continue_when_the_inner_loop_was_collapsed() {
    use crate::decompiler::lifter::opcode_handlers::forward_target_is_continue;

    let code = [0u32, 0, 0, jump(2), 0, edge(OP_FORNLOOP, -5), edge(OP_FORNLOOP, -7), 0];
    assert!(forward_target_is_continue(&code, 3, 6, None, el(6)));
    assert_eq!(lone_exit_jump_after(&code, 3, 4, Some(8), el(6)), Some(true));
    // The collapsed inner loop's own latch (pc5) stays a continue (RigDriver n2
    // pc18 -> inner FORNLOOP).
    assert!(forward_target_is_continue(&code, 3, 5, None, el(6)));
    // Inner loop emitted: break, as before.
    assert!(!forward_target_is_continue(&code, 3, 6, None, el(5)));
    assert_eq!(lone_exit_jump_after(&code, 3, 4, Some(8), el(5)), Some(false));
    // An emitted latch that does not enclose the jump is ignored.
    assert!(!forward_target_is_continue(&code, 3, 6, None, el(7)));
}

// (B) A loop that prints its condition recompute as trailing body statements
// (JailbreakBanJail ChatMain pc29 -> JUMPBACK pc37, BloxFruits SetUpSkills
// `min3 = math.min(task.wait(), 0.05)`) must not `continue` to its latch.
#[test]
fn latch_jump_is_break_when_the_recompute_is_a_trailing_statement() {
    use crate::decompiler::lifter::opcode_handlers::forward_target_is_continue;

    let code = [0u32, jump(2), 0, 0, edge(OP_JUMPBACK, -5), 0];
    let rotated = Some(EmittedLoop { latch: Some(4), trailing_recompute: true });
    assert!(!forward_target_is_continue(&code, 1, 4, None, rotated));
    assert_eq!(lone_exit_jump_after(&code, 1, 2, None, rotated), Some(false));
    // The pre-latch-fix reading (`jt < loop_end`) is kept for such a loop.
    assert!(forward_target_is_continue(&code, 1, 4, Some(9), rotated));
    assert_eq!(lone_exit_jump_after(&code, 1, 2, Some(9), rotated), Some(true));
    // Without a trailing recompute the same jump is a continue.
    let plain = Some(EmittedLoop::with_latch(Some(4)));
    assert!(forward_target_is_continue(&code, 1, 4, None, plain));
    assert_eq!(lone_exit_jump_after(&code, 1, 2, None, plain), Some(true));
}

// Sae Blend `we`: the lift of `if v4 <= 0 then <call> end` re-declared the
// register that an earlier pure literal (`result22 = 0`) lives in, under the
// SAME name (`local result22, result23 = self:ye(..)`). Name-based inlining
// then propagated `0` into `v4 = result22` inside the arm: a fabricated write.
#[test]
fn inlining_a_literal_never_rewrites_reads_of_a_shadowing_local() {
    use crate::ast::{BinOp, Expr, Stat};
    use crate::decompiler::lifter::post_passes::{inline_pure_literals, inline_single_use_temps};

    let name = |n: &str| Expr::Name(n.to_string());
    let arm = || {
        vec![
            Stat::Local {
                names: vec!["result22".into(), "result23".into()],
                values: vec![Expr::Call { func: Box::new(name("ye")), args: vec![] }],
            },
            Stat::Assign { targets: vec![name("v4")], values: vec![name("result22")] },
        ]
    };
    let make = |def: Stat| {
        vec![
            def,
            Stat::If {
                condition: Expr::BinOp {
                    left: Box::new(name("v4")),
                    op: BinOp::LE,
                    right: Box::new(Expr::Number(0.0)),
                },
                then_body: arm(),
                elseif_clauses: vec![],
                else_body: None,
            },
        ]
    };
    let assign = || Stat::Assign { targets: vec![name("result22")], values: vec![Expr::Number(0.0)] };
    let local = || Stat::Local { names: vec!["result22".into()], values: vec![Expr::Number(0.0)] };
    for def in [assign(), local()] {
        let mut stmts = make(def);
        inline_single_use_temps(&mut stmts);
        inline_pure_literals(&mut stmts);
        let Some(Stat::If { then_body, .. }) = stmts.iter().find(|s| matches!(s, Stat::If { .. })) else {
            panic!("if lost: {stmts:?}");
        };
        let rendered = format!("{then_body:?}");
        assert!(rendered.contains("Name(\"result22\")") && !rendered.contains("Number(0.0)"), "{rendered}");
    }
}

// Sae Blend `we`: `if v4 < 101 then <exit> end` jumps to the loop-exit pad
// (`DIVK; JUMP past the latch`), code inside the loop's pc span that is NOT a
// body block. It leaves the loop, so it is a `break`, not a `continue`.
#[test]
fn jump_to_a_loop_exit_pad_is_not_a_continue() {
    use crate::decompiler::lifter::opcode_handlers::lands_on_loop_exit_pad;

    // Body blocks [0,3) [3,6); pad [6,8) is outside the body; latch [8,9)
    // jumps back to pc 0.
    let mut code = [0u32; 10];
    code[8] = edge(OP_JUMPBACK, -9);
    let body = [(0usize, 3usize), (3, 6), (8, 9)];
    assert!(lands_on_loop_exit_pad(&code, 2, 6, Some(&body)), "pad target");
    assert!(!lands_on_loop_exit_pad(&code, 2, 3, Some(&body)), "body block target");
    assert!(!lands_on_loop_exit_pad(&code, 2, 8, Some(&body)), "latch target");
    assert!(!lands_on_loop_exit_pad(&code, 2, 6, None), "no body known");
}

// ClientSynchronizer `k`: an arm writes a parameter register (`GETIMPORT R1`)
// that the code after the arm returns. A nested-`if` lift restores the
// pre-branch view and loses it, so the widening must leave such arms alone.
#[test]
fn live_out_register_is_read_before_overwrite() {
    use crate::decompiler::lifter::opcode_handlers::reg_read_before_overwrite;
    use crate::parser::opcodes::LuauOpcode;

    let abc = |op: LuauOpcode, a: u32, b: u32, c: u32| (op as u32) | (a << 8) | (b << 16) | (c << 24);
    // pc0: MOVE R5 R1 (reads R1, writes R5)   pc1: MOVE R1 R7 (overwrites R1)
    // pc2: MOVE R6 R1 (reads the NEW R1)
    let code = [
        abc(LuauOpcode::Move, 5, 1, 0),
        abc(LuauOpcode::Move, 1, 7, 0),
        abc(LuauOpcode::Move, 6, 1, 0),
    ];
    assert!(reg_read_before_overwrite(&code, 0, 1), "read at pc0");
    assert!(!reg_read_before_overwrite(&code, 1, 1), "overwritten before the pc2 read");
    assert!(reg_read_before_overwrite(&code, 2, 1), "read at pc2");
    assert!(!reg_read_before_overwrite(&code, 0, 9), "never touched");
}
