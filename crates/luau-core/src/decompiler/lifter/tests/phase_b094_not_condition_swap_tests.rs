//! Phase B0.94 tests: swap branches when condition is `not X` to eliminate
//! unnecessary negation in ternary expressions and if-statements.

use crate::ast::{BinOp, Expr, Stat, UnOp};
use super::super::{simplify_expr, simplify_stmts};

fn name(s: &str) -> Expr { Expr::Name(s.to_string()) }

// ─── Ternary `not` swap ──────────────────────────────────────────────

#[test]
fn b094_ternary_not_cond_swaps_branches() {
    // if not x then a else b  →  if x then b else a
    let expr = Expr::Ternary {
        cond: Box::new(Expr::UnOp {
            op: UnOp::Not,
            operand: Box::new(name("x")),
        }),
        then_expr: Box::new(name("a")),
        else_expr: Box::new(name("b")),
    };
    let simplified = simplify_expr(&expr);
    match &simplified {
        Expr::Ternary { cond, then_expr, else_expr } => {
            assert!(matches!(cond.as_ref(), Expr::Name(n) if n == "x"),
                "cond should be x, got {:?}", cond);
            assert!(matches!(then_expr.as_ref(), Expr::Name(n) if n == "b"),
                "then should be b (swapped), got {:?}", then_expr);
            assert!(matches!(else_expr.as_ref(), Expr::Name(n) if n == "a"),
                "else should be a (swapped), got {:?}", else_expr);
        }
        other => panic!("expected Ternary, got {:?}", other),
    }
}

#[test]
fn b094_ternary_no_not_preserved() {
    // if x then a else b  →  stays as-is (no negation to strip)
    let expr = Expr::Ternary {
        cond: Box::new(name("x")),
        then_expr: Box::new(name("a")),
        else_expr: Box::new(name("b")),
    };
    let simplified = simplify_expr(&expr);
    match &simplified {
        Expr::Ternary { cond, then_expr, else_expr } => {
            assert!(matches!(cond.as_ref(), Expr::Name(n) if n == "x"));
            assert!(matches!(then_expr.as_ref(), Expr::Name(n) if n == "a"));
            assert!(matches!(else_expr.as_ref(), Expr::Name(n) if n == "b"));
        }
        other => panic!("expected Ternary, got {:?}", other),
    }
}

#[test]
fn b094_ternary_not_comparison_folds_and_swaps() {
    // if not (a == b) then x else y  →  if a ~= b then x else y
    // (not (==) folds to ~= in simplify_expr, then no `not` remains to swap)
    let expr = Expr::Ternary {
        cond: Box::new(Expr::UnOp {
            op: UnOp::Not,
            operand: Box::new(Expr::BinOp {
                left: Box::new(name("a")),
                op: BinOp::Eq,
                right: Box::new(name("b")),
            }),
        }),
        then_expr: Box::new(name("x")),
        else_expr: Box::new(name("y")),
    };
    let simplified = simplify_expr(&expr);
    // not (==) → ~= in simplify_expr UnOp handler, so no swap needed
    match &simplified {
        Expr::Ternary { cond, then_expr, else_expr } => {
            assert!(matches!(cond.as_ref(), Expr::BinOp { op: BinOp::NotEq, .. }),
                "not (==) should fold to ~=, got {:?}", cond);
            // Branches stay in original order since the `not` was absorbed
            assert!(matches!(then_expr.as_ref(), Expr::Name(n) if n == "x"));
            assert!(matches!(else_expr.as_ref(), Expr::Name(n) if n == "y"));
        }
        other => panic!("expected Ternary, got {:?}", other),
    }
}

// ─── Stat::If `not` swap ─────────────────────────────────────────────

#[test]
fn b094_if_not_cond_swaps_branches() {
    // if not x then A else B end  →  if x then B else A end
    let mut stmts = vec![Stat::If {
        condition: Expr::UnOp {
            op: UnOp::Not,
            operand: Box::new(name("x")),
        },
        then_body: vec![Stat::Return { values: vec![name("a")] }],
        elseif_clauses: vec![],
        else_body: Some(vec![Stat::Return { values: vec![name("b")] }]),
    }];
    simplify_stmts(&mut stmts);
    match &stmts[0] {
        Stat::If { condition, then_body, else_body, .. } => {
            assert!(matches!(condition, Expr::Name(n) if n == "x"),
                "condition should be x, got {:?}", condition);
            // then_body should now contain the former else (return b)
            if let Stat::Return { values } = &then_body[0] {
                assert!(matches!(&values[0], Expr::Name(n) if n == "b"),
                    "then should now return b (swapped), got {:?}", values[0]);
            } else {
                panic!("expected Return in then_body");
            }
            // else_body should now contain the former then (return a)
            let eb = else_body.as_ref().unwrap();
            if let Stat::Return { values } = &eb[0] {
                assert!(matches!(&values[0], Expr::Name(n) if n == "a"),
                    "else should now return a (swapped), got {:?}", values[0]);
            } else {
                panic!("expected Return in else_body");
            }
        }
        other => panic!("expected If, got {:?}", other),
    }
}

#[test]
fn b094_if_not_cond_no_else_preserved() {
    // if not x then A end  →  stays (no else to swap with)
    let mut stmts = vec![Stat::If {
        condition: Expr::UnOp {
            op: UnOp::Not,
            operand: Box::new(name("x")),
        },
        then_body: vec![Stat::Return { values: vec![name("a")] }],
        elseif_clauses: vec![],
        else_body: None,
    }];
    simplify_stmts(&mut stmts);
    match &stmts[0] {
        Stat::If { condition, else_body, .. } => {
            // Should remain `not x` since there's no else to swap with
            assert!(matches!(condition, Expr::UnOp { op: UnOp::Not, .. }),
                "condition should stay as `not x`, got {:?}", condition);
            assert!(else_body.is_none());
        }
        other => panic!("expected If, got {:?}", other),
    }
}

#[test]
fn b094_if_not_cond_with_elseif_preserved() {
    // if not x then A elseif y then B else C end  →  stays (has elseif)
    let mut stmts = vec![Stat::If {
        condition: Expr::UnOp {
            op: UnOp::Not,
            operand: Box::new(name("x")),
        },
        then_body: vec![Stat::Return { values: vec![name("a")] }],
        elseif_clauses: vec![
            (name("y"), vec![Stat::Return { values: vec![name("b")] }]),
        ],
        else_body: Some(vec![Stat::Return { values: vec![name("c")] }]),
    }];
    simplify_stmts(&mut stmts);
    match &stmts[0] {
        Stat::If { condition, elseif_clauses, .. } => {
            // Should not swap because elseif exists
            assert!(matches!(condition, Expr::UnOp { op: UnOp::Not, .. }),
                "should not swap with elseif present, got {:?}", condition);
            assert_eq!(elseif_clauses.len(), 1);
        }
        other => panic!("expected If, got {:?}", other),
    }
}
