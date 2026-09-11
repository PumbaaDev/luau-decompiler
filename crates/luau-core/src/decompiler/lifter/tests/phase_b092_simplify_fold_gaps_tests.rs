//! Phase B0.92 tests: simplify_expr + fold_expr gap fixes.
//!
//! - `simplify_expr` now recurses into Ternary sub-expressions
//! - `not <number>` → false, `not <string>` → false (truthy literals)
//! - `fold_expr`: identical-branch ternary `if c then X else X` → X
//! - `fold_expr`: `nil and X` → nil, `nil or X` → X

use crate::ast::{BinOp, Expr, Stat, UnOp};
use super::super::{simplify_expr, fold_constants_in_stmts};

fn name(s: &str) -> Expr { Expr::Name(s.to_string()) }

// ─── not <truthy-literal> → false ──────────────────────────────────

#[test]
fn b092_not_number_is_false() {
    // In Lua, all numbers are truthy. `not 5` → false
    let expr = Expr::UnOp { op: UnOp::Not, operand: Box::new(Expr::Number(5.0)) };
    let simplified = simplify_expr(&expr);
    assert!(matches!(simplified, Expr::Bool(false)),
        "not 5 should simplify to false, got {:?}", simplified);
}

#[test]
fn b092_not_zero_is_false() {
    // 0 is TRUTHY in Lua (unlike C/JS). `not 0` → false
    let expr = Expr::UnOp { op: UnOp::Not, operand: Box::new(Expr::Number(0.0)) };
    let simplified = simplify_expr(&expr);
    assert!(matches!(simplified, Expr::Bool(false)),
        "not 0 should simplify to false, got {:?}", simplified);
}

#[test]
fn b092_not_string_is_false() {
    // All strings (including "") are truthy in Lua. `not "hello"` → false
    let expr = Expr::UnOp { op: UnOp::Not, operand: Box::new(Expr::String("hello".into())) };
    let simplified = simplify_expr(&expr);
    assert!(matches!(simplified, Expr::Bool(false)),
        "not \"hello\" should simplify to false, got {:?}", simplified);
}

#[test]
fn b092_not_empty_string_is_false() {
    // Even empty string is truthy in Lua. `not ""` → false
    let expr = Expr::UnOp { op: UnOp::Not, operand: Box::new(Expr::String("".into())) };
    let simplified = simplify_expr(&expr);
    assert!(matches!(simplified, Expr::Bool(false)),
        "not \"\" should simplify to false, got {:?}", simplified);
}

// ─── simplify_expr recurses into Ternary ───────────────────────────

#[test]
fn b092_simplify_recurses_into_ternary_cond() {
    // if (not not x) then a else b  →  if x then a else b
    let expr = Expr::Ternary {
        cond: Box::new(Expr::UnOp {
            op: UnOp::Not,
            operand: Box::new(Expr::UnOp {
                op: UnOp::Not,
                operand: Box::new(name("x")),
            }),
        }),
        then_expr: Box::new(name("a")),
        else_expr: Box::new(name("b")),
    };
    let simplified = simplify_expr(&expr);
    if let Expr::Ternary { cond, .. } = &simplified {
        assert!(matches!(cond.as_ref(), Expr::Name(n) if n == "x"),
            "condition should simplify to Name(\"x\"), got {:?}", cond);
    } else {
        panic!("expected Ternary, got {:?}", simplified);
    }
}

#[test]
fn b092_simplify_ternary_constant_cond_true() {
    // if true then a else b  →  a
    let expr = Expr::Ternary {
        cond: Box::new(Expr::Bool(true)),
        then_expr: Box::new(name("a")),
        else_expr: Box::new(name("b")),
    };
    let simplified = simplify_expr(&expr);
    assert!(matches!(&simplified, Expr::Name(n) if n == "a"),
        "if true then a else b → a, got {:?}", simplified);
}

#[test]
fn b092_simplify_ternary_constant_cond_false() {
    // if false then a else b  →  b
    let expr = Expr::Ternary {
        cond: Box::new(Expr::Bool(false)),
        then_expr: Box::new(name("a")),
        else_expr: Box::new(name("b")),
    };
    let simplified = simplify_expr(&expr);
    assert!(matches!(&simplified, Expr::Name(n) if n == "b"),
        "if false then a else b → b, got {:?}", simplified);
}

#[test]
fn b092_simplify_ternary_identical_branches() {
    // if c then x else x  →  x
    let expr = Expr::Ternary {
        cond: Box::new(name("c")),
        then_expr: Box::new(name("x")),
        else_expr: Box::new(name("x")),
    };
    let simplified = simplify_expr(&expr);
    assert!(matches!(&simplified, Expr::Name(n) if n == "x"),
        "if c then x else x → x, got {:?}", simplified);
}

// ─── fold_expr: nil and/or ─────────────────────────────────────────

#[test]
fn b092_fold_nil_and_x() {
    // nil and X → nil
    let mut stmts = vec![Stat::Local {
        names: vec!["r".into()],
        values: vec![Expr::BinOp {
            left: Box::new(Expr::Nil),
            op: BinOp::And,
            right: Box::new(name("x")),
        }],
    }];
    fold_constants_in_stmts(&mut stmts);
    if let Stat::Local { values, .. } = &stmts[0] {
        assert!(matches!(&values[0], Expr::Nil),
            "nil and x should fold to nil, got {:?}", values[0]);
    } else {
        panic!("expected Local");
    }
}

