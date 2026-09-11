//! Extracted from lifter.rs as part of Phase B0.52P6 (part 3).
//! `super::super::` references the `lifter` module.

//! Phase B0.42 — elseif chain fusion + short-circuit and/or collapse.
//!
//! These tests operate directly on the AST so they don't depend on
//! bytecode-shape detection. They exercise:
//!   * `collapse_elseif_chains` — nested `else if` → `elseif`
//!   * `collapse_short_circuit_assignments` — `if/else` → `and`/`or`
//!     /ternary expressions
//!
//! Negative tests pin down the conservative cases where collapse must
//! NOT happen (multi-statement bodies, mismatched targets, elseif
//! present, condition isn't the named target, etc.).
use crate::ast::{BinOp, Expr, Stat, UnOp};
use super::super::{collapse_elseif_chains, collapse_short_circuit_assignments};

fn name(s: &str) -> Expr { Expr::Name(s.to_string()) }
fn assign(target: &str, value: Expr) -> Stat {
    Stat::Assign { targets: vec![name(target)], values: vec![value] }
}
fn if_simple(cond: Expr, then_body: Vec<Stat>, else_body: Option<Vec<Stat>>) -> Stat {
    Stat::If { condition: cond, then_body, elseif_clauses: vec![], else_body }
}

// ─── Elseif fusion ───────────────────────────────────────────────────

#[test]
fn b042_elseif_fusion_collapses_nested_else_if() {
    // if a then ... else if b then ... else ... end end
    let inner = if_simple(name("b"),
        vec![assign("x", Expr::Number(2.0))],
        Some(vec![assign("x", Expr::Number(3.0))]));
    let outer = if_simple(name("a"),
        vec![assign("x", Expr::Number(1.0))],
        Some(vec![inner]));
    let mut stmts = vec![outer];
    collapse_elseif_chains(&mut stmts);
    match &stmts[0] {
        Stat::If { elseif_clauses, else_body, .. } => {
            assert_eq!(elseif_clauses.len(), 1, "should have one elseif");
            assert!(else_body.is_some(), "else body should still be present");
        }
        _ => panic!("expected Stat::If"),
    }
}

#[test]
fn b042_elseif_fusion_chains_through_three_levels() {
    // if a ... else if b ... else if c ... else ... end end end → 2 elseifs
    let l3 = if_simple(name("c"),
        vec![assign("x", Expr::Number(3.0))],
        Some(vec![assign("x", Expr::Number(4.0))]));
    let l2 = if_simple(name("b"),
        vec![assign("x", Expr::Number(2.0))],
        Some(vec![l3]));
    let l1 = if_simple(name("a"),
        vec![assign("x", Expr::Number(1.0))],
        Some(vec![l2]));
    let mut stmts = vec![l1];
    collapse_elseif_chains(&mut stmts);
    if let Stat::If { elseif_clauses, else_body, .. } = &stmts[0] {
        assert_eq!(elseif_clauses.len(), 2, "should chain into two elseifs");
        assert!(else_body.is_some());
    } else { panic!("expected Stat::If"); }
}

#[test]
fn b042_elseif_fusion_does_not_collapse_when_else_has_extra_stmts() {
    // else block has an If PLUS another statement — should NOT collapse
    let inner = if_simple(name("b"), vec![assign("x", Expr::Number(2.0))], None);
    let extra = assign("y", Expr::Number(99.0));
    let outer = if_simple(name("a"),
        vec![assign("x", Expr::Number(1.0))],
        Some(vec![inner, extra]));
    let mut stmts = vec![outer];
    collapse_elseif_chains(&mut stmts);
    if let Stat::If { elseif_clauses, else_body, .. } = &stmts[0] {
        assert_eq!(elseif_clauses.len(), 0, "two-stmt else must NOT fuse");
        assert_eq!(else_body.as_ref().unwrap().len(), 2);
    } else { panic!("expected Stat::If"); }
}

// ─── Short-circuit AND ───────────────────────────────────────────────

#[test]
fn b042_short_circuit_and_collapses() {
    // if x then x = a end → x = x and a
    let mut stmts = vec![if_simple(
        name("x"),
        vec![assign("x", name("a"))],
        None,
    )];
    collapse_short_circuit_assignments(&mut stmts);
    match &stmts[0] {
        Stat::Assign { targets, values } => {
            assert!(matches!(&targets[0], Expr::Name(n) if n == "x"));
            if let Expr::BinOp { op, left, right } = &values[0] {
                assert_eq!(*op, BinOp::And);
                assert!(matches!(left.as_ref(), Expr::Name(n) if n == "x"));
                assert!(matches!(right.as_ref(), Expr::Name(n) if n == "a"));
            } else { panic!("expected BinOp"); }
        }
        other => panic!("expected Stat::Assign, got {:?}", other),
    }
}

// ─── Short-circuit OR ────────────────────────────────────────────────

#[test]
fn b042_short_circuit_or_collapses() {
    // if not x then x = a end → x = x or a
    let cond = Expr::UnOp { op: UnOp::Not, operand: Box::new(name("x")) };
    let mut stmts = vec![if_simple(cond, vec![assign("x", name("a"))], None)];
    collapse_short_circuit_assignments(&mut stmts);
    match &stmts[0] {
        Stat::Assign { values, .. } => {
            if let Expr::BinOp { op, .. } = &values[0] {
                assert_eq!(*op, BinOp::Or, "should produce OR for `if not X` shape");
            } else { panic!("expected BinOp"); }
        }
        other => panic!("expected Stat::Assign, got {:?}", other),
    }
}

