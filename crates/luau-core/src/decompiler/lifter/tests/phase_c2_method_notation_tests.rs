//! Phase C2 pass #5 — method-notation conversion.
//!
//! Tests for `convert_dot_to_method_function`: rewrite
//! `T.m = function(self, ...)` as `function T:m(...)` when the body
//! uses `self.x` or `self:y()` at least twice.

use crate::ast::{Expr, Stat};
use crate::decompiler::lifter::post_passes::convert_dot_to_method_function;

fn name(s: &str) -> Expr {
    Expr::Name(s.to_string())
}

fn field(obj: Expr, f: &str) -> Expr {
    Expr::Field {
        object: Box::new(obj),
        field: f.to_string(),
    }
}

/// Basic conversion: `T.m = function(self, a) self.x; self.y; self.z end`
/// → `function T:m(a) ... end` (3 self-receiver uses ≥ 2, converts).
#[test]
fn c2_basic_conversion_with_three_self_uses() {
    // Body: three field reads of `self.x`, `self.y`, `self.z`.
    let body = vec![
        Stat::ExprStat(field(name("self"), "x")),
        Stat::ExprStat(field(name("self"), "y")),
        Stat::ExprStat(field(name("self"), "z")),
    ];
    let func = Expr::Function {
        params: vec!["self".to_string(), "a".to_string()],
        is_vararg: false,
        body,
    };
    let mut stmts = vec![Stat::Assign {
        targets: vec![field(name("T"), "m")],
        values: vec![func],
    }];

    convert_dot_to_method_function(&mut stmts);

    assert_eq!(stmts.len(), 1);
    match &stmts[0] {
        Stat::MethodFunction {
            receiver,
            method,
            is_method,
            func,
        } => {
            assert!(matches!(receiver, Expr::Name(n) if n == "T"));
            assert_eq!(method, "m");
            assert!(*is_method, "expected is_method=true");
            // `self` should remain in params[0]; the emitter strips it.
            match func {
                Expr::Function { params, .. } => {
                    assert_eq!(params.len(), 2);
                    assert_eq!(params[0], "self");
                    assert_eq!(params[1], "a");
                }
                other => panic!("expected Function, got {:?}", other),
            }
        }
        other => panic!("expected MethodFunction, got {:?}", other),
    }
}

/// Function has `self` as first param but NO `self.x` / `self:y()` uses in
/// the body — does NOT convert (user may still need `self` for other uses).
#[test]
fn c2_zero_self_uses_does_not_convert() {
    // Body has no self references.
    let body = vec![Stat::Return {
        values: vec![Expr::Number(1.0)],
    }];
    let func = Expr::Function {
        params: vec!["self".to_string()],
        is_vararg: false,
        body,
    };
    let mut stmts = vec![Stat::Assign {
        targets: vec![field(name("T"), "m")],
        values: vec![func],
    }];

    convert_dot_to_method_function(&mut stmts);

    assert_eq!(stmts.len(), 1);
    match &stmts[0] {
        Stat::Assign { targets, values } => {
            assert!(matches!(&targets[0], Expr::Field { .. }));
            match &values[0] {
                Expr::Function { params, .. } => {
                    assert_eq!(params.len(), 1);
                    assert_eq!(params[0], "self");
                }
                other => panic!("expected Function, got {:?}", other),
            }
        }
        other => panic!("expected unchanged Assign, got {:?}", other),
    }
}