#[test]
fn b092_fold_nil_or_x() {
    // nil or X → X
    let mut stmts = vec![Stat::Local {
        names: vec!["r".into()],
        values: vec![Expr::BinOp {
            left: Box::new(Expr::Nil),
            op: BinOp::Or,
            right: Box::new(name("x")),
        }],
    }];
    fold_constants_in_stmts(&mut stmts);
    if let Stat::Local { values, .. } = &stmts[0] {
        assert!(matches!(&values[0], Expr::Name(n) if n == "x"),
            "nil or x should fold to x, got {:?}", values[0]);
    } else {
        panic!("expected Local");
    }
}

// ─── fold_expr: identical-branch ternary ───────────────────────────

#[test]
fn b092_fold_ternary_identical_branches() {
    // if c then 42 else 42  →  42
    let mut stmts = vec![Stat::Local {
        names: vec!["r".into()],
        values: vec![Expr::Ternary {
            cond: Box::new(name("c")),
            then_expr: Box::new(Expr::Number(42.0)),
            else_expr: Box::new(Expr::Number(42.0)),
        }],
    }];
    fold_constants_in_stmts(&mut stmts);
    if let Stat::Local { values, .. } = &stmts[0] {
        assert!(matches!(&values[0], Expr::Number(n) if *n == 42.0),
            "identical branches should fold, got {:?}", values[0]);
    } else {
        panic!("expected Local");
    }
}

#[test]
fn b092_fold_ternary_different_branches_preserved() {
    // if c then a else b  →  stays as-is
    let mut stmts = vec![Stat::Local {
        names: vec!["r".into()],
        values: vec![Expr::Ternary {
            cond: Box::new(name("c")),
            then_expr: Box::new(name("a")),
            else_expr: Box::new(name("b")),
        }],
    }];
    fold_constants_in_stmts(&mut stmts);
    if let Stat::Local { values, .. } = &stmts[0] {
        assert!(matches!(&values[0], Expr::Ternary { .. }),
            "different branches should be preserved, got {:?}", values[0]);
    } else {
        panic!("expected Local");
    }
}

// ─── fold_expr: not <truthy-literal> ───────────────────────────────

#[test]
fn b092_fold_not_number() {
    let mut stmts = vec![Stat::Local {
        names: vec!["r".into()],
        values: vec![Expr::UnOp {
            op: UnOp::Not,
            operand: Box::new(Expr::Number(42.0)),
        }],
    }];
    fold_constants_in_stmts(&mut stmts);
    if let Stat::Local { values, .. } = &stmts[0] {
        assert!(matches!(&values[0], Expr::Bool(false)),
            "not 42 should fold to false, got {:?}", values[0]);
    } else {
        panic!("expected Local");
    }
}

#[test]
fn b092_fold_not_string() {
    let mut stmts = vec![Stat::Local {
        names: vec!["r".into()],
        values: vec![Expr::UnOp {
            op: UnOp::Not,
            operand: Box::new(Expr::String("hello".into())),
        }],
    }];
    fold_constants_in_stmts(&mut stmts);
    if let Stat::Local { values, .. } = &stmts[0] {
        assert!(matches!(&values[0], Expr::Bool(false)),
            "not \"hello\" should fold to false, got {:?}", values[0]);
    } else {
        panic!("expected Local");
    }
}

// ─── Phase B0.101b: Sub/Mul simplifications ───────────────────────

#[test]
fn b101b_sub_negation_to_add() {
    // x - (-y) → x + y
    let expr = Expr::BinOp {
        left: Box::new(name("x")),
        op: BinOp::Sub,
        right: Box::new(Expr::UnOp {
            op: UnOp::Negate,
            operand: Box::new(name("y")),
        }),
    };
    let simplified = simplify_expr(&expr);
    match &simplified {
        Expr::BinOp { op: BinOp::Add, left, right } => {
            assert!(matches!(left.as_ref(), Expr::Name(n) if n == "x"));
            assert!(matches!(right.as_ref(), Expr::Name(n) if n == "y"));
        }
        other => panic!("expected x + y, got {:?}", other),
    }
}

#[test]
fn b101b_mul_neg1_right_to_negate() {
    // x * -1 → -x
    let expr = Expr::BinOp {
        left: Box::new(name("x")),
        op: BinOp::Mul,
        right: Box::new(Expr::Number(-1.0)),
    };
    let simplified = simplify_expr(&expr);
    match &simplified {
        Expr::UnOp { op: UnOp::Negate, operand } => {
            assert!(matches!(operand.as_ref(), Expr::Name(n) if n == "x"));
        }
        other => panic!("expected -x, got {:?}", other),
    }
}

#[test]
fn b101b_mul_neg1_left_to_negate() {
    // -1 * x → -x
    let expr = Expr::BinOp {
        left: Box::new(Expr::Number(-1.0)),
        op: BinOp::Mul,
        right: Box::new(name("x")),
    };
    let simplified = simplify_expr(&expr);
    match &simplified {
        Expr::UnOp { op: UnOp::Negate, operand } => {
            assert!(matches!(operand.as_ref(), Expr::Name(n) if n == "x"));
        }
        other => panic!("expected -x, got {:?}", other),
    }
}
