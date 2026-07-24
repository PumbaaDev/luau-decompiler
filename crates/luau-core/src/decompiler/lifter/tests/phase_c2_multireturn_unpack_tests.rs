//! Phase C2 tests: `fold_multireturn_unpack`.
//!
//! Pattern folded:
//!   local v1, v2, v3 = f()
//!   x.a = v1
//!   x.b = v2
//!   x.c = v3
//! →
//!   x.a, x.b, x.c = f()
//!
//! Gated on: strictly sequential per-slot Assign stmts in order, each temp
//! read exactly once in the block.

use crate::ast::{Expr, Stat};
use crate::decompiler::lifter::post_passes::fold_multireturn_unpack;

fn name(s: &str) -> Expr { Expr::Name(s.to_string()) }

fn field(base: &str, f: &str) -> Expr {
    Expr::Field {
        object: Box::new(name(base)),
        field: f.to_string(),
    }
}

fn call(fname: &str) -> Expr {
    Expr::Call {
        func: Box::new(name(fname)),
        args: vec![],
    }
}

#[test]
fn c2_basic_three_result_fold() {
    // local v1, v2, v3 = f()
    // x.a = v1
    // x.b = v2
    // x.c = v3
    // → x.a, x.b, x.c = f()
    let mut stmts = vec![
        Stat::Local {
            names: vec!["v1".into(), "v2".into(), "v3".into()],
            values: vec![call("f")],
        },
        Stat::Assign {
            targets: vec![field("x", "a")],
            values: vec![name("v1")],
        },
        Stat::Assign {
            targets: vec![field("x", "b")],
            values: vec![name("v2")],
        },
        Stat::Assign {
            targets: vec![field("x", "c")],
            values: vec![name("v3")],
        },
    ];
    fold_multireturn_unpack(&mut stmts);
    assert_eq!(stmts.len(), 1, "expected single folded Assign, got {:?}", stmts);
    match &stmts[0] {
        Stat::Assign { targets, values } => {
            assert_eq!(targets.len(), 3, "expected 3 LHS targets");
            assert_eq!(values.len(), 1, "expected single multi-return RHS");
            // Targets are x.a, x.b, x.c in order
            for (i, f_name) in ["a", "b", "c"].iter().enumerate() {
                match &targets[i] {
                    Expr::Field { object, field } => {
                        assert!(matches!(&**object, Expr::Name(n) if n == "x"));
                        assert_eq!(field, f_name);
                    }
                    other => panic!("expected Field target at idx {}, got {:?}", i, other),
                }
            }
            assert!(matches!(&values[0], Expr::Call { .. }),
                "expected Call as sole value, got {:?}", values[0]);
        }
        other => panic!("expected Assign, got {:?}", other),
    }
}

#[test]
fn c2_different_order_must_not_fold() {
    // local v1, v2 = f()
    // x.a = v2   -- wrong order: v2 first
    // x.b = v1
    // → must NOT fold (order mismatch)
    let mut stmts = vec![
        Stat::Local {
            names: vec!["v1".into(), "v2".into()],
            values: vec![call("f")],
        },
        Stat::Assign {
            targets: vec![field("x", "a")],
            values: vec![name("v2")],
        },
        Stat::Assign {
            targets: vec![field("x", "b")],
            values: vec![name("v1")],
        },
    ];
    let before = stmts.clone();
    fold_multireturn_unpack(&mut stmts);
    assert_eq!(stmts.len(), before.len(), "out-of-order pattern must not fold");
    assert!(matches!(&stmts[0], Stat::Local { .. }),
        "Local should be preserved when order mismatches");
    assert!(matches!(&stmts[1], Stat::Assign { .. }));
    assert!(matches!(&stmts[2], Stat::Assign { .. }));
}

#[test]
fn c2_temp_used_twice_must_not_fold() {
    // local v1, v2 = f()
    // x.a = v1
    // x.b = v2
    // log(v1)          -- v1 used a second time
    // → must NOT fold (v1 read count != 1)
    let mut stmts = vec![
        Stat::Local {
            names: vec!["v1".into(), "v2".into()],
            values: vec![call("f")],
        },
        Stat::Assign {
            targets: vec![field("x", "a")],
            values: vec![name("v1")],
        },
        Stat::Assign {
            targets: vec![field("x", "b")],
            values: vec![name("v2")],
        },
        Stat::ExprStat(Expr::Call {
            func: Box::new(name("log")),
            args: vec![name("v1")],
        }),
    ];
    let len_before = stmts.len();
    fold_multireturn_unpack(&mut stmts);
    assert_eq!(stmts.len(), len_before,
        "temp used twice must prevent fold; got {:?}", stmts);
    assert!(matches!(&stmts[0], Stat::Local { .. }),
        "Local should be preserved when a temp has >1 read");
}
