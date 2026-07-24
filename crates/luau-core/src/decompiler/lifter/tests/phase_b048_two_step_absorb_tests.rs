//! Extracted from lifter.rs as part of Phase B0.52P6 (part 3).
//! `super::super::` references the `lifter` module.

//! Phase B0.48 — two-step function-literal absorb in table-constructor
//! reconstruction.
//!
//! Roblox bytecode emits module-style function fields in two steps:
//!
//!     local v0 = {}
//!     local arithmetic = function(...) ... end   -- step 1: bind closure
//!     v0.arithmetic = arithmetic                 -- step 2: SETTABLEKS
//!
//! We want the folded shape
//!
//!     local v0 = { arithmetic = function(...) ... end, ... }
//!
//! This module exercises the lookahead-absorb branch in
//! `reconstruct_table_constructors` added for B0.48.
//!
//! Positive cases (must fold):
//!   1. Function two-step — the archetypal Roblox module pattern.
//!   2. String two-step — `local S = "cfg"; M.name = S`
//!   3. Number two-step — `local N = 42; M.x = N`
//!   4. Mixed direct + two-step in one constructor run.
//!   5. Multiple two-step fields in a row.
//!   6. Two-step via Index with string key promoted to Named.
//!   7. Table literal (empty) two-step — `local T = {}; M.nested = T`
//!
//! Negative / edge cases (must NOT absorb):
//!   8. F read twice (in assign AND later stmt) — preserve reads.
//!   9. Intervening non-matching stmt between local-F and field-assign.
//!  10. Impure RHS: Call — skip two-step fold entirely.
//!  11. F read later outside constructor region — don't absorb.
//!
//! Regression guards:
//!  12. B0.47 direct-assign pattern still works.
//!  13. Method-style `function(self, ...)` absorbs as value.
//!  14. Works with assign-form seed (`M = {}`) too.
//!  15. Empty-table F propagates nested structure unchanged.

use std::collections::HashMap;
use super::super::{reconstruct_table_constructors, is_pure_two_step_value,
    two_step_field_absorb};
use crate::ast::{Expr, Stat, TableField};

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

