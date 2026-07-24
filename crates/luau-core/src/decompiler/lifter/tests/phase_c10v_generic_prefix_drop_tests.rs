//! Phase C10V — extend `is_generic_vn` to cover decompiler-generated prefixes
//! beyond `v\d+`: `result\d+`, `fn\d+`, `tbl\d+`, `arg\d+`.
//!
//! Concrete corpus counts that motivate the change (the reference build 1776658242):
//!   `local result\d+ = {}`: 651
//!   `local fn\d+ = {}`:     771
//!   `local tbl\d+ = {}`:    997
//! All pure garbage when the name is never read or reassigned downstream.
//!
//! The tests exercise the existing `eliminate_dead_key_eq_value_locals`
//! pure-RHS drop path, which becomes active for these names once the
//! gate accepts them.

use super::super::eliminate_dead_key_eq_value_locals;
use crate::ast::{Expr, Stat};

fn local_empty_table(name: &str) -> Stat {
    Stat::Local {
        names: vec![name.to_string()],
        values: vec![Expr::Table { fields: vec![] }],
    }
}

fn ret_nothing() -> Stat {
    Stat::Return { values: vec![] }
}

fn ret_name(n: &str) -> Stat {
    Stat::Return { values: vec![Expr::Name(n.to_string())] }
}

#[test]
fn drops_dead_result_empty_table() {
    let mut stmts = vec![local_empty_table("result32"), ret_nothing()];
    eliminate_dead_key_eq_value_locals(&mut stmts);
    assert_eq!(stmts.len(), 1, "dead `local result32 = {{}}` should drop: {:?}", stmts);
}

#[test]
fn drops_dead_fn_empty_table() {
    let mut stmts = vec![local_empty_table("fn7"), ret_nothing()];
    eliminate_dead_key_eq_value_locals(&mut stmts);
    assert_eq!(stmts.len(), 1, "dead `local fn7 = {{}}` should drop");
}

#[test]
fn drops_dead_tbl_empty_table() {
    let mut stmts = vec![local_empty_table("tbl14"), ret_nothing()];
    eliminate_dead_key_eq_value_locals(&mut stmts);
    assert_eq!(stmts.len(), 1, "dead `local tbl14 = {{}}` should drop");
}

#[test]
fn drops_dead_arg_empty_table() {
    let mut stmts = vec![local_empty_table("arg3"), ret_nothing()];
    eliminate_dead_key_eq_value_locals(&mut stmts);
    assert_eq!(stmts.len(), 1, "dead `local arg3 = {{}}` should drop");
}

#[test]
fn keeps_used_result_local() {
    // `local result5 = {}; return result5` — still referenced, must survive.
    let mut stmts = vec![local_empty_table("result5"), ret_name("result5")];
    eliminate_dead_key_eq_value_locals(&mut stmts);
    assert_eq!(stmts.len(), 2, "used local must not drop: {:?}", stmts);
}

#[test]
fn keeps_user_named_local_with_prefix_substring() {
    // `result_total` is a user-shaped name: starts with "result" but the
    // tail is not all digits. Must not match the generic gate.
    let mut stmts = vec![local_empty_table("result_total"), ret_nothing()];
    eliminate_dead_key_eq_value_locals(&mut stmts);
    assert_eq!(stmts.len(), 2, "user-shaped name must NOT drop: {:?}", stmts);
}

#[test]
fn keeps_bare_prefix_without_digits() {
    // Plain `result` (no digits) is a plausible user local; do not touch.
    let mut stmts = vec![local_empty_table("result"), ret_nothing()];
    eliminate_dead_key_eq_value_locals(&mut stmts);
    assert_eq!(stmts.len(), 2, "bare `result` is a user name, must NOT drop");
}

#[test]
fn keeps_result_with_downstream_reassignment() {
    // `local result2 = {}; result2 = something` — dropping would turn the
    // second line into a global write. The `no_downstream_write` guard
    // inside the pass must keep the local even when unread.
    let mut stmts = vec![
        local_empty_table("result2"),
        Stat::Assign {
            targets: vec![Expr::Name("result2".to_string())],
            values: vec![Expr::Nil],
        },
    ];
    eliminate_dead_key_eq_value_locals(&mut stmts);
    assert_eq!(stmts.len(), 2, "must NOT drop when later reassigned: {:?}", stmts);
}
