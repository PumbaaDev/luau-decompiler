//! Phase B0.93c tests: collapse `if cond then return true else return false end`
//! into `return cond` (and variants).

use crate::ast::{BinOp, Expr, Stat, UnOp};
use crate::decompiler::lifter::post_passes::collapse_if_return_bool;

fn name(s: &str) -> Expr { Expr::Name(s.to_string()) }

// ─── Pattern A: if/else with return true / return false ────────────

#[test]
fn b093c_if_return_true_else_return_false() {
    // if cond then return true else return false end → return cond
    let mut stmts = vec![Stat::If {
        condition: name("cond"),
        then_body: vec![Stat::Return { values: vec![Expr::Bool(true)] }],
        elseif_clauses: vec![],
        else_body: Some(vec![Stat::Return { values: vec![Expr::Bool(false)] }]),
    }];
    collapse_if_return_bool(&mut stmts);
    assert_eq!(stmts.len(), 1);
    match &stmts[0] {
        Stat::Return { values } if values.len() == 1 => {
            assert!(matches!(&values[0], Expr::Name(n) if n == "cond"),
                "should return cond, got {:?}", values[0]);
        }
        other => panic!("expected Return, got {:?}", other),
    }
}

#[test]
fn b093c_if_return_false_else_return_true() {
    // if cond then return false else return true end → return not cond
    let mut stmts = vec![Stat::If {
        condition: name("cond"),
        then_body: vec![Stat::Return { values: vec![Expr::Bool(false)] }],
        elseif_clauses: vec![],
        else_body: Some(vec![Stat::Return { values: vec![Expr::Bool(true)] }]),
    }];
    collapse_if_return_bool(&mut stmts);
    assert_eq!(stmts.len(), 1);
    match &stmts[0] {
        Stat::Return { values } if values.len() == 1 => {
            assert!(matches!(&values[0], Expr::UnOp { op: UnOp::Not, .. }),
                "should return not cond, got {:?}", values[0]);
        }
        other => panic!("expected Return, got {:?}", other),
    }
}

// ─── Pattern B: if without else + following return ─────────────────

#[test]
fn b093c_if_return_true_followed_by_return_false() {
    // if cond then return true end; return false → return cond
    let mut stmts = vec![
        Stat::If {
            condition: name("cond"),
            then_body: vec![Stat::Return { values: vec![Expr::Bool(true)] }],
            elseif_clauses: vec![],
            else_body: None,
        },
        Stat::Return { values: vec![Expr::Bool(false)] },
    ];
    collapse_if_return_bool(&mut stmts);
    assert_eq!(stmts.len(), 1);
    match &stmts[0] {
        Stat::Return { values } if values.len() == 1 => {
            assert!(matches!(&values[0], Expr::Name(n) if n == "cond"),
                "should return cond, got {:?}", values[0]);
        }
        other => panic!("expected Return, got {:?}", other),
    }
}

#[test]
fn b093c_if_return_false_followed_by_return_true() {
    // if cond then return false end; return true → return not cond
    let mut stmts = vec![
        Stat::If {
            condition: name("cond"),
            then_body: vec![Stat::Return { values: vec![Expr::Bool(false)] }],
            elseif_clauses: vec![],
            else_body: None,
        },
        Stat::Return { values: vec![Expr::Bool(true)] },
    ];
    collapse_if_return_bool(&mut stmts);
    assert_eq!(stmts.len(), 1);
    match &stmts[0] {
        Stat::Return { values } if values.len() == 1 => {
            assert!(matches!(&values[0], Expr::UnOp { op: UnOp::Not, .. }),
                "should return not cond, got {:?}", values[0]);
        }
        other => panic!("expected Return, got {:?}", other),
    }
}

// ─── Non-matching patterns (should NOT collapse) ───────────────────

#[test]
fn b093c_if_return_true_else_return_true_preserved() {
    // if cond then return true else return true end — both same, no collapse
    let mut stmts = vec![Stat::If {
        condition: name("cond"),
        then_body: vec![Stat::Return { values: vec![Expr::Bool(true)] }],
        elseif_clauses: vec![],
        else_body: Some(vec![Stat::Return { values: vec![Expr::Bool(true)] }]),
    }];
    collapse_if_return_bool(&mut stmts);
    assert!(matches!(&stmts[0], Stat::If { .. }),
        "identical bool branches should not collapse");
}

#[test]
fn b093c_if_return_nonbool_preserved() {
    // if cond then return 42 else return false end — not bool-only
    let mut stmts = vec![Stat::If {
        condition: name("cond"),
        then_body: vec![Stat::Return { values: vec![Expr::Number(42.0)] }],
        elseif_clauses: vec![],
        else_body: Some(vec![Stat::Return { values: vec![Expr::Bool(false)] }]),
    }];
    collapse_if_return_bool(&mut stmts);
    assert!(matches!(&stmts[0], Stat::If { .. }),
        "non-bool return values should not collapse");
}

#[test]
fn b093c_if_with_elseif_preserved() {
    // if cond then return true elseif cond2 then ... else return false end
    let mut stmts = vec![Stat::If {
        condition: name("cond"),
        then_body: vec![Stat::Return { values: vec![Expr::Bool(true)] }],
        elseif_clauses: vec![
            (name("cond2"), vec![Stat::Return { values: vec![Expr::Bool(true)] }]),
        ],
        else_body: Some(vec![Stat::Return { values: vec![Expr::Bool(false)] }]),
    }];
    collapse_if_return_bool(&mut stmts);
    assert!(matches!(&stmts[0], Stat::If { .. }),
        "if with elseif should not collapse");
}

#[test]
fn b093c_with_complex_condition() {
    // if a == b then return true else return false end → return a == b
    let cond = Expr::BinOp {
        left: Box::new(name("a")),
        op: BinOp::Eq,
        right: Box::new(name("b")),
    };
    let mut stmts = vec![Stat::If {
        condition: cond.clone(),
        then_body: vec![Stat::Return { values: vec![Expr::Bool(true)] }],
        elseif_clauses: vec![],
        else_body: Some(vec![Stat::Return { values: vec![Expr::Bool(false)] }]),
    }];
    collapse_if_return_bool(&mut stmts);
    assert_eq!(stmts.len(), 1);
    match &stmts[0] {
        Stat::Return { values } if values.len() == 1 => {
            assert!(matches!(&values[0], Expr::BinOp { op: BinOp::Eq, .. }),
                "should return a == b, got {:?}", values[0]);
        }
        other => panic!("expected Return, got {:?}", other),
    }
}
