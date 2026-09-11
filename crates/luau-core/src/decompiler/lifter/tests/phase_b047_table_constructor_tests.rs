//! Extracted from lifter.rs as part of Phase B0.52P6 (part 3).
//! `super::super::` references the `lifter` module.

//! Phase B0.47 — exercise `reconstruct_table_constructors` directly
//! (the AST-only, lifter-independent pass).
//!
//! Positive cases (must fold):
//!   1. `local M = {}; M.a = 1; M.b = 2; M.c = 3` → `local M = {a=1, b=2, c=3}`
//!   2. Mixed string + integer keys → constructor with `[N] = v`
//!   3. String key that's a valid identifier coming via Index → Named
//!   4. Function-valued field
//!   5. Method-style `M.foo = function(self, x) ... end`
//!   6. Nested module-style table inside a function literal
//!   7. Assign-form seed (`M = {}` not `local M = {}`)
//!
//! Negative / edge cases (must NOT fold or fold partially):
//!   8. Reassignment between seed and field-assigns — only fold prefix
//!   9. Non-matching statement interrupts the run — only fold prefix
//!  10. Circular RHS (`M.a = M.b + 1`) — break the run there
//!  11. Empty body (`local v0 = {}; return v0`) — leaves seed untouched
//!  12. Nested `M.a = {}; M.a.b = 1` — folds outer, leaves nested intact
//!  13. Independent constructors back-to-back both fold
//!
//! Regression guards:
//!  14. Preserves B0.45A inline behavior (no regression in
//!      `inline_single_use_temps` interaction).
//!  15. Preserves B0.46A `convert_while_true_break_to_repeat` chain
//!      (the new pass is structurally orthogonal to repeat-until).

use super::super::{reconstruct_table_constructors, inline_single_use_temps,
    convert_while_true_break_to_repeat, is_valid_luau_identifier};
use crate::ast::{BinOp, Expr, Stat, TableField, UnOp};

fn name(s: &str) -> Expr { Expr::Name(s.to_string()) }
fn num(n: f64) -> Expr { Expr::Number(n) }
fn string(s: &str) -> Expr { Expr::String(s.to_string()) }
fn empty_table() -> Expr { Expr::Table { fields: vec![] } }

fn local_empty(n: &str) -> Stat {
    Stat::Local { names: vec![n.to_string()], values: vec![empty_table()] }
}

fn assign_empty(n: &str) -> Stat {
    Stat::Assign {
        targets: vec![Expr::Name(n.to_string())],
        values: vec![empty_table()],
    }
}

fn field_assign(target: &str, field: &str, value: Expr) -> Stat {
    Stat::Assign {
        targets: vec![Expr::Field {
            object: Box::new(Expr::Name(target.to_string())),
            field: field.to_string(),
        }],
        values: vec![value],
    }
}

fn index_assign(target: &str, key: Expr, value: Expr) -> Stat {
    Stat::Assign {
        targets: vec![Expr::Index {
            object: Box::new(Expr::Name(target.to_string())),
            key: Box::new(key),
        }],
        values: vec![value],
    }
}

fn ret_name(n: &str) -> Stat {
    Stat::Return { values: vec![Expr::Name(n.to_string())] }
}

fn make_function(params: Vec<&str>, body: Vec<Stat>) -> Expr {
    Expr::Function {
        params: params.iter().map(|s| s.to_string()).collect(),
        is_vararg: false,
        body,
    }
}

// ─── Positive 1: basic 3-field constructor ────────────────────────