fn local_single(n: &str, v: Expr) -> Stat {
    Stat::Local { names: vec![n.to_string()], values: vec![v] }
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

// ─── Positive 1: function two-step (Roblox archetypal) ───────────

#[test]
fn b048_function_two_step_absorbs() {
    // local v0 = {}
    // local arithmetic = function(a, b) return a + b end
    // v0.arithmetic = arithmetic
    // return v0
    let func = make_function(
        vec!["a", "b"],
        vec![Stat::Return { values: vec![name("a")] }],
    );
    let mut stmts = vec![
        local_empty("v0"),
        local_single("arithmetic", func),
        field_assign("v0", "arithmetic", name("arithmetic")),
        ret_name("v0"),
    ];
    reconstruct_table_constructors(&mut stmts);

    assert_eq!(stmts.len(), 2,
        "both local-F and field-assign must fold; got {:?}", stmts);
    if let Stat::Local { names, values } = &stmts[0] {
        assert_eq!(names, &["v0"]);
        if let Expr::Table { fields } = &values[0] {
            assert_eq!(fields.len(), 1);
            match &fields[0] {
                TableField::Named(k, Expr::Function { params, .. }) => {
                    assert_eq!(k, "arithmetic");
                    assert_eq!(params, &["a".to_string(), "b".to_string()]);
                }
                other => panic!("expected Named(arithmetic, fn); got {:?}", other),
            }
        } else { panic!("expected Table"); }
    } else { panic!("expected Local"); }
    assert!(matches!(&stmts[1], Stat::Return { .. }));
}

// ─── Positive 2: string two-step ─────────────────────────────────

#[test]
fn b048_string_two_step_absorbs() {
    // local M = {}; local S = "Config"; M.name = S
    let mut stmts = vec![
        local_empty("M"),
        local_single("S", string("Config")),
        field_assign("M", "name", name("S")),
    ];
    reconstruct_table_constructors(&mut stmts);

    assert_eq!(stmts.len(), 1);
    if let Stat::Local { values, .. } = &stmts[0] {
        if let Expr::Table { fields } = &values[0] {
            assert_eq!(fields.len(), 1);
            match &fields[0] {
                TableField::Named(k, Expr::String(v)) => {
                    assert_eq!(k, "name");
                    assert_eq!(v, "Config");
                }
                other => panic!("expected Named(name, \"Config\"); got {:?}", other),
            }
        }
    } else { panic!("expected Local"); }
}

// ─── Positive 3: number two-step ─────────────────────────────────

#[test]
fn b048_number_two_step_absorbs() {
    // local M = {}; local N = 42; M.x = N
    let mut stmts = vec![
        local_empty("M"),
        local_single("N", num(42.0)),
        field_assign("M", "x", name("N")),
    ];
    reconstruct_table_constructors(&mut stmts);

    assert_eq!(stmts.len(), 1);
    if let Stat::Local { values, .. } = &stmts[0] {
        if let Expr::Table { fields } = &values[0] {
            assert_eq!(fields.len(), 1);
            match &fields[0] {
                TableField::Named(k, Expr::Number(v)) => {
                    assert_eq!(k, "x");
                    assert_eq!(*v, 42.0);
                }
                other => panic!("expected Named(x, 42); got {:?}", other),
            }
        }
    }
}

// ─── Positive 4: mixed direct + two-step in one run ──────────────

#[test]
fn b048_mixed_direct_and_two_step() {
    // local M = {}
    // M.a = 1                             -- direct
    // local F = function() end            -- two-step start
    // M.b = F                             -- two-step end
    // M.c = 3                             -- direct
    let func = make_function(vec![], vec![]);
    let mut stmts = vec![
        local_empty("M"),
        field_assign("M", "a", num(1.0)),
        local_single("F", func),
        field_assign("M", "b", name("F")),
        field_assign("M", "c", num(3.0)),
    ];
    reconstruct_table_constructors(&mut stmts);

    assert_eq!(stmts.len(), 1,
        "all 4 assigns + 1 local-F should fold into seed; got {:?}", stmts);
    if let Stat::Local { values, .. } = &stmts[0] {
        if let Expr::Table { fields } = &values[0] {
            assert_eq!(fields.len(), 3);
            assert!(matches!(&fields[0], TableField::Named(k, _) if k == "a"));
            match &fields[1] {
                TableField::Named(k, Expr::Function { .. }) => assert_eq!(k, "b"),
                other => panic!("expected Named(b, fn); got {:?}", other),
            }
            assert!(matches!(&fields[2], TableField::Named(k, _) if k == "c"));
        }
    }
}

// ─── Positive 5: multiple two-steps in a row ─────────────────────

#[test]
fn b048_multiple_two_step_in_a_row() {
    // The real Roblox pattern: every method is a separate two-step.
    // local v0 = {}
    // local m1 = function() end
    // v0.m1 = m1
    // local m2 = function() end
    // v0.m2 = m2
    // return v0
    let f1 = make_function(vec![], vec![]);
    let f2 = make_function(vec![], vec![]);
    let mut stmts = vec![
        local_empty("v0"),
        local_single("m1", f1),
        field_assign("v0", "m1", name("m1")),
        local_single("m2", f2),
        field_assign("v0", "m2", name("m2")),
        ret_name("v0"),
    ];
    reconstruct_table_constructors(&mut stmts);

    assert_eq!(stmts.len(), 2);
    if let Stat::Local { values, .. } = &stmts[0] {
        if let Expr::Table { fields } = &values[0] {
            assert_eq!(fields.len(), 2);
            assert!(matches!(&fields[0],
                TableField::Named(k, Expr::Function { .. }) if k == "m1"));
            assert!(matches!(&fields[1],
                TableField::Named(k, Expr::Function { .. }) if k == "m2"));
        }
    }
    assert!(matches!(&stmts[1], Stat::Return { .. }));
}

// ─── Positive 6: two-step via Index[string] promoted to Named ───

#[test]
fn b048_two_step_index_string_promoted_to_named() {
    // local M = {}; local F = function() end; M["fn"] = F
    let f = make_function(vec![], vec![]);
    let mut stmts = vec![
        local_empty("M"),
        local_single("F", f),
        index_assign("M", string("fn"), name("F")),
    ];
    reconstruct_table_constructors(&mut stmts);

    assert_eq!(stmts.len(), 1);
    if let Stat::Local { values, .. } = &stmts[0] {
        if let Expr::Table { fields } = &values[0] {
            assert_eq!(fields.len(), 1);
            // "fn" is a valid identifier, so promoted to Named form.
            assert!(matches!(&fields[0],
                TableField::Named(k, Expr::Function { .. }) if k == "fn"));
        }
    }
}

// ─── Positive 7: empty-table two-step (nested table literal) ─────

#[test]
fn b048_empty_table_two_step() {
    // local M = {}; local sub = {}; M.nested = sub
    let mut stmts = vec![
        local_empty("M"),
        local_single("sub", empty_table()),
        field_assign("M", "nested", name("sub")),
    ];
    reconstruct_table_constructors(&mut stmts);

    assert_eq!(stmts.len(), 1);
    if let Stat::Local { values, .. } = &stmts[0] {
        if let Expr::Table { fields } = &values[0] {
            assert_eq!(fields.len(), 1);
            match &fields[0] {
                TableField::Named(k, Expr::Table { fields: nested }) => {
                    assert_eq!(k, "nested");
                    assert!(nested.is_empty(), "nested table preserved as-is");
                }
                other => panic!("expected Named(nested, {{}}); got {:?}", other),
            }
        }
    }
}

// ─── Negative 8: F read twice — don't absorb ─────────────────────

#[test]
fn b048_f_read_twice_blocks_absorb() {
    // local M = {}
    // local F = function() end
    // M.a = F         -- read 1
    // M.b = F         -- read 2 → don't absorb, F must stay visible
    let f = make_function(vec![], vec![]);
    let mut stmts = vec![
        local_empty("M"),
        local_single("F", f),
        field_assign("M", "a", name("F")),
        field_assign("M", "b", name("F")),
    ];
    reconstruct_table_constructors(&mut stmts);

    // The two-step fold for `local F; M.a=F` cannot fire because F is
    // read twice. The walk should break at the local_single and NOT
    // absorb anything beyond that. Expected: seed (empty) + local F +
    // M.a=F + M.b=F = 4 stmts.
    assert_eq!(stmts.len(), 4,
        "F read twice must block two-step fold; got {:?}", stmts);
    if let Stat::Local { values, .. } = &stmts[0] {
        if let Expr::Table { fields } = &values[0] {
            assert!(fields.is_empty());
        }
    }
    assert!(matches!(&stmts[1], Stat::Local { names, .. } if names[0] == "F"));
    assert!(matches!(&stmts[2], Stat::Assign { .. }));
    assert!(matches!(&stmts[3], Stat::Assign { .. }));
}

// ─── Negative 9: intervening non-matching stmt blocks absorb ────

#[test]
fn b048_intervening_stmt_blocks_absorb() {
    // local M = {}
    // local F = function() end
    // print("hi")        -- intervening stmt breaks pair
    // M.a = F
    let f = make_function(vec![], vec![]);
    let print_call = Stat::ExprStat(Expr::Call {
        func: Box::new(name("print")),
        args: vec![string("hi")],
    });
    let mut stmts = vec![
        local_empty("M"),
        local_single("F", f),
        print_call,
        field_assign("M", "a", name("F")),
    ];
    reconstruct_table_constructors(&mut stmts);

    // The run breaks at the local-F (no matching field-assign follows
    // immediately). All four stmts survive.
    assert_eq!(stmts.len(), 4,
        "intervening stmt must break run; got {:?}", stmts);
    if let Stat::Local { values, .. } = &stmts[0] {
        if let Expr::Table { fields } = &values[0] {
            assert!(fields.is_empty(), "seed stays empty");
        }
    }
}

// ─── Negative 10: impure RHS (Call) — don't absorb ──────────────

#[test]
fn b048_impure_call_rhs_blocks_absorb() {
    // local M = {}
    // local F = loadModule()     -- CALL is impure, skip
    // M.a = F
    let call = Expr::Call {
        func: Box::new(name("loadModule")),
        args: vec![],
    };
    let mut stmts = vec![
        local_empty("M"),
        local_single("F", call),
        field_assign("M", "a", name("F")),
    ];
    reconstruct_table_constructors(&mut stmts);

    // Two-step must not fire — Call is impure.
    assert_eq!(stmts.len(), 3,
        "impure Call RHS must block two-step fold; got {:?}", stmts);
    if let Stat::Local { values, .. } = &stmts[0] {
        if let Expr::Table { fields } = &values[0] {
            assert!(fields.is_empty());
        }
    }
    assert!(matches!(&stmts[1], Stat::Local { names, .. } if names[0] == "F"));
}

// ─── Negative 11: F read after constructor region — don't absorb ─

#[test]
fn b048_f_read_outside_constructor_blocks_absorb() {
    // local M = {}
    // local F = function() end
    // M.a = F                    -- read 1
    // return F                   -- read 2, outside — block absorb
    let f = make_function(vec![], vec![]);
    let mut stmts = vec![
        local_empty("M"),
        local_single("F", f),
        field_assign("M", "a", name("F")),
        Stat::Return { values: vec![name("F")] },
    ];
    reconstruct_table_constructors(&mut stmts);

    // F is read AFTER the field-assign (in the return), so the absorb
    // must bail — post_reads=1 > 0.
    assert_eq!(stmts.len(), 4,
        "F read after constructor region blocks two-step fold; got {:?}",
        stmts);
    if let Stat::Local { values, .. } = &stmts[0] {
        if let Expr::Table { fields } = &values[0] {
            assert!(fields.is_empty());
        }
    }
}

// ─── Regression 12: direct-only still works ──────────────────────

#[test]
fn b048_direct_only_regression() {
    // Pure B0.47 pattern — must still absorb cleanly with B0.48 in place.
    let mut stmts = vec![
        local_empty("M"),
        field_assign("M", "a", num(1.0)),
        field_assign("M", "b", num(2.0)),
    ];
    reconstruct_table_constructors(&mut stmts);

    assert_eq!(stmts.len(), 1);
    if let Stat::Local { values, .. } = &stmts[0] {
        if let Expr::Table { fields } = &values[0] {
            assert_eq!(fields.len(), 2);
        }
    }
}

// ─── Regression 13: method-style function(self,...) absorbs ──────

#[test]
fn b048_method_style_function_two_step() {
    // local M = {}
    // local greet = function(self, msg) print(self, msg) end
    // M.greet = greet
    let body = vec![Stat::ExprStat(Expr::Call {
        func: Box::new(name("print")),
        args: vec![name("self"), name("msg")],
    })];
    let func = make_function(vec!["self", "msg"], body);
    let mut stmts = vec![
        local_empty("M"),
        local_single("greet", func),
        field_assign("M", "greet", name("greet")),
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
                other => panic!("expected Named(greet, fn(self,msg)); got {:?}", other),
            }
        }
    }
}

