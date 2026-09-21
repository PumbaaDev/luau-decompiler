//! Phase C2 pass #4 — SETLIST / sequential-integer-index table coalesce.
//!
//! The lifter emits `local t = {}; t[1] = a; t[2] = b; t[3] = c` for the
//! SETLIST bytecode family.  After `reconstruct_table_constructors` runs,
//! those assignments collapse into a single `Expr::Table` with
//! `TableField::Indexed(Number(k), v)` entries (integer keys don't round-trip
//! as identifiers, so they go via the indexed form).
//!
//! `coalesce_setlist_sequential` promotes any leading run of
//! `Indexed(Number(1)), Indexed(Number(2)), ..., Indexed(Number(N))` fields
//! into `Sequential(v)` so the emitter prints `{a, b, c}` instead of
//! `{[1] = a, [2] = b, [3] = c}`.
//!
//! Tests:
//!   1. basic 5-element array folds every field to Sequential
//!   2. gap in indices (1, 2, 4 — missing 3) — fold only the 1,2 prefix,
//!      the gap entry stays `Indexed`

use super::super::{coalesce_setlist_sequential, reconstruct_table_constructors};
use crate::ast::{Expr, Stat, TableField};

fn name(s: &str) -> Expr { Expr::Name(s.to_string()) }
fn num(n: f64) -> Expr { Expr::Number(n) }
fn empty_table() -> Expr { Expr::Table { fields: vec![] } }

fn local_empty(n: &str) -> Stat {
    Stat::Local { names: vec![n.to_string()], values: vec![empty_table()] }
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

// ─── Test 1: basic 5-element array fold ─────────────────────────────────

#[test]
fn c2_basic_five_element_array_folds_to_sequential() {
    // local t = {}; t[1]="a"; t[2]="b"; t[3]="c"; t[4]="d"; t[5]="e"; return t
    // After reconstruct_table_constructors + coalesce_setlist_sequential:
    //   local t = {"a", "b", "c", "d", "e"}; return t
    let mut stmts = vec![
        local_empty("t"),
        index_assign("t", num(1.0), Expr::String("a".into())),
        index_assign("t", num(2.0), Expr::String("b".into())),
        index_assign("t", num(3.0), Expr::String("c".into())),
        index_assign("t", num(4.0), Expr::String("d".into())),
        index_assign("t", num(5.0), Expr::String("e".into())),
        ret_name("t"),
    ];

    reconstruct_table_constructors(&mut stmts);
    coalesce_setlist_sequential(&mut stmts);

    assert_eq!(
        stmts.len(),
        2,
        "expected seed + return after fold; got {} stmts: {:?}",
        stmts.len(),
        stmts
    );

    match &stmts[0] {
        Stat::Local { names, values } => {
            assert_eq!(names, &vec!["t".to_string()]);
            assert_eq!(values.len(), 1);
            match &values[0] {
                Expr::Table { fields } => {
                    assert_eq!(fields.len(), 5, "expected 5 fields, got {}", fields.len());
                    let expected = ["a", "b", "c", "d", "e"];
                    for (i, f) in fields.iter().enumerate() {
                        match f {
                            TableField::Sequential(Expr::String(s)) => {
                                assert_eq!(
                                    s, expected[i],
                                    "field {} expected Sequential(\"{}\") got Sequential(\"{}\")",
                                    i, expected[i], s
                                );
                            }
                            other => panic!(
                                "field {} should be Sequential(String), got {:?}",
                                i, other
                            ),
                        }
                    }
                }
                other => panic!("expected Table value, got {:?}", other),
            }
        }
        other => panic!("expected Local seed, got {:?}", other),
    }

    match &stmts[1] {
        Stat::Return { values } => {
            assert_eq!(values.len(), 1);
            match &values[0] {
                Expr::Name(n) => assert_eq!(n, "t"),
                other => panic!("return value should be Name(\"t\"), got {:?}", other),
            }
        }
        other => panic!("expected Return, got {:?}", other),
    }
}

// ─── Test 2: gap in indices (1, 2, 4 — missing 3) — must NOT fold gap ──

#[test]
fn c2_gap_in_indices_does_not_fold_past_gap() {
    // local t = {}; t[1]="a"; t[2]="b"; t[4]="d"; return t
    // After reconstruct_table_constructors all three assigns fold into
    // TableField::Indexed entries.  coalesce_setlist_sequential then
    // promotes the 1,2 prefix to Sequential but leaves the 4 as Indexed
    // (gap at 3 breaks the contiguous run).
    let mut stmts = vec![
        local_empty("t"),
        index_assign("t", num(1.0), Expr::String("a".into())),
        index_assign("t", num(2.0), Expr::String("b".into())),
        index_assign("t", num(4.0), Expr::String("d".into())),
        ret_name("t"),
    ];

    reconstruct_table_constructors(&mut stmts);
    coalesce_setlist_sequential(&mut stmts);

    // All three field-assigns should still be absorbed into the Table —
    // reconstruct_table_constructors folds any indexed writes.
    assert_eq!(
        stmts.len(),
        2,
        "expected seed + return after fold; got {} stmts: {:?}",
        stmts.len(),
        stmts
    );

    match &stmts[0] {
        Stat::Local { values, .. } => {
            match &values[0] {
                Expr::Table { fields } => {
                    assert_eq!(
                        fields.len(),
                        3,
                        "expected 3 fields; got {}: {:?}",
                        fields.len(),
                        fields
                    );
                    // First two should be Sequential (1,2 prefix promoted).
                    match &fields[0] {
                        TableField::Sequential(Expr::String(s)) => {
                            assert_eq!(s, "a", "fields[0] should be Sequential(\"a\")");
                        }
                        other => panic!(
                            "fields[0] should be Sequential(\"a\"), got {:?}",
                            other
                        ),
                    }
                    match &fields[1] {
                        TableField::Sequential(Expr::String(s)) => {
                            assert_eq!(s, "b", "fields[1] should be Sequential(\"b\")");
                        }
                        other => panic!(
                            "fields[1] should be Sequential(\"b\"), got {:?}",
                            other
                        ),
                    }
                    // Third must remain Indexed because 4 is a gap (3 missing).
                    match &fields[2] {
                        TableField::Indexed(Expr::Number(k), Expr::String(s)) => {
                            assert_eq!(*k, 4.0, "gap field key should still be 4");
                            assert_eq!(s, "d", "gap field value should be \"d\"");
                        }
                        other => panic!(
                            "fields[2] should be Indexed(Number(4.0), String(\"d\")) \
                             because of the gap at 3; got {:?}",
                            other
                        ),
                    }
                }
                other => panic!("expected Table value, got {:?}", other),
            }
        }
        other => panic!("expected Local seed, got {:?}", other),
    }

    // Silence unused-import warning for `name` — we only use helper fns above.
    let _ = name;
}
