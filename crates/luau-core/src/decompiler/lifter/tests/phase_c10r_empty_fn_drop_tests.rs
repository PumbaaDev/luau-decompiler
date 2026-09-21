//! Phase C10R — drop dead empty `function() end` stubs.
//!
//! C10f emits `Expr::Function { params: [], is_vararg: false, body: [] }`
//! as a placeholder when a child proto fails to lift (opcode_handlers.rs).
//! The file-header aggregate unresolved count already reports the failure,
//! so a downstream-dead `local X = function() end` carries zero diagnostic
//! value. ~5169 corpus occurrences (4152 from HUD alone).
//!
//! Safety is the same as C10b/C10Q: the post-pass must refuse to drop when
//! the local is either read OR reassigned downstream. Tests go directly
//! against the private `eliminate_dead_key_eq_value_locals` entry.

use super::super::eliminate_dead_key_eq_value_locals;
use crate::ast::{Expr, Stat};

fn empty_fn_local(name: &str) -> Stat {
    Stat::Local {
        names: vec![name.to_string()],
        values: vec![Expr::Function {
            params: vec![],
            is_vararg: false,
            body: vec![],
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
fn drops_dead_empty_fn_stub() {
    // local fn2 = function() end; return
    let mut stmts = vec![empty_fn_local("fn2"), ret_nothing()];
    eliminate_dead_key_eq_value_locals(&mut stmts);
    assert_eq!(stmts.len(), 1, "dead empty-fn stub should be dropped: {:?}", stmts);
    assert!(matches!(&stmts[0], Stat::Return { .. }));
}

#[test]
fn drops_dead_empty_fn_with_vn_name() {
    // local v_7 = function() end
    let mut stmts = vec![empty_fn_local("v_7"), ret_nothing()];
    eliminate_dead_key_eq_value_locals(&mut stmts);
    assert_eq!(stmts.len(), 1, "v_N empty-fn stub should also drop");
}

#[test]
fn keeps_empty_fn_when_returned() {
    // local fn2 = function() end; return fn2
    let mut stmts = vec![empty_fn_local("fn2"), ret_name("fn2")];
    eliminate_dead_key_eq_value_locals(&mut stmts);
    assert_eq!(stmts.len(), 2, "returned empty-fn must not be dropped");
}

#[test]
fn keeps_empty_fn_when_called_downstream() {
    // local fn2 = function() end; fn2()
    let mut stmts = vec![
        empty_fn_local("fn2"),
        Stat::ExprStat(Expr::Call {
            func: Box::new(Expr::Name("fn2".to_string())),
            args: vec![],
        }),
    ];
    eliminate_dead_key_eq_value_locals(&mut stmts);
    assert_eq!(stmts.len(), 2, "called empty-fn must not be dropped");
}

#[test]
fn keeps_empty_fn_when_reassigned_later() {
    // local fn2 = function() end; fn2 = nil
    // Dropping would silently turn the reassignment into a global write.
    let mut stmts = vec![
        empty_fn_local("fn2"),
        Stat::Assign {
            targets: vec![Expr::Name("fn2".to_string())],
            values: vec![Expr::Nil],
        },
    ];
    eliminate_dead_key_eq_value_locals(&mut stmts);
    assert_eq!(stmts.len(), 2, "downstream reassignment must block drop");
}

#[test]
fn keeps_function_with_params() {
    // local fn2 = function(a) end — non-empty params → not a C10f stub
    let mut stmts = vec![
        Stat::Local {
            names: vec!["fn2".to_string()],
            values: vec![Expr::Function {
                params: vec!["a".to_string()],
                is_vararg: false,
                body: vec![],
            }],
        },
        ret_nothing(),
    ];
    eliminate_dead_key_eq_value_locals(&mut stmts);
    assert_eq!(stmts.len(), 2, "function with params must not be dropped by C10R");
}

#[test]
fn keeps_function_with_body() {
    // local fn2 = function() return 1 end — non-empty body → not a stub
    let mut stmts = vec![
        Stat::Local {
            names: vec!["fn2".to_string()],
            values: vec![Expr::Function {
                params: vec![],
                is_vararg: false,
                body: vec![Stat::Return { values: vec![Expr::Number(1.0)] }],
            }],
        },
        ret_nothing(),
    ];
    eliminate_dead_key_eq_value_locals(&mut stmts);
    assert_eq!(stmts.len(), 2, "function with body must not be dropped by C10R");
}

#[test]
fn keeps_vararg_function() {
    // local fn2 = function(...) end — is_vararg → not a C10f zero-arg stub
    let mut stmts = vec![
        Stat::Local {
            names: vec!["fn2".to_string()],
            values: vec![Expr::Function {
                params: vec![],
                is_vararg: true,
                body: vec![],
            }],
        },
        ret_nothing(),
    ];
    eliminate_dead_key_eq_value_locals(&mut stmts);
    assert_eq!(stmts.len(), 2, "vararg empty-body function must not be dropped");
}

#[test]
fn drops_nested_empty_fn_inside_outer_closure() {
    // local g = function() local fn2 = function() end; return end
    let mut stmts = vec![Stat::Local {
        names: vec!["g".to_string()],
        values: vec![Expr::Function {
            params: vec![],
            is_vararg: false,
            body: vec![empty_fn_local("fn2"), ret_nothing()],
        }],
    }];
    eliminate_dead_key_eq_value_locals(&mut stmts);
    assert_eq!(stmts.len(), 1);
    if let Stat::Local { values, .. } = &stmts[0] {
        if let Expr::Function { body, .. } = &values[0] {
            assert_eq!(body.len(), 1, "inner dead empty-fn stub should drop");
            assert!(matches!(&body[0], Stat::Return { .. }));
        } else {
            panic!("expected function RHS");
        }
    } else {
        panic!("expected local");
    }
}

#[test]
fn keeps_empty_fn_when_read_in_nested_closure() {
    // local fn2 = function() end
    // local g = function() return fn2 end
    let mut stmts = vec![
        empty_fn_local("fn2"),
        Stat::Local {
            names: vec!["g".to_string()],
            values: vec![Expr::Function {
                params: vec![],
                is_vararg: false,
                body: vec![ret_name("fn2")],
            }],
        },
    ];
    eliminate_dead_key_eq_value_locals(&mut stmts);
    assert_eq!(stmts.len(), 2, "nested-closure read must block drop");
}