// ─── Regression 14: assign-form seed + two-step combines ─────────

#[test]
fn b048_assign_form_seed_plus_two_step() {
    // M = {}
    // local F = function() end
    // M.foo = F
    let f = make_function(vec![], vec![]);
    let mut stmts = vec![
        assign_empty("M"),
        local_single("F", f),
        field_assign("M", "foo", name("F")),
    ];
    reconstruct_table_constructors(&mut stmts);

    assert_eq!(stmts.len(), 1);
    match &stmts[0] {
        Stat::Assign { values, .. } => {
            if let Expr::Table { fields } = &values[0] {
                assert_eq!(fields.len(), 1);
                assert!(matches!(&fields[0],
                    TableField::Named(k, Expr::Function { .. }) if k == "foo"));
            } else { panic!("expected Table"); }
        }
        other => panic!("expected Assign; got {:?}", other),
    }
}

// ─── Regression 15: bool / nil pure values also fold ─────────────

#[test]
fn b048_bool_and_nil_two_step() {
    // local M = {}
    // local B = true; M.flag = B
    // local N = nil;  M.empty = N
    let mut stmts = vec![
        local_empty("M"),
        local_single("B", Expr::Bool(true)),
        field_assign("M", "flag", name("B")),
        local_single("N", Expr::Nil),
        field_assign("M", "empty", name("N")),
    ];
    reconstruct_table_constructors(&mut stmts);

    assert_eq!(stmts.len(), 1);
    if let Stat::Local { values, .. } = &stmts[0] {
        if let Expr::Table { fields } = &values[0] {
            assert_eq!(fields.len(), 2);
            assert!(matches!(&fields[0],
                TableField::Named(k, Expr::Bool(true)) if k == "flag"));
            assert!(matches!(&fields[1],
                TableField::Named(k, Expr::Nil) if k == "empty"));
        }
    }
}