#[test]
fn b047_basic_three_named_fields() {
    // local M = {}; M.a = 1; M.b = 2; M.c = 3; return M
    // → local M = {a = 1, b = 2, c = 3}; return M
    let mut stmts = vec![
        local_empty("M"),
        field_assign("M", "a", num(1.0)),
        field_assign("M", "b", num(2.0)),
        field_assign("M", "c", num(3.0)),
        ret_name("M"),
    ];
    reconstruct_table_constructors(&mut stmts);

    assert_eq!(stmts.len(), 2,
        "expected seed + return after fold; got {:?}", stmts);
    match &stmts[0] {
        Stat::Local { names, values } => {
            assert_eq!(names, &["M"]);
            assert_eq!(values.len(), 1);
            match &values[0] {
                Expr::Table { fields } => {
                    assert_eq!(fields.len(), 3, "expected 3 named fields; got {:?}", fields);
                    match &fields[0] {
                        TableField::Named(k, v) => {
                            assert_eq!(k, "a");
                            assert!(matches!(v, Expr::Number(n) if *n == 1.0));
                        }
                        other => panic!("expected Named(a, 1); got {:?}", other),
                    }
                    match &fields[1] {
                        TableField::Named(k, v) => {
                            assert_eq!(k, "b");
                            assert!(matches!(v, Expr::Number(n) if *n == 2.0));
                        }
                        other => panic!("expected Named(b, 2); got {:?}", other),
                    }
                    match &fields[2] {
                        TableField::Named(k, v) => {
                            assert_eq!(k, "c");
                            assert!(matches!(v, Expr::Number(n) if *n == 3.0));
                        }
                        other => panic!("expected Named(c, 3); got {:?}", other),
                    }
                }
                other => panic!("expected Table seed; got {:?}", other),
            }
        }
        other => panic!("expected Stat::Local seed; got {:?}", other),
    }
    assert!(matches!(&stmts[1], Stat::Return { .. }));
}

// ─── Positive 2: mixed string + integer keys ──────────────────────

#[test]
fn b047_mixed_named_and_indexed_fields() {
    // local M = {}; M.foo = 1; M[2] = "two"; M.bar = 3
    let mut stmts = vec![
        local_empty("M"),
        field_assign("M", "foo", num(1.0)),
        index_assign("M", num(2.0), string("two")),
        field_assign("M", "bar", num(3.0)),
    ];
    reconstruct_table_constructors(&mut stmts);

    assert_eq!(stmts.len(), 1, "all field assigns absorbed");
    match &stmts[0] {
        Stat::Local { values, .. } => {
            if let Expr::Table { fields } = &values[0] {
                assert_eq!(fields.len(), 3);
                assert!(matches!(&fields[0], TableField::Named(k, _) if k == "foo"));
                match &fields[1] {
                    TableField::Indexed(k, v) => {
                        assert!(matches!(k, Expr::Number(n) if *n == 2.0));
                        assert!(matches!(v, Expr::String(s) if s == "two"));
                    }
                    other => panic!("expected Indexed(2, \"two\"); got {:?}", other),
                }
                assert!(matches!(&fields[2], TableField::Named(k, _) if k == "bar"));
            } else {
                panic!("expected Table; got {:?}", values[0]);
            }
        }
        other => panic!("expected Local; got {:?}", other),
    }
}

// ─── Positive 3: string-keyed Index promoted to Named ─────────────

#[test]
fn b047_string_index_promoted_to_named() {
    // M["foo"] = 1 should become foo = 1 (since "foo" is a valid ident)
    // M["a-b"] = 1 should stay as ["a-b"] = 1 (not a valid ident)
    let mut stmts = vec![
        local_empty("M"),
        index_assign("M", string("foo"), num(1.0)),
        index_assign("M", string("a-b"), num(2.0)),
    ];
    reconstruct_table_constructors(&mut stmts);

    assert_eq!(stmts.len(), 1);
    if let Stat::Local { values, .. } = &stmts[0] {
        if let Expr::Table { fields } = &values[0] {
            assert_eq!(fields.len(), 2);
            // First should be promoted to Named
            assert!(matches!(&fields[0], TableField::Named(k, _) if k == "foo"),
                "expected Named promotion; got {:?}", fields[0]);
            // Second must stay Indexed
            match &fields[1] {
                TableField::Indexed(k, _) => {
                    assert!(matches!(k, Expr::String(s) if s == "a-b"));
                }
                other => panic!("expected Indexed(\"a-b\"); got {:?}", other),
            }
        } else {
            panic!("expected Table");
        }
    } else {
        panic!("expected Local");
    }
}

// ─── Positive 4: function-valued field ────────────────────────────

