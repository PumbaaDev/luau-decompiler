//! Phase B0.97 — collapse if/else return to ternary return.
//!
//! Pattern A: `if cond then return a else return b end` → `return if cond then a else b`
//! Pattern B: `if cond then return a end; return b` → `return if cond then a else b`

use crate::ast::{BinOp, Expr, Stat};
use super::super::post_passes::collapse_if_return_ternary;

fn name(s: &str) -> Expr { Expr::Name(s.to_string()) }
fn num(n: f64) -> Expr { Expr::Number(n) }
fn str_(s: &str) -> Expr { Expr::String(s.to_string()) }

/// Pattern A: both branches return → ternary return
#[test]
fn b097_if_else_return_to_ternary() {
    let mut stmts = vec![
        Stat::If {
            condition: name("cond"),
            then_body: vec![Stat::Return { values: vec![name("a")] }],
            elseif_clauses: vec![],
            else_body: Some(vec![Stat::Return { values: vec![name("b")] }]),
        },
    ];
    collapse_if_return_ternary(&mut stmts);
    assert_eq!(stmts.len(), 1);
    match &stmts[0] {
        Stat::Return { values } => {
            assert_eq!(values.len(), 1);
            assert!(matches!(&values[0], Expr::Ternary { .. }));
        }
        other => panic!("Expected Return, got {:?}", other),
    }
}

/// Pattern B: if-return then fallthrough return → ternary return
#[test]
fn b097_if_return_fallthrough_to_ternary() {
    let mut stmts = vec![
        Stat::If {
            condition: name("cond"),
            then_body: vec![Stat::Return { values: vec![num(1.0)] }],
            elseif_clauses: vec![],
            else_body: None,
        },
        Stat::Return { values: vec![num(0.0)] },
    ];
    collapse_if_return_ternary(&mut stmts);
    assert_eq!(stmts.len(), 1);
    match &stmts[0] {
        Stat::Return { values } => {
            assert_eq!(values.len(), 1);
            assert!(matches!(&values[0], Expr::Ternary { .. }));
        }
        other => panic!("Expected Return, got {:?}", other),
    }
}

/// Multi-return should NOT be collapsed (ternary only produces one value).
#[test]
fn b097_multi_return_not_collapsed() {
    let mut stmts = vec![
        Stat::If {
            condition: name("cond"),
            then_body: vec![Stat::Return { values: vec![name("a"), name("b")] }],
            elseif_clauses: vec![],
            else_body: Some(vec![Stat::Return { values: vec![name("c"), name("d")] }]),
        },
    ];
    collapse_if_return_ternary(&mut stmts);
    // Should remain as if-else, not collapsed
    assert!(matches!(&stmts[0], Stat::If { .. }));
}

/// Elseif clauses should NOT be collapsed.
#[test]
fn b097_elseif_not_collapsed() {
    let mut stmts = vec![
        Stat::If {
            condition: name("a"),
            then_body: vec![Stat::Return { values: vec![num(1.0)] }],
            elseif_clauses: vec![(name("b"), vec![Stat::Return { values: vec![num(2.0)] }])],
            else_body: Some(vec![Stat::Return { values: vec![num(3.0)] }]),
        },
    ];
    collapse_if_return_ternary(&mut stmts);
    assert!(matches!(&stmts[0], Stat::If { .. }));
}

/// Recurses into function bodies.
#[test]
fn b097_recurse_into_function() {
    let inner = vec![
        Stat::If {
            condition: name("x"),
            then_body: vec![Stat::Return { values: vec![str_("yes")] }],
            elseif_clauses: vec![],
            else_body: Some(vec![Stat::Return { values: vec![str_("no")] }]),
        },
    ];
    let mut stmts = vec![
        Stat::LocalFunction {
            name: "test".to_string(),
            func: Expr::Function {
                params: vec![],
                is_vararg: false,
                body: inner,
            },
        },
    ];
    collapse_if_return_ternary(&mut stmts);
    if let Stat::LocalFunction { func, .. } = &stmts[0] {
        if let Expr::Function { body, .. } = func {
            assert_eq!(body.len(), 1);
            assert!(matches!(&body[0], Stat::Return { .. }));
        } else {
            panic!("Expected Function");
        }
    } else {
        panic!("Expected LocalFunction");
    }
}

/// Pattern B: non-matching next statement (not a Return) should not collapse.
#[test]
fn b097_non_return_next_stmt_unchanged() {
    let mut stmts = vec![
        Stat::If {
            condition: name("cond"),
            then_body: vec![Stat::Return { values: vec![num(1.0)] }],
            elseif_clauses: vec![],
            else_body: None,
        },
        Stat::ExprStat(Expr::Call {
            func: Box::new(name("print")),
            args: vec![str_("hello")],
        }),
    ];
    let original_len = stmts.len();
    collapse_if_return_ternary(&mut stmts);
    assert_eq!(stmts.len(), original_len);
    assert!(matches!(&stmts[0], Stat::If { .. }));
}

/// Pattern A with string values — verify correct ternary structure.
#[test]
fn b097_string_return_ternary_structure() {
    let mut stmts = vec![
        Stat::If {
            condition: Expr::BinOp {
                left: Box::new(name("x")),
                op: BinOp::GT,
                right: Box::new(num(0.0)),
            },
            then_body: vec![Stat::Return { values: vec![str_("positive")] }],
            elseif_clauses: vec![],
            else_body: Some(vec![Stat::Return { values: vec![str_("non-positive")] }]),
        },
    ];
    collapse_if_return_ternary(&mut stmts);
    if let Stat::Return { values } = &stmts[0] {
        if let Expr::Ternary { cond, then_expr, else_expr } = &values[0] {
            assert!(matches!(cond.as_ref(), Expr::BinOp { op: BinOp::GT, .. }));
            assert!(matches!(then_expr.as_ref(), Expr::String(s) if s == "positive"));
            assert!(matches!(else_expr.as_ref(), Expr::String(s) if s == "non-positive"));
        } else {
            panic!("Expected Ternary");
        }
    } else {
        panic!("Expected Return");
    }
}
