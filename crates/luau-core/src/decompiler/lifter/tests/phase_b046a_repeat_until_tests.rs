//! Extracted from lifter.rs as part of Phase B0.52P6 (part 3).
//! `super::super::` references the `lifter` module.

//! Phase B0.46A — post-AST `while true do <body>; if cond then break end end`
//! → `repeat <body> until cond` rewriter.
//!
//! These tests target `convert_while_true_break_to_repeat` directly
//! (the AST-only, lifter-independent pass) so they don't depend on any
//! bytecode shape. They cover positive and negative cases plus a
//! nested-loop scenario.
//!
//! Six required tests + a couple of safety checks for the helper:
//!   * positive: bare cond
//!   * positive (inverted): `if not cond then break`
//!   * negative: extra stmt after the if-cond-break
//!   * negative: if has an else clause
//!   * negative: bare `break` at end (no wrapping if)
//!   * positive: nested repeat-until inside an outer loop body
//!   * sanity: `matches_repeat_until_shape` empty-body rejection
//!   * sanity: `matches_repeat_until_shape` then-body must be exactly Break
//!   * full ordering: convert runs BEFORE convert_single_pass_loops

use super::super::{convert_while_true_break_to_repeat, matches_repeat_until_shape,
    convert_single_pass_loops};
use crate::ast::{Expr, Stat, UnOp};

fn name(s: &str) -> Expr { Expr::Name(s.to_string()) }
fn assign(target: &str, value: Expr) -> Stat {
    Stat::Assign { targets: vec![Expr::Name(target.to_string())], values: vec![value] }
}
fn num(n: f64) -> Expr { Expr::Number(n) }

/// `if cond then break end` (no elseif, no else)
fn if_break(cond: Expr) -> Stat {
    Stat::If {
        condition: cond,
        then_body: vec![Stat::Break],
        elseif_clauses: vec![],
        else_body: None,
    }
}

/// Wrap stmts in `while true do ... end`
fn while_true(body: Vec<Stat>) -> Stat {
    Stat::While { condition: Expr::Bool(true), body }
}

// ─── Positive: bare cond ─────────────────────────────────────────

#[test]
fn b046a_positive_basic_cond() {
    // while true do x = 1; if cond then break end end
    // → repeat x = 1 until cond
    let mut stmts = vec![while_true(vec![
        assign("x", num(1.0)),
        if_break(name("cond")),
    ])];
    convert_while_true_break_to_repeat(&mut stmts);

    assert_eq!(stmts.len(), 1, "outer count unchanged");
    match &stmts[0] {
        Stat::Repeat { body, condition } => {
            assert_eq!(body.len(), 1, "body should contain only the assign; got {:?}", body);
            match &body[0] {
                Stat::Assign { targets, values } => {
                    assert_eq!(targets.len(), 1);
                    assert!(matches!(&targets[0], Expr::Name(n) if n == "x"));
                    assert!(matches!(&values[0], Expr::Number(n) if *n == 1.0));
                }
                other => panic!("expected Assign, got {:?}", other),
            }
            match condition {
                Expr::Name(n) => assert_eq!(n, "cond"),
                other => panic!("expected Name(\"cond\"), got {:?}", other),
            }
        }
        other => panic!("expected Stat::Repeat, got {:?}", other),
    }
}

// ─── Positive: inverted `not cond` ───────────────────────────────

#[test]
fn b046a_positive_inverted_not_cond() {
    // while true do x = 1; if not cond then break end end
    // → repeat x = 1 until not cond
    let not_cond = Expr::UnOp { op: UnOp::Not, operand: Box::new(name("cond")) };
    let mut stmts = vec![while_true(vec![
        assign("x", num(1.0)),
        if_break(not_cond),
    ])];
    convert_while_true_break_to_repeat(&mut stmts);

    assert_eq!(stmts.len(), 1);
    match &stmts[0] {
        Stat::Repeat { body, condition } => {
            assert_eq!(body.len(), 1);
            // Verbatim preservation: until-cond is still `not cond`
            match condition {
                Expr::UnOp { op: UnOp::Not, operand } => {
                    match operand.as_ref() {
                        Expr::Name(n) => assert_eq!(n, "cond"),
                        other => panic!("expected Name inside Not, got {:?}", other),
                    }
                }
                other => panic!("expected UnOp::Not, got {:?}", other),
            }
        }
        other => panic!("expected Stat::Repeat, got {:?}", other),
    }
}

// ─── Negative: extra stmt after the if-cond-break ────────────────

#[test]
fn b046a_negative_extra_stmt_after_if() {
    // while true do x = 1; if cond then break end; y = 2 end
    // → must remain a while-true (the if is not the LAST stmt).
    let mut stmts = vec![while_true(vec![
        assign("x", num(1.0)),
        if_break(name("cond")),
        assign("y", num(2.0)),
    ])];
    convert_while_true_break_to_repeat(&mut stmts);

    assert_eq!(stmts.len(), 1);
    match &stmts[0] {
        Stat::While { condition: Expr::Bool(true), body } => {
            assert_eq!(body.len(), 3, "body untouched");
        }
        other => panic!("expected unchanged Stat::While, got {:?}", other),
    }
}

// ─── Negative: if has an else clause ────────────────────────────