#[test]
fn b047_function_valued_field_folds() {
    // local M = {}; M.foo = function(x) return x end; M.bar = 1
    let body = vec![Stat::Return { values: vec![name("x")] }];
    let func = make_function(vec!["x"], body);
    let mut stmts = vec![
        local_empty("M"),
        field_assign("M", "foo", func),
        field_assign("M", "bar", num(1.0)),
    ];
    reconstruct_table_constructors(&mut stmts);

    assert_eq!(stmts.len(), 1);
    if let Stat::Local { values, .. } = &stmts[0] {
        if let Expr::Table { fields } = &values[0] {
            assert_eq!(fields.len(), 2);
            match &fields[0] {
                TableField::Named(k, v) => {
                    assert_eq!(k, "foo");
                    assert!(matches!(v, Expr::Function { .. }),
                        "function should fold as field value; got {:?}", v);
                }
                other => panic!("expected Named(foo, fn); got {:?}", other),
            }
        }
    }
}

// ─── Positive 5: method-style function field ──────────────────────

#[test]
fn b047_method_style_function_folds() {
    // local M = {}; M.greet = function(self, msg) ... end
    let body = vec![Stat::ExprStat(Expr::Call {
        func: Box::new(name("print")),
        args: vec![name("self"), name("msg")],
    })];
    let func = make_function(vec!["self", "msg"], body);
    let mut stmts = vec![
        local_empty("M"),
        field_assign("M", "greet", func),
    ];
    reconstruct_table_constructors(&mut stmts);

    assert_eq!(stmts.len(), 1);
    if let Stat::Local { values, .. } = &stmts[0] {
        if let Expr::Table { fields } = &values[0] {
            assert_eq!(fields.len(), 1);
            match &fields[0] {
                TableField::Named(k, Expr::Function { params, .. }) => {
                    assert_eq!(k, "greet");
                    assert_eq!(params, &["self".to_string(), "msg".to_string()]);
                }
                other => panic!("expected Named greet fn; got {:?}", other),
            }
        }
    }
}

// ─── Positive 6: nested constructor inside a function body ────────

#[test]
fn b047_nested_constructor_inside_closure() {
    // local outer = function()
    //     local inner = {}; inner.x = 1; return inner
    // end
    let inner_body = vec![
        local_empty("inner"),
        field_assign("inner", "x", num(1.0)),
        ret_name("inner"),
    ];
    let outer_fn = make_function(vec![], inner_body);
    let mut stmts = vec![
        Stat::Local {
            names: vec!["outer".to_string()],
            values: vec![outer_fn],
        },
    ];
    reconstruct_table_constructors(&mut stmts);

    // Outer stmt should be unchanged at the top level; we drilled into
    // the closure body and folded inside.
    assert_eq!(stmts.len(), 1);
    if let Stat::Local { values, .. } = &stmts[0] {
        if let Expr::Function { body, .. } = &values[0] {
            // body should now contain only seed + return (field assign absorbed)
            assert_eq!(body.len(), 2,
                "inner body should have folded inner constructor; got {:?}", body);
            if let Stat::Local { values: iv, .. } = &body[0] {
                if let Expr::Table { fields } = &iv[0] {
                    assert_eq!(fields.len(), 1);
                    assert!(matches!(&fields[0], TableField::Named(k, _) if k == "x"));
                } else {
                    panic!("expected nested Table");
                }
            } else {
                panic!("expected inner Local");
            }
        } else {
            panic!("expected outer Function");
        }
    }
}

// ─── Positive 7: assign-form seed (not local) ────────────────────

#[test]
fn b047_assign_form_seed_folds() {
    // M = {}; M.a = 1; M.b = 2
    let mut stmts = vec![
        assign_empty("M"),
        field_assign("M", "a", num(1.0)),
        field_assign("M", "b", num(2.0)),
    ];
    reconstruct_table_constructors(&mut stmts);

    assert_eq!(stmts.len(), 1);
    match &stmts[0] {
        Stat::Assign { targets, values } => {
            assert_eq!(targets.len(), 1);
            assert!(matches!(&targets[0], Expr::Name(n) if n == "M"));
            if let Expr::Table { fields } = &values[0] {
                assert_eq!(fields.len(), 2);
            } else {
                panic!("expected Table value");
            }
        }
        other => panic!("expected Assign seed; got {:?}", other),
    }
}

