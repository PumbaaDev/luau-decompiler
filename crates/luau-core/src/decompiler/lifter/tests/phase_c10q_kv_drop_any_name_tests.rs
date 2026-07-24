//! Phase C10Q — extend `{K="K"}` dead-local drop to any local name.
//!
//! C10b originally only dropped `local v_N = { K = "K" }`. The the reference
//! HUD corpus has ~1611 `Get = "Get"` occurrences inside deeply-nested
//! closure stubs where the enclosing local has a non-`v_N` name
//! (e.g. `local fn2 = { Get = "Get" }`, `local isInTouchZone2 = ...`).
//! The read-check already recurses into nested closures, so extending
//! the name gate is safe as long as we also guard against downstream
//! reassignment to the same name (which would silently become a global
//! write if the `local` disappeared).
//!
//! Tests in this module go directly against
//! `eliminate_dead_key_eq_value_locals` — a private post-pass in
//! `lifter/mod.rs`. The pass is reachable from this submodule without
//! a visibility change.

use super::super::eliminate_dead_key_eq_value_locals;
use crate::ast::{Expr, Stat, TableField};

fn local_kv(name: &str, k: &str, v: &str) -> Stat {
    Stat::Local {
        names: vec![name.to_string()],
        values: vec![Expr::Table {
            fields: vec![TableField::Named(k.to_string(), Expr::String(v.to_string()))],
        }],
    }
}

fn ret_nothing() -> Stat {
    Stat::Return { values: vec![] }
}

fn ret_name(n: &str) -> Stat {
    Stat::Return { values: vec![Expr::Name(n.to_string())] }
}

#[test]
fn drops_non_vn_local_when_unused() {
    // local fn2 = { Get = "Get" }; return
    let mut stmts = vec![local_kv("fn2", "Get", "Get"), ret_nothing()];
    eliminate_dead_key_eq_value_locals(&mut stmts);
    assert_eq!(stmts.len(), 1, "should drop dead local: {:?}", stmts);
    assert!(matches!(&stmts[0], Stat::Return { .. }));
}

#[test]
fn drops_multifield_kv_table_when_unused() {
    let mut stmts = vec![
        Stat::Local {
            names: vec!["dispatch".to_string()],
            values: vec![Expr::Table {
                fields: vec![
                    TableField::Named("Get".to_string(), Expr::String("Get".to_string())),
                    TableField::Named("Set".to_string(), Expr::String("Set".to_string())),
                ],
            }],
        },
        ret_nothing(),
    ];
    eliminate_dead_key_eq_value_locals(&mut stmts);
    assert_eq!(stmts.len(), 1, "multi-field K=K should still drop when unused");
}

#[test]
fn keeps_local_when_returned() {
    let mut stmts = vec![local_kv("Foo", "Get", "Get"), ret_name("Foo")];
    eliminate_dead_key_eq_value_locals(&mut stmts);
    assert_eq!(stmts.len(), 2, "must not drop when downstream returns it");
}

#[test]
fn keeps_local_when_read_in_nested_closure() {
    // local Foo = {Get="Get"}; local g = function() return Foo end
    let mut stmts = vec![
        local_kv("Foo", "Get", "Get"),
        Stat::Local {
            names: vec!["g".to_string()],
            values: vec![Expr::Function {
                params: vec![],
                is_vararg: false,
                body: vec![ret_name("Foo")],
            }],
        },
    ];
    eliminate_dead_key_eq_value_locals(&mut stmts);
    assert_eq!(stmts.len(), 2, "nested-closure read must block drop");
}

#[test]
fn keeps_local_when_reassigned_later() {
    // local Foo = {Get="Get"}; Foo = nil
    // Dropping would turn the reassignment into an implicit global write.
    let mut stmts = vec![
        local_kv("Foo", "Get", "Get"),
        Stat::Assign {
            targets: vec![Expr::Name("Foo".to_string())],
            values: vec![Expr::Nil],
        },
    ];
    eliminate_dead_key_eq_value_locals(&mut stmts);
    assert_eq!(stmts.len(), 2, "downstream reassignment must block drop");
}

#[test]
fn keeps_non_kv_table() {
    // local Foo = {Get = 42}; — value is not a string matching the key
    let mut stmts = vec![
        Stat::Local {
            names: vec!["Foo".to_string()],
            values: vec![Expr::Table {
                fields: vec![TableField::Named(
                    "Get".to_string(),
                    Expr::Number(42.0),
                )],
            }],
        },
        ret_nothing(),
    ];
    eliminate_dead_key_eq_value_locals(&mut stmts);
    assert_eq!(stmts.len(), 2, "non-K=K table must not be dropped by this pass");
}

#[test]
fn keeps_kv_mismatch() {
    // local Foo = {Get = "Got"}; — key != value
    let mut stmts = vec![local_kv("Foo", "Get", "Got"), ret_nothing()];
    eliminate_dead_key_eq_value_locals(&mut stmts);
    assert_eq!(stmts.len(), 2, "K != V must not be dropped");
}

#[test]
fn drops_nested_inside_function_body() {
    // local g = function() local fn2 = {Get="Get"}; return end
    let mut stmts = vec![Stat::Local {
        names: vec!["g".to_string()],
        values: vec![Expr::Function {
            params: vec![],
            is_vararg: false,
            body: vec![local_kv("fn2", "Get", "Get"), ret_nothing()],
        }],
    }];
    eliminate_dead_key_eq_value_locals(&mut stmts);
    // outer vec still has one element (the function local)
    assert_eq!(stmts.len(), 1);
    if let Stat::Local { values, .. } = &stmts[0] {
        if let Expr::Function { body, .. } = &values[0] {
            assert_eq!(body.len(), 1, "inner dead K=K local should be dropped");
            assert!(matches!(&body[0], Stat::Return { .. }));
        } else {
            panic!("expected function RHS");
        }
    } else {
        panic!("expected local");
    }
}
