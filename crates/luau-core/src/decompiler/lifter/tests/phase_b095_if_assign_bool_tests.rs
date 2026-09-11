//! Phase B0.95 tests: collapse `if cond then X = true else X = false end`
//! into `X = cond` (and variants).

use crate::ast::{BinOp, Expr, Stat, UnOp};
use crate::decompiler::lifter::post_passes::collapse_if_assign_bool;

fn name(s: &str) -> Expr { Expr::Name(s.to_string()) }

// ─── Pattern A: if/else with assign true / assign false ──────────────

#[test]
fn b095_if_assign_true_else_assign_false() {
    // if cond then x = true else x = false end → x = cond
    let mut stmts = vec![Stat::If {
        condition: name("cond"),
        then_body: vec![Stat::Assign {
            targets: vec![name("x")],
            values: vec![Expr::Bool(true)],
        }],
        elseif_clauses: vec![],
        else_body: Some(vec![Stat::Assign {
            targets: vec![name("x")],
            values: vec![Expr::Bool(false)],
        }]),
    }];
    collapse_if_assign_bool(&mut stmts);
    assert_eq!(stmts.len(), 1);
    match &stmts[0] {
        Stat::Assign { targets, values } if targets.len() == 1 && values.len() == 1 => {
            assert!(matches!(&targets[0], Expr::Name(n) if n == "x"));
            assert!(matches!(&values[0], Expr::Name(n) if n == "cond"),
                "should assign cond, got {:?}", values[0]);
        }
        other => panic!("expected Assign, got {:?}", other),
    }
}

#[test]
fn b095_if_assign_false_else_assign_true() {
    // if cond then x = false else x = true end → x = not cond
    let mut stmts = vec![Stat::If {
        condition: name("cond"),
        then_body: vec![Stat::Assign {
            targets: vec![name("x")],
            values: vec![Expr::Bool(false)],
        }],
        elseif_clauses: vec![],
        else_body: Some(vec![Stat::Assign {
            targets: vec![name("x")],
            values: vec![Expr::Bool(true)],
        }]),
    }];
    collapse_if_assign_bool(&mut stmts);
    assert_eq!(stmts.len(), 1);
    match &stmts[0] {
        Stat::Assign { targets, values } if targets.len() == 1 && values.len() == 1 => {
            assert!(matches!(&targets[0], Expr::Name(n) if n == "x"));
            assert!(matches!(&values[0], Expr::UnOp { op: UnOp::Not, .. }),
                "should assign not cond, got {:?}", values[0]);
        }
        other => panic!("expected Assign, got {:?}", other),
    }
}

// ─── Pattern B: if without else + following assign ────────────────────

#[test]
fn b095_if_assign_true_followed_by_assign_false() {
    // if cond then x = true end; x = false → x = cond
    let mut stmts = vec![
        Stat::If {
            condition: name("cond"),
            then_body: vec![Stat::Assign {
                targets: vec![name("x")],
                values: vec![Expr::Bool(true)],
            }],
            elseif_clauses: vec![],
            else_body: None,
        },
        Stat::Assign {
            targets: vec![name("x")],
            values: vec![Expr::Bool(false)],
        },
    ];
    collapse_if_assign_bool(&mut stmts);
    assert_eq!(stmts.len(), 1);
    match &stmts[0] {
        Stat::Assign { targets, values } if targets.len() == 1 && values.len() == 1 => {
            assert!(matches!(&targets[0], Expr::Name(n) if n == "x"));
            assert!(matches!(&values[0], Expr::Name(n) if n == "cond"),
                "should assign cond, got {:?}", values[0]);
        }
        other => panic!("expected Assign, got {:?}", other),
    }
}

