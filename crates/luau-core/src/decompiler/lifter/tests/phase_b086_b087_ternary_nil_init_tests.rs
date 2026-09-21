//! Phase B0.86 + B0.87 + B0.88 + B0.89 tests.
//!
//! B0.86: TER pattern now produces Expr::Ternary instead of `cond and a or b`.
//! B0.87: `local x = nil; if cond then x = a [else x = b] end` collapses
//!        into `local x = if cond then a else b`.
//! B0.88: Extends B0.87 to all literal inits (Bool/Number/String).
//! B0.89: `local x = nil; x = expr` merges into `local x = expr`.

use crate::ast::{Expr, Stat};
use super::super::post_passes::merge_dead_init_with_assignment;
use super::super::{
    collapse_nil_init_conditional,
    collapse_short_circuit_assignments,
};

fn name(s: &str) -> Expr { Expr::Name(s.to_string()) }
fn assign(target: &str, value: Expr) -> Stat {
    Stat::Assign { targets: vec![name(target)], values: vec![value] }
}
fn local_nil(n: &str) -> Stat {
    Stat::Local { names: vec![n.to_string()], values: vec![Expr::Nil] }
}
fn if_simple(cond: Expr, then_body: Vec<Stat>, else_body: Option<Vec<Stat>>) -> Stat {
    Stat::If { condition: cond, then_body, elseif_clauses: vec![], else_body }
}

// ─── B0.86: TER produces Ternary ────────────────────────────────────

#[test]
fn b086_ter_pattern_produces_ternary() {
    // if c then x = a else x = b end → x = Ternary(c, a, b)
    let mut stmts = vec![if_simple(
        name("c"),
        vec![assign("x", name("a"))],
        Some(vec![assign("x", name("b"))]),
    )];
    collapse_short_circuit_assignments(&mut stmts);
    match &stmts[0] {
        Stat::Assign { values, .. } => {
            assert!(matches!(&values[0], Expr::Ternary { .. }),
                "TER should produce Ternary, got {:?}", values[0]);
        }
        other => panic!("expected Assign, got {:?}", other),
    }
}

#[test]
fn b086_ter_with_falsy_then_value_is_safe() {
    // if c then x = false else x = b end
    // Old code would produce `c and false or b` which is WRONG (always b).
    // Ternary correctly handles this.
    let mut stmts = vec![if_simple(
        name("c"),
        vec![assign("x", Expr::Bool(false))],
        Some(vec![assign("x", name("b"))]),
    )];
    collapse_short_circuit_assignments(&mut stmts);
    if let Stat::Assign { values, .. } = &stmts[0] {
        if let Expr::Ternary { then_expr, .. } = &values[0] {
            assert!(matches!(then_expr.as_ref(), Expr::Bool(false)),
                "then_expr should be Bool(false)");
        } else { panic!("expected Ternary"); }
    } else { panic!("expected Assign"); }
}

#[test]
fn b086_ter_with_nil_then_value_is_safe() {
    // if c then x = nil else x = b end
    // Old code: `c and nil or b` → always b (nil is falsy). WRONG.
    // Ternary: `if c then nil else b` → correct.
    let mut stmts = vec![if_simple(
        name("c"),
        vec![assign("x", Expr::Nil)],
        Some(vec![assign("x", name("b"))]),
    )];
    collapse_short_circuit_assignments(&mut stmts);
    if let Stat::Assign { values, .. } = &stmts[0] {
        if let Expr::Ternary { then_expr, .. } = &values[0] {
            assert!(matches!(then_expr.as_ref(), Expr::Nil),
                "then_expr should be Nil");
        } else { panic!("expected Ternary"); }
    } else { panic!("expected Assign"); }
}

// ─── B0.87: nil-init + conditional → collapsed local ────────────────