// ─── Negative 8: reassignment breaks the run ─────────────────────

#[test]
fn b047_reassignment_breaks_the_run() {
    // local M = {}; M.a = 1; M = somethingElse(); M.b = 2
    // → fold first part only; reassignment + post-reassignment field stay
    let reassign = Stat::Assign {
        targets: vec![Expr::Name("M".to_string())],
        values: vec![Expr::Call {
            func: Box::new(name("somethingElse")),
            args: vec![],
        }],
    };
    let mut stmts = vec![
        local_empty("M"),
        field_assign("M", "a", num(1.0)),
        reassign,
        field_assign("M", "b", num(2.0)),
    ];
    reconstruct_table_constructors(&mut stmts);

    // Expected: [local M={a=1}, M=somethingElse(), M.b=2]
    // The 2nd reassignment is a non-empty Table call → not a seed,
    // so M.b=2 is left as a post-init field assign.
    assert_eq!(stmts.len(), 3,
        "expected seed-with-a + reassign + M.b=2; got {:?}", stmts);
    if let Stat::Local { values, .. } = &stmts[0] {
        if let Expr::Table { fields } = &values[0] {
            assert_eq!(fields.len(), 1, "only M.a absorbed");
            assert!(matches!(&fields[0], TableField::Named(k, _) if k == "a"));
        }
    }
    assert!(matches!(&stmts[1], Stat::Assign { .. }), "reassignment preserved");
    assert!(matches!(&stmts[2], Stat::Assign { .. }), "post-reassign field preserved");
}

// ─── Negative 9: non-matching stmt interrupts the run ────────────

#[test]
fn b047_non_matching_stmt_interrupts() {
    // local M = {}; M.a = 1; print("hi"); M.b = 2
    // → fold only M.a, leave print and M.b
    let print_call = Stat::ExprStat(Expr::Call {
        func: Box::new(name("print")),
        args: vec![string("hi")],
    });
    let mut stmts = vec![
        local_empty("M"),
        field_assign("M", "a", num(1.0)),
        print_call,
        field_assign("M", "b", num(2.0)),
    ];
    reconstruct_table_constructors(&mut stmts);

    assert_eq!(stmts.len(), 3,
        "expected seed-with-a + print + M.b; got {:?}", stmts);
    if let Stat::Local { values, .. } = &stmts[0] {
        if let Expr::Table { fields } = &values[0] {
            assert_eq!(fields.len(), 1);
        }
    }
    assert!(matches!(&stmts[1], Stat::ExprStat(_)));
    // M.b = 2 is still an Assign with Field target
    match &stmts[2] {
        Stat::Assign { targets, .. } => {
            assert!(matches!(&targets[0], Expr::Field { field, .. } if field == "b"));
        }
        other => panic!("expected post-print field assign; got {:?}", other),
    }
}

// ─── Negative 10: circular RHS terminates the fold ───────────────

#[test]
fn b047_circular_rhs_terminates_fold() {
    // local M = {}; M.a = 1; M.b = M.a + 1; M.c = 2
    // → fold M.a only; M.b reads M, so we stop. M.c stays as post-init.
    let m_a_plus_one = Expr::BinOp {
        left: Box::new(Expr::Field {
            object: Box::new(name("M")),
            field: "a".to_string(),
        }),
        op: BinOp::Add,
        right: Box::new(num(1.0)),
    };
    let mut stmts = vec![
        local_empty("M"),
        field_assign("M", "a", num(1.0)),
        field_assign("M", "b", m_a_plus_one),
        field_assign("M", "c", num(2.0)),
    ];
    reconstruct_table_constructors(&mut stmts);

    // Expected: 3 stmts — seed{a=1}, M.b = M.a + 1, M.c = 2
    assert_eq!(stmts.len(), 3,
        "expected partial fold; got {:?}", stmts);
    if let Stat::Local { values, .. } = &stmts[0] {
        if let Expr::Table { fields } = &values[0] {
            assert_eq!(fields.len(), 1);
            assert!(matches!(&fields[0], TableField::Named(k, _) if k == "a"));
        }
    }
    // The two remaining stmts are the field assigns
    assert!(matches!(&stmts[1], Stat::Assign { .. }));
    assert!(matches!(&stmts[2], Stat::Assign { .. }));
}