#[test]
fn b095_if_assign_false_followed_by_assign_true() {
    // if cond then x = false end; x = true → x = not cond
    let mut stmts = vec![
        Stat::If {
            condition: name("cond"),
            then_body: vec![Stat::Assign {
                targets: vec![name("x")],
                values: vec![Expr::Bool(false)],
            }],
            elseif_clauses: vec![],
            else_body: None,
        },
        Stat::Assign {
            targets: vec![name("x")],
            values: vec![Expr::Bool(true)],
        },
    ];
    collapse_if_assign_bool(&mut stmts);
    assert_eq!(stmts.len(), 1);
    match &stmts[0] {
        Stat::Assign { targets, values } if targets.len() == 1 && values.len() == 1 => {
            assert!(matches!(&targets[0], Expr::Name(n) if n == "x"));
            assert!(matches!(&values[0], Expr::UnOp { op: UnOp::Not, .. }),
                "should assign not cond, got {:?}", values[0]);
        }
        other => panic!("expected Assign, got {:?}", other),
    }
}

// ─── Non-matching patterns (should NOT collapse) ──────────────────────

#[test]
fn b095_different_targets_preserved() {
    // if cond then x = true else y = false end — different targets
    let mut stmts = vec![Stat::If {
        condition: name("cond"),
        then_body: vec![Stat::Assign {
            targets: vec![name("x")],
            values: vec![Expr::Bool(true)],
        }],
        elseif_clauses: vec![],
        else_body: Some(vec![Stat::Assign {
            targets: vec![name("y")],
            values: vec![Expr::Bool(false)],
        }]),
    }];
    collapse_if_assign_bool(&mut stmts);
    assert!(matches!(&stmts[0], Stat::If { .. }),
        "different targets should not collapse");
}

#[test]
fn b095_same_bool_preserved() {
    // if cond then x = true else x = true end — same booleans
    let mut stmts = vec![Stat::If {
        condition: name("cond"),
        then_body: vec![Stat::Assign {
            targets: vec![name("x")],
            values: vec![Expr::Bool(true)],
        }],
        elseif_clauses: vec![],
        else_body: Some(vec![Stat::Assign {
            targets: vec![name("x")],
            values: vec![Expr::Bool(true)],
        }]),
    }];
    collapse_if_assign_bool(&mut stmts);
    assert!(matches!(&stmts[0], Stat::If { .. }),
        "same boolean values should not collapse");
}

#[test]
fn b095_with_complex_condition() {
    // if a == b then x = true else x = false end → x = a == b
    let cond = Expr::BinOp {
        left: Box::new(name("a")),
        op: BinOp::Eq,
        right: Box::new(name("b")),
    };
    let mut stmts = vec![Stat::If {
        condition: cond.clone(),
        then_body: vec![Stat::Assign {
            targets: vec![name("x")],
            values: vec![Expr::Bool(true)],
        }],
        elseif_clauses: vec![],
        else_body: Some(vec![Stat::Assign {
            targets: vec![name("x")],
            values: vec![Expr::Bool(false)],
        }]),
    }];
    collapse_if_assign_bool(&mut stmts);
    assert_eq!(stmts.len(), 1);
    match &stmts[0] {
        Stat::Assign { values, .. } if values.len() == 1 => {
            assert!(matches!(&values[0], Expr::BinOp { op: BinOp::Eq, .. }),
                "should assign a == b, got {:?}", values[0]);
        }
        other => panic!("expected Assign, got {:?}", other),
    }
}

#[test]
fn b095_with_elseif_preserved() {
    // if cond then x = true elseif cond2 then ... else x = false end
    let mut stmts = vec![Stat::If {
        condition: name("cond"),
        then_body: vec![Stat::Assign {
            targets: vec![name("x")],
            values: vec![Expr::Bool(true)],
        }],
        elseif_clauses: vec![
            (name("cond2"), vec![Stat::Assign {
                targets: vec![name("x")],
                values: vec![Expr::Bool(true)],
            }]),
        ],
        else_body: Some(vec![Stat::Assign {
            targets: vec![name("x")],
            values: vec![Expr::Bool(false)],
        }]),
    }];
    collapse_if_assign_bool(&mut stmts);
    assert!(matches!(&stmts[0], Stat::If { .. }),
        "if with elseif should not collapse");
}