#[test]
fn b087_nil_init_with_else_collapses_to_ternary() {
    // local x = nil; if c then x = a else x = b end → local x = Ternary(c, a, b)
    let mut stmts = vec![
        local_nil("x"),
        if_simple(
            name("c"),
            vec![assign("x", name("a"))],
            Some(vec![assign("x", name("b"))]),
        ),
    ];
    collapse_nil_init_conditional(&mut stmts);
    assert_eq!(stmts.len(), 1, "should merge into one statement");
    match &stmts[0] {
        Stat::Local { names, values } => {
            assert_eq!(names[0], "x");
            if let Expr::Ternary { cond, then_expr, else_expr } = &values[0] {
                assert!(matches!(cond.as_ref(), Expr::Name(n) if n == "c"));
                assert!(matches!(then_expr.as_ref(), Expr::Name(n) if n == "a"));
                assert!(matches!(else_expr.as_ref(), Expr::Name(n) if n == "b"));
            } else { panic!("expected Ternary, got {:?}", values[0]); }
        }
        other => panic!("expected Local, got {:?}", other),
    }
}

#[test]
fn b087_nil_init_no_else_collapses_with_nil_fallback() {
    // local x = nil; if c then x = a end → local x = Ternary(c, a, nil)
    let mut stmts = vec![
        local_nil("x"),
        if_simple(name("c"), vec![assign("x", name("a"))], None),
    ];
    collapse_nil_init_conditional(&mut stmts);
    assert_eq!(stmts.len(), 1, "should merge into one statement");
    if let Stat::Local { values, .. } = &stmts[0] {
        if let Expr::Ternary { else_expr, .. } = &values[0] {
            assert!(matches!(else_expr.as_ref(), Expr::Nil),
                "else_expr should be Nil for no-else pattern");
        } else { panic!("expected Ternary"); }
    } else { panic!("expected Local"); }
}

#[test]
fn b087_no_collapse_when_cond_references_local() {
    // local x = nil; if x then x = a end → KEEP (cond reads x)
    let mut stmts = vec![
        local_nil("x"),
        if_simple(name("x"), vec![assign("x", name("a"))], None),
    ];
    collapse_nil_init_conditional(&mut stmts);
    assert_eq!(stmts.len(), 2, "should NOT collapse when cond references the local");
}

#[test]
fn b087_no_collapse_when_value_references_local() {
    // local x = nil; if c then x = x + 1 end → KEEP (value reads x)
    let val = Expr::BinOp {
        left: Box::new(name("x")),
        op: crate::ast::BinOp::Add,
        right: Box::new(Expr::Number(1.0)),
    };
    let mut stmts = vec![
        local_nil("x"),
        if_simple(name("c"), vec![assign("x", val)], None),
    ];
    collapse_nil_init_conditional(&mut stmts);
    assert_eq!(stmts.len(), 2, "should NOT collapse when value references the local");
}

// ─── B0.88: literal-init + conditional → ternary ────────────────────

#[test]
fn b088_bool_init_no_else_collapses() {
    // local x = true; if c then x = false end → local x = Ternary(c, false, true)
    let mut stmts = vec![
        Stat::Local { names: vec!["x".to_string()], values: vec![Expr::Bool(true)] },
        if_simple(name("c"), vec![assign("x", Expr::Bool(false))], None),
    ];
    collapse_nil_init_conditional(&mut stmts);
    assert_eq!(stmts.len(), 1, "should collapse bool-init pattern");
    if let Stat::Local { values, .. } = &stmts[0] {
        if let Expr::Ternary { then_expr, else_expr, .. } = &values[0] {
            assert!(matches!(then_expr.as_ref(), Expr::Bool(false)));
            assert!(matches!(else_expr.as_ref(), Expr::Bool(true)));
        } else { panic!("expected Ternary, got {:?}", values[0]); }
    } else { panic!("expected Local"); }
}

#[test]
fn b088_number_init_no_else_collapses() {
    // local x = 0; if c then x = 1 end → local x = Ternary(c, 1, 0)
    let mut stmts = vec![
        Stat::Local { names: vec!["x".to_string()], values: vec![Expr::Number(0.0)] },
        if_simple(name("c"), vec![assign("x", Expr::Number(1.0))], None),
    ];
    collapse_nil_init_conditional(&mut stmts);
    assert_eq!(stmts.len(), 1, "should collapse number-init pattern");
    if let Stat::Local { values, .. } = &stmts[0] {
        if let Expr::Ternary { else_expr, .. } = &values[0] {
            assert!(matches!(else_expr.as_ref(), Expr::Number(n) if *n == 0.0));
        } else { panic!("expected Ternary"); }
    } else { panic!("expected Local"); }
}