#[test]
fn b046a_negative_if_with_else() {
    // while true do x = 1; if cond then break else other() end end
    // → must remain a while-true (else clause forbids the rewrite).
    let if_with_else = Stat::If {
        condition: name("cond"),
        then_body: vec![Stat::Break],
        elseif_clauses: vec![],
        else_body: Some(vec![Stat::ExprStat(Expr::Call {
            func: Box::new(name("other")),
            args: vec![],
        })]),
    };
    let mut stmts = vec![while_true(vec![
        assign("x", num(1.0)),
        if_with_else,
    ])];
    convert_while_true_break_to_repeat(&mut stmts);

    assert_eq!(stmts.len(), 1);
    assert!(matches!(&stmts[0], Stat::While { condition: Expr::Bool(true), .. }),
        "while-with-else must NOT convert; got {:?}", stmts[0]);
}

// ─── Negative: bare break at end (no wrapping if) ───────────────

#[test]
fn b046a_negative_immediate_bare_break() {
    // while true do break end
    // → must remain a while-true (no condition to extract).
    let mut stmts = vec![while_true(vec![Stat::Break])];
    convert_while_true_break_to_repeat(&mut stmts);

    assert_eq!(stmts.len(), 1);
    match &stmts[0] {
        Stat::While { condition: Expr::Bool(true), body } => {
            assert_eq!(body.len(), 1);
            assert!(matches!(&body[0], Stat::Break));
        }
        other => panic!("bare-break while must NOT convert; got {:?}", other),
    }
}

// ─── Positive: nested repeat-until inside outer loop body ────────

#[test]
fn b046a_positive_nested_inside_outer_loop() {
    // for i = 1, 10 do
    //     while true do
    //         x = 1
    //         if cond then break end
    //     end
    // end
    // → for i = 1, 10 do
    //       repeat x = 1 until cond
    //   end
    let inner = while_true(vec![
        assign("x", num(1.0)),
        if_break(name("cond")),
    ]);
    let outer = Stat::NumericFor {
        var: "i".to_string(),
        start: num(1.0),
        stop: num(10.0),
        step: None,
        body: vec![inner],
    };
    let mut stmts = vec![outer];
    convert_while_true_break_to_repeat(&mut stmts);

    assert_eq!(stmts.len(), 1);
    match &stmts[0] {
        Stat::NumericFor { body, .. } => {
            assert_eq!(body.len(), 1);
            match &body[0] {
                Stat::Repeat { body: rep_body, condition } => {
                    assert_eq!(rep_body.len(), 1);
                    assert!(matches!(&rep_body[0], Stat::Assign { .. }));
                    assert!(matches!(condition, Expr::Name(n) if n == "cond"));
                }
                other => panic!("expected nested Stat::Repeat, got {:?}", other),
            }
        }
        other => panic!("outer must remain NumericFor, got {:?}", other),
    }
}

// ─── Sanity: matches_repeat_until_shape ─────────────────────────

#[test]
fn b046a_helper_rejects_empty_body() {
    // Just `if cond then break end` alone — body.len() == 1, no preceding
    // stmts. Must NOT match (we require a non-empty repeat body).
    let body = vec![if_break(name("cond"))];
    assert!(!matches_repeat_until_shape(&body));
}

#[test]
fn b046a_helper_rejects_then_body_more_than_break() {
    // if cond then x=1; break end — then_body has 2 stmts, not just Break.
    let bad_if = Stat::If {
        condition: name("cond"),
        then_body: vec![assign("x", num(1.0)), Stat::Break],
        elseif_clauses: vec![],
        else_body: None,
    };
    let body = vec![assign("y", num(2.0)), bad_if];
    assert!(!matches_repeat_until_shape(&body));
}

#[test]
fn b046a_helper_rejects_then_body_not_break() {
    // if cond then return end — then_body's single stmt is Return, not Break.
    let bad_if = Stat::If {
        condition: name("cond"),
        then_body: vec![Stat::Return { values: vec![] }],
        elseif_clauses: vec![],
        else_body: None,
    };
    let body = vec![assign("y", num(2.0)), bad_if];
    assert!(!matches_repeat_until_shape(&body));
}

#[test]
fn b046a_helper_rejects_elseif_clauses() {
    // if c1 then break elseif c2 then ... end
    let bad_if = Stat::If {
        condition: name("c1"),
        then_body: vec![Stat::Break],
        elseif_clauses: vec![(name("c2"), vec![assign("z", num(0.0))])],
        else_body: None,
    };
    let body = vec![assign("y", num(2.0)), bad_if];
    assert!(!matches_repeat_until_shape(&body));
}

// ─── Pipeline ordering: B0.46A runs BEFORE single-pass collapse ──

#[test]
fn b046a_pipeline_order_repeat_runs_before_single_pass_collapse() {
    // The whole point of running B0.46A first is so that
    // `convert_single_pass_loops` doesn't rewrite the trailing
    // `if cond then break end` into nested if/else and rob us of
    // the chance to spot the repeat-until shape.
    //
    // Verify by running both passes in the same order the production
    // pipeline does and confirming we end up with a Repeat (not nested
    // ifs).
    let mut stmts = vec![while_true(vec![
        assign("x", num(1.0)),
        if_break(name("cond")),
    ])];
    convert_while_true_break_to_repeat(&mut stmts);
    convert_single_pass_loops(&mut stmts);

    assert_eq!(stmts.len(), 1);
    assert!(matches!(&stmts[0], Stat::Repeat { .. }),
        "after B0.46A + single-pass-collapse, must be Repeat; got {:?}", stmts[0]);
}