// ─── Negative 11: empty body — seed alone, untouched ─────────────

#[test]
fn b047_empty_body_seed_unchanged() {
    // local v0 = {}; return v0
    // → unchanged (no field assigns to absorb)
    let mut stmts = vec![local_empty("v0"), ret_name("v0")];
    reconstruct_table_constructors(&mut stmts);

    assert_eq!(stmts.len(), 2);
    if let Stat::Local { values, .. } = &stmts[0] {
        if let Expr::Table { fields } = &values[0] {
            assert!(fields.is_empty(), "still empty Table");
        } else {
            panic!("expected Table");
        }
    }
    assert!(matches!(&stmts[1], Stat::Return { .. }));
}

// ─── Edge 12: nested table assign via subfield (M.a={}; M.a.b=1) ──

#[test]
fn b047_nested_subfield_assign_left_alone() {
    // local M = {}; M.a = {}; M.a.b = 1
    // → fold outer to `M = {a = {}}`, then leave `M.a.b = 1` because
    //   it targets M.a.b (Field with Field object), NOT M directly.
    let inner_assign = Stat::Assign {
        targets: vec![Expr::Field {
            object: Box::new(Expr::Field {
                object: Box::new(name("M")),
                field: "a".to_string(),
            }),
            field: "b".to_string(),
        }],
        values: vec![num(1.0)],
    };
    let mut stmts = vec![
        local_empty("M"),
        field_assign("M", "a", empty_table()),
        inner_assign,
    ];
    reconstruct_table_constructors(&mut stmts);

    // Outer seed + nested-subfield assign survive (2 stmts).
    assert_eq!(stmts.len(), 2,
        "expected seed-with-a + nested-subfield; got {:?}", stmts);
    if let Stat::Local { values, .. } = &stmts[0] {
        if let Expr::Table { fields } = &values[0] {
            assert_eq!(fields.len(), 1);
            match &fields[0] {
                TableField::Named(k, v) => {
                    assert_eq!(k, "a");
                    assert!(matches!(v, Expr::Table { fields } if fields.is_empty()));
                }
                other => panic!("expected Named(a, {{}}); got {:?}", other),
            }
        }
    }
    // M.a.b = 1 — Field-of-Field target, NOT a fold candidate.
    match &stmts[1] {
        Stat::Assign { targets, .. } => {
            match &targets[0] {
                Expr::Field { object, field } if field == "b" => {
                    assert!(matches!(object.as_ref(), Expr::Field { .. }));
                }
                other => panic!("expected Field-of-Field target; got {:?}", other),
            }
        }
        _ => panic!("expected Assign for M.a.b = 1"),
    }
}

// ─── Edge 13: two independent constructors back-to-back ──────────

#[test]
fn b047_two_independent_constructors_both_fold() {
    // local A = {}; A.x = 1; local B = {}; B.y = 2
    let mut stmts = vec![
        local_empty("A"),
        field_assign("A", "x", num(1.0)),
        local_empty("B"),
        field_assign("B", "y", num(2.0)),
    ];
    reconstruct_table_constructors(&mut stmts);

    assert_eq!(stmts.len(), 2, "both constructors collapsed; got {:?}", stmts);
    for (idx, expected_field) in [("A", "x"), ("B", "y")].iter().enumerate() {
        if let Stat::Local { names, values } = &stmts[idx] {
            assert_eq!(names[0], expected_field.0);
            if let Expr::Table { fields } = &values[0] {
                assert_eq!(fields.len(), 1);
                assert!(matches!(&fields[0], TableField::Named(k, _) if k == expected_field.1));
            } else {
                panic!("expected Table at idx {}", idx);
            }
        } else {
            panic!("expected Local at idx {}; got {:?}", idx, stmts[idx]);
        }
    }
}