// ─── Ternary ─────────────────────────────────────────────────────────

#[test]
fn b042_ternary_collapses() {
    // if c then x = a else x = b end → x = if c then a else b (Ternary)
    // Phase B0.86: now uses Expr::Ternary instead of `cond and a or b`
    // to avoid semantic incorrectness when `a` is falsy.
    let mut stmts = vec![if_simple(
        name("c"),
        vec![assign("x", name("a"))],
        Some(vec![assign("x", name("b"))]),
    )];
    collapse_short_circuit_assignments(&mut stmts);
    match &stmts[0] {
        Stat::Assign { targets, values } => {
            assert!(matches!(&targets[0], Expr::Name(n) if n == "x"));
            if let Expr::Ternary { cond, then_expr, else_expr } = &values[0] {
                assert!(matches!(cond.as_ref(), Expr::Name(n) if n == "c"));
                assert!(matches!(then_expr.as_ref(), Expr::Name(n) if n == "a"));
                assert!(matches!(else_expr.as_ref(), Expr::Name(n) if n == "b"));
            } else { panic!("expected Ternary, got {:?}", values[0]); }
        }
        other => panic!("expected Stat::Assign, got {:?}", other),
    }
}

// ─── Negative tests ──────────────────────────────────────────────────

#[test]
fn b042_no_collapse_when_then_has_multiple_stmts() {
    // Multi-statement then-body — must NOT collapse
    let mut stmts = vec![if_simple(
        name("x"),
        vec![assign("x", name("a")), assign("y", name("b"))],
        None,
    )];
    collapse_short_circuit_assignments(&mut stmts);
    assert!(matches!(&stmts[0], Stat::If { .. }), "must remain Stat::If");
}

#[test]
fn b042_no_collapse_when_targets_differ_in_ternary() {
    // if c then x = a else y = b end → not a ternary, leave alone
    let mut stmts = vec![if_simple(
        name("c"),
        vec![assign("x", name("a"))],
        Some(vec![assign("y", name("b"))]),
    )];
    collapse_short_circuit_assignments(&mut stmts);
    assert!(matches!(&stmts[0], Stat::If { .. }), "differing targets must stay as If");
}

#[test]
fn b042_no_collapse_when_condition_is_unrelated_to_target() {
    // if c then x = a end (no else) — c is NOT x, so we can't safely
    // collapse to `x = c and a` (would need register-equivalence info
    // that isn't present in the AST). Leave as If.
    let mut stmts = vec![if_simple(
        name("c"),
        vec![assign("x", name("a"))],
        None,
    )];
    collapse_short_circuit_assignments(&mut stmts);
    assert!(
        matches!(&stmts[0], Stat::If { .. }),
        "unrelated cond/target must NOT collapse without else branch"
    );
}

#[test]
fn b042_no_collapse_when_elseif_present() {
    // if c then x = a elseif d then x = b end — has elseif, keep as If
    let stmt = Stat::If {
        condition: name("c"),
        then_body: vec![assign("x", name("a"))],
        elseif_clauses: vec![(name("d"), vec![assign("x", name("b"))])],
        else_body: None,
    };
    let mut stmts = vec![stmt];
    collapse_short_circuit_assignments(&mut stmts);
    assert!(matches!(&stmts[0], Stat::If { .. }), "elseif present must keep If");
}

#[test]
fn b042_no_collapse_for_local_declaration() {
    // if x then local x = a end — local introduces new binding, must
    // NOT collapse (would change scoping).
    let mut stmts = vec![if_simple(
        name("x"),
        vec![Stat::Local { names: vec!["x".into()], values: vec![name("a")] }],
        None,
    )];
    collapse_short_circuit_assignments(&mut stmts);
    assert!(matches!(&stmts[0], Stat::If { .. }), "local decl must keep If");
}

#[test]
fn b042_collapse_recurses_into_nested_blocks() {
    // while true do if x then x = a end end → while true do x = x and a end
    let inner = if_simple(name("x"), vec![assign("x", name("a"))], None);
    let mut stmts = vec![Stat::While {
        condition: Expr::Bool(true),
        body: vec![inner],
    }];
    collapse_short_circuit_assignments(&mut stmts);
    if let Stat::While { body, .. } = &stmts[0] {
        assert!(matches!(body[0], Stat::Assign { .. }),
            "inner If should be collapsed inside While body");
    } else { panic!("expected Stat::While"); }
}

#[test]
fn b042_short_circuit_does_not_misfire_on_call_target() {
    // if x then obj.field = a end — target isn't a Name, keep alone
    let target = Expr::Field { object: Box::new(name("obj")), field: "field".into() };
    let mut stmts = vec![if_simple(
        name("x"),
        vec![Stat::Assign { targets: vec![target], values: vec![name("a")] }],
        None,
    )];
    collapse_short_circuit_assignments(&mut stmts);
    assert!(matches!(&stmts[0], Stat::If { .. }), "non-Name target must keep If");
}