#[test]
fn b088_string_init_no_else_collapses() {
    // local x = "default"; if c then x = "other" end → Ternary
    let mut stmts = vec![
        Stat::Local {
            names: vec!["x".to_string()],
            values: vec![Expr::String("default".to_string())],
        },
        if_simple(name("c"), vec![assign("x", Expr::String("other".to_string()))], None),
    ];
    collapse_nil_init_conditional(&mut stmts);
    assert_eq!(stmts.len(), 1, "should collapse string-init pattern");
}

#[test]
fn b088_non_literal_init_no_else_keeps() {
    // local x = foo(); if c then x = a end → KEEP (init is a Call, not pure)
    let call_init = Expr::Call { func: Box::new(name("foo")), args: vec![] };
    let mut stmts = vec![
        Stat::Local { names: vec!["x".to_string()], values: vec![call_init] },
        if_simple(name("c"), vec![assign("x", name("a"))], None),
    ];
    collapse_nil_init_conditional(&mut stmts);
    assert_eq!(stmts.len(), 2, "should NOT collapse when init is a call (non-pure)");
}

#[test]
fn b088_any_init_with_both_branches_collapses() {
    // Pattern A: local x = foo(); if c then x = a else x = b end
    // Init is dead (both branches assign), so ANY init works.
    let call_init = Expr::Call { func: Box::new(name("foo")), args: vec![] };
    let mut stmts = vec![
        Stat::Local { names: vec!["x".to_string()], values: vec![call_init] },
        if_simple(
            name("c"),
            vec![assign("x", name("a"))],
            Some(vec![assign("x", name("b"))]),
        ),
    ];
    collapse_nil_init_conditional(&mut stmts);
    assert_eq!(stmts.len(), 1, "should collapse with both-branch assign even with non-pure init");
    if let Stat::Local { values, .. } = &stmts[0] {
        if let Expr::Ternary { cond, then_expr, else_expr } = &values[0] {
            assert!(matches!(cond.as_ref(), Expr::Name(n) if n == "c"));
            assert!(matches!(then_expr.as_ref(), Expr::Name(n) if n == "a"));
            assert!(matches!(else_expr.as_ref(), Expr::Name(n) if n == "b"));
        } else { panic!("expected Ternary"); }
    } else { panic!("expected Local"); }
}

#[test]
fn b087_no_collapse_with_elseif() {
    // local x = nil; if c then x = a elseif d then x = b end → KEEP
    let stmt = Stat::If {
        condition: name("c"),
        then_body: vec![assign("x", name("a"))],
        elseif_clauses: vec![(name("d"), vec![assign("x", name("b"))])],
        else_body: None,
    };
    let mut stmts = vec![local_nil("x"), stmt];
    collapse_nil_init_conditional(&mut stmts);
    assert_eq!(stmts.len(), 2, "should NOT collapse with elseif");
}

#[test]
fn b087_no_collapse_when_then_target_differs() {
    // local x = nil; if c then y = a end → KEEP (target is y, not x)
    let mut stmts = vec![
        local_nil("x"),
        if_simple(name("c"), vec![assign("y", name("a"))], None),
    ];
    collapse_nil_init_conditional(&mut stmts);
    assert_eq!(stmts.len(), 2, "should NOT collapse when target doesn't match local");
}

#[test]
fn b087_recurses_into_nested_blocks() {
    // while true do local x = nil; if c then x = a end; end
    let inner = vec![
        local_nil("x"),
        if_simple(name("c"), vec![assign("x", name("a"))], None),
    ];
    let mut stmts = vec![Stat::While { condition: Expr::Bool(true), body: inner }];
    collapse_nil_init_conditional(&mut stmts);
    if let Stat::While { body, .. } = &stmts[0] {
        assert_eq!(body.len(), 1, "inner should collapse to one statement");
        assert!(matches!(&body[0], Stat::Local { values, .. } if matches!(&values[0], Expr::Ternary { .. })));
    } else { panic!("expected While"); }
}