// ─── Regression 14: chains gracefully into inline_single_use_temps ──

#[test]
fn b047_does_not_break_b045a_inline_pass() {
    // The classic single-use call temp inline still works after B0.47.
    // local v1 = someFunc(); return v1   →   return someFunc()
    let mut stmts = vec![
        Stat::Local {
            names: vec!["v1".to_string()],
            values: vec![Expr::Call {
                func: Box::new(name("someFunc")),
                args: vec![],
            }],
        },
        Stat::Return { values: vec![name("v1")] },
    ];
    // Run the full chain in production order.
    reconstruct_table_constructors(&mut stmts);
    inline_single_use_temps(&mut stmts);

    assert_eq!(stmts.len(), 1);
    match &stmts[0] {
        Stat::Return { values } => {
            assert!(matches!(&values[0], Expr::Call { .. }),
                "expected return Call; got {:?}", values[0]);
        }
        other => panic!("expected Return; got {:?}", other),
    }
}

// ─── Regression 15: chain compatible with B0.46A repeat-until ────

#[test]
fn b047_does_not_break_b046a_repeat_until() {
    // while true do x = 1; if cond then break end end → repeat ... until cond
    // The B0.47 pass should not interfere with the repeat-until folding.
    let if_break = Stat::If {
        condition: name("cond"),
        then_body: vec![Stat::Break],
        elseif_clauses: vec![],
        else_body: None,
    };
    let mut stmts = vec![Stat::While {
        condition: Expr::Bool(true),
        body: vec![
            Stat::Assign {
                targets: vec![Expr::Name("x".to_string())],
                values: vec![num(1.0)],
            },
            if_break,
        ],
    }];
    // Production order: B0.46A first, then B0.47 (which should leave the
    // repeat alone because it's not a constructor pattern).
    convert_while_true_break_to_repeat(&mut stmts);
    reconstruct_table_constructors(&mut stmts);

    assert_eq!(stmts.len(), 1);
    match &stmts[0] {
        Stat::Repeat { body, condition } => {
            assert_eq!(body.len(), 1);
            assert!(matches!(condition, Expr::Name(n) if n == "cond"));
        }
        other => panic!("expected Stat::Repeat; got {:?}", other),
    }
}

// ─── Helper sanity: is_valid_luau_identifier ────────────────────

#[test]
fn b047_is_valid_luau_identifier_classifies_correctly() {
    // Valid
    assert!(is_valid_luau_identifier("foo"));
    assert!(is_valid_luau_identifier("foo_bar"));
    assert!(is_valid_luau_identifier("_underscore"));
    assert!(is_valid_luau_identifier("Foo123"));

    // Invalid
    assert!(!is_valid_luau_identifier(""));
    assert!(!is_valid_luau_identifier("123foo"));     // starts with digit
    assert!(!is_valid_luau_identifier("foo-bar"));    // hyphen
    assert!(!is_valid_luau_identifier("foo bar"));    // space
    assert!(!is_valid_luau_identifier("foo.bar"));    // dot
    assert!(!is_valid_luau_identifier("$foo"));
}

// ─── Edge 16: additional negative — UnOp circular reference ─────

#[test]
fn b047_circular_rhs_via_unop_terminates_fold() {
    // local M = {}; M.a = #M (length-of M reads M)
    let m_len = Expr::UnOp {
        op: UnOp::Length,
        operand: Box::new(name("M")),
    };
    let mut stmts = vec![
        local_empty("M"),
        field_assign("M", "a", m_len),
    ];
    reconstruct_table_constructors(&mut stmts);

    // The single field assign references M, so the fold must skip it.
    assert_eq!(stmts.len(), 2, "circular ref blocks fold; got {:?}", stmts);
    if let Stat::Local { values, .. } = &stmts[0] {
        if let Expr::Table { fields } = &values[0] {
            assert!(fields.is_empty(), "no fields absorbed");
        }
    }
    assert!(matches!(&stmts[1], Stat::Assign { .. }));
}