// ─── Helper sanity: is_pure_two_step_value classifies correctly ──

#[test]
fn b048_is_pure_two_step_value_classifies_correctly() {
    // Pure (fold-able as two-step RHS)
    assert!(is_pure_two_step_value(&Expr::Nil));
    assert!(is_pure_two_step_value(&Expr::Bool(true)));
    assert!(is_pure_two_step_value(&Expr::Number(3.14)));
    assert!(is_pure_two_step_value(&Expr::String("x".to_string())));
    assert!(is_pure_two_step_value(&Expr::Vector(0.0, 0.0, 0.0)));
    assert!(is_pure_two_step_value(&Expr::Table { fields: vec![] }));
    assert!(is_pure_two_step_value(&make_function(vec![], vec![])));

    // Impure (must NOT be folded via two-step)
    assert!(!is_pure_two_step_value(&name("x")));
    assert!(!is_pure_two_step_value(&Expr::Call {
        func: Box::new(name("f")), args: vec![],
    }));
    assert!(!is_pure_two_step_value(&Expr::MethodCall {
        object: Box::new(name("o")), method: "m".to_string(), args: vec![],
    }));
    assert!(!is_pure_two_step_value(&Expr::BinOp {
        left: Box::new(num(1.0)),
        op: crate::ast::BinOp::Add,
        right: Box::new(num(2.0)),
    }));
    assert!(!is_pure_two_step_value(&Expr::UnOp {
        op: crate::ast::UnOp::Negate,
        operand: Box::new(num(1.0)),
    }));
    assert!(!is_pure_two_step_value(&Expr::Field {
        object: Box::new(name("t")), field: "f".to_string(),
    }));
    assert!(!is_pure_two_step_value(&Expr::Index {
        object: Box::new(name("t")), key: Box::new(num(1.0)),
    }));
}

// ─── Direct helper test: two_step_field_absorb contract ─────────

#[test]
fn b048_helper_returns_none_when_f_name_collides_with_target() {
    // Protect against `local M = {}; local M = "oops"; M.x = M` —
    // shadowing via the same name is a hazard; helper must decline.
    let local_stmt = local_single("M", string("oops"));
    let assign_stmt = field_assign("M", "x", name("M"));
    let result = two_step_field_absorb(&local_stmt, &assign_stmt, "M", &HashMap::new());
    assert!(result.is_none(),
        "f_name == target_name must block absorb; got {:?}", result);
}