// ─── B0.89: dead-init merge ─────────────────────────────────────────

#[test]
fn b089_nil_init_then_assign_merges() {
    // local x = nil; x = foo() → local x = foo()
    let call = Expr::Call { func: Box::new(name("foo")), args: vec![] };
    let mut stmts = vec![
        local_nil("x"),
        assign("x", call.clone()),
    ];
    merge_dead_init_with_assignment(&mut stmts);
    assert_eq!(stmts.len(), 1, "should merge into one statement");
    if let Stat::Local { names, values } = &stmts[0] {
        assert_eq!(names[0], "x");
        assert!(matches!(&values[0], Expr::Call { .. }), "value should be the call");
    } else { panic!("expected Local"); }
}

#[test]
fn b089_literal_init_then_assign_merges() {
    // local x = 0; x = a → local x = a
    let mut stmts = vec![
        Stat::Local { names: vec!["x".to_string()], values: vec![Expr::Number(0.0)] },
        assign("x", name("a")),
    ];
    merge_dead_init_with_assignment(&mut stmts);
    assert_eq!(stmts.len(), 1, "should merge number-init + assign");
    if let Stat::Local { values, .. } = &stmts[0] {
        assert!(matches!(&values[0], Expr::Name(n) if n == "a"));
    } else { panic!("expected Local"); }
}

#[test]
fn b089_no_merge_when_rhs_references_local() {
    // local x = nil; x = x + 1 → KEEP (RHS reads x)
    let add = Expr::BinOp {
        left: Box::new(name("x")),
        op: crate::ast::BinOp::Add,
        right: Box::new(Expr::Number(1.0)),
    };
    let mut stmts = vec![local_nil("x"), assign("x", add)];
    merge_dead_init_with_assignment(&mut stmts);
    assert_eq!(stmts.len(), 2, "should NOT merge when RHS references x");
}

#[test]
fn b089_no_merge_when_target_differs() {
    // local x = nil; y = a → KEEP (target is y, not x)
    let mut stmts = vec![local_nil("x"), assign("y", name("a"))];
    merge_dead_init_with_assignment(&mut stmts);
    assert_eq!(stmts.len(), 2, "should NOT merge when target doesn't match");
}

#[test]
fn b089_no_merge_when_next_is_not_assign() {
    // local x = nil; if ... → KEEP (next is If, not Assign)
    let mut stmts = vec![
        local_nil("x"),
        if_simple(name("c"), vec![assign("x", name("a"))], None),
    ];
    merge_dead_init_with_assignment(&mut stmts);
    assert_eq!(stmts.len(), 2, "should NOT merge when next is If (handled by B0.87)");
}

#[test]
fn b089_no_merge_when_init_is_non_literal() {
    // local x = foo(); x = bar() → KEEP (init is a call, not pure)
    let call1 = Expr::Call { func: Box::new(name("foo")), args: vec![] };
    let call2 = Expr::Call { func: Box::new(name("bar")), args: vec![] };
    let mut stmts = vec![
        Stat::Local { names: vec!["x".to_string()], values: vec![call1] },
        assign("x", call2),
    ];
    merge_dead_init_with_assignment(&mut stmts);
    assert_eq!(stmts.len(), 2, "should NOT merge when init is a call");
}

#[test]
fn b089_recurses_into_nested_blocks() {
    // for i = 1, 10 do local x = nil; x = a end
    let inner = vec![local_nil("x"), assign("x", name("a"))];
    let mut stmts = vec![Stat::NumericFor {
        var: "i".to_string(),
        start: Expr::Number(1.0),
        stop: Expr::Number(10.0),
        step: None,
        body: inner,
    }];
    merge_dead_init_with_assignment(&mut stmts);
    if let Stat::NumericFor { body, .. } = &stmts[0] {
        assert_eq!(body.len(), 1, "inner should merge");
    } else { panic!("expected NumericFor"); }
}
