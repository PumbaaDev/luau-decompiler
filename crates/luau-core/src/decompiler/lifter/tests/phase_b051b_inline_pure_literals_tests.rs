//! Extracted from lifter.rs as part of Phase B0.52P6 (part 3).
//! `super::super::` references the `lifter` module.

//! Phase B0.51B — `inline_pure_literals` always-inline pass for
//! literal RHS values (Nil/Bool/Number/String only; Vector, Import,
//! Table, Function are excluded).  Targets the Roblox pattern where
//! a register is reused with multiple LOADK loads, e.g.
//! `local v3 = "Players"; game:GetService(v3); ...
//! game:GetService(v3); ...` becomes `game:GetService("Players"); ...`
//! at every site, regardless of read count.
//!
//! Required tests (6+):
//!   1. LOADK string arg inlined into method call (single use)
//!   2. LOADK string arg inlined into MULTIPLE method calls
//!   3. Number literal inlined into multiple binop reads
//!   4. Bool/Nil literal inlined at multiple sites
//!   5. Reassignment truncates the inlining range
//!   6. Capitalized name is preserved (Konstant style)
//!   7. Non-literal pure RHS (Field) does NOT inline at multiple sites
//!   8. Multi-value Local skipped
//!   9. Pure literal in loop body inlines (re-evaluation OK)
//!  10. Existing B0.45A single-use behavior preserved
//!  11. GETIMPORT-style Field-of-Name (game.Players) covered by B0.45A still
//!  12. Reassignment kept and reads after it untouched

use super::super::{inline_pure_literals, inline_single_use_temps,
    is_inlinable_literal, stmt_writes_name_recursive};
use crate::ast::{BinOp, Expr, Stat};

fn name(s: &str) -> Expr { Expr::Name(s.to_string()) }
fn num(n: f64) -> Expr { Expr::Number(n) }
fn string(s: &str) -> Expr { Expr::String(s.to_string()) }

fn local(n: &str, e: Expr) -> Stat {
    Stat::Local { names: vec![n.to_string()], values: vec![e] }
}

fn assign_name(n: &str, e: Expr) -> Stat {
    Stat::Assign { targets: vec![Expr::Name(n.to_string())], values: vec![e] }
}

fn call(func: Expr, args: Vec<Expr>) -> Expr {
    Expr::Call { func: Box::new(func), args }
}

fn methodcall(obj: Expr, m: &str, args: Vec<Expr>) -> Expr {
    Expr::MethodCall { object: Box::new(obj), method: m.to_string(), args }
}

fn field(obj: Expr, f: &str) -> Expr {
    Expr::Field { object: Box::new(obj), field: f.to_string() }
}

fn ret(values: Vec<Expr>) -> Stat {
    Stat::Return { values }
}

// ─── helper sanity checks ────────────────────────────────────────

#[test]
fn b051b_is_inlinable_literal_classifies_correctly() {
    assert!(is_inlinable_literal(&Expr::Nil));
    assert!(is_inlinable_literal(&Expr::Bool(true)));
    assert!(is_inlinable_literal(&num(1.0)));
    assert!(is_inlinable_literal(&string("x")));
    // Vector is intentionally excluded (user preference to keep
    // explicit Vector3.new at the declaration site).
    assert!(!is_inlinable_literal(&Expr::Vector(0.0, 0.0, 0.0)));
    // Names, fields, calls, etc. are NOT literals.
    assert!(!is_inlinable_literal(&name("x")));
    assert!(!is_inlinable_literal(&field(name("t"), "f")));
    assert!(!is_inlinable_literal(&call(name("f"), vec![])));
    assert!(!is_inlinable_literal(&Expr::Varargs));
}

#[test]
fn b051b_stmt_writes_name_recursive_finds_local_and_assign() {
    assert!(stmt_writes_name_recursive(&local("v3", num(1.0)), "v3"));
    assert!(stmt_writes_name_recursive(&assign_name("v3", num(1.0)), "v3"));
    assert!(!stmt_writes_name_recursive(&assign_name("other", num(1.0)), "v3"));
}

#[test]
fn b051b_stmt_writes_name_recursive_recurses_into_if() {
    // `if cond then v3 = 1 end` writes to v3
    let s = Stat::If {
        condition: Expr::Bool(true),
        then_body: vec![assign_name("v3", num(1.0))],
        elseif_clauses: vec![],
        else_body: None,
    };
    assert!(stmt_writes_name_recursive(&s, "v3"));
}

// ─── Required test 1: single-use literal arg inlined ─────────────

#[test]
fn b051b_loadk_string_arg_inlined_into_method_call() {
    // local v3 = "Players"
    // game:GetService(v3)
    //
    // After: `game:GetService("Players")` (single read; B0.45A also
    // handles this — we re-test under the new pass for completeness.)
    let mut stmts = vec![
        local("v3", string("Players")),
        Stat::ExprStat(methodcall(name("game"), "GetService", vec![name("v3")])),
    ];
    inline_pure_literals(&mut stmts);
    assert_eq!(stmts.len(), 1, "expected single stmt; got {:?}", stmts);
    match &stmts[0] {
        Stat::ExprStat(Expr::MethodCall { args, method, .. }) => {
            assert_eq!(method, "GetService");
            assert_eq!(args.len(), 1);
            assert!(matches!(&args[0], Expr::String(s) if s == "Players"),
                "expected GetService(\"Players\"); got {:?}", args[0]);
        }
        other => panic!("expected ExprStat(MethodCall); got {:?}", other),
    }
}

// ─── Required test 2: literal inlined at MULTIPLE call sites ─────

#[test]
fn b051b_loadk_string_arg_inlined_into_multiple_method_calls() {
    // local v3 = "UserInputService"
    // game:GetService(v3)
    // workspace:GetService(v3)
    // somewhere:GetService(v3)
    //
    // After: each call has the literal "UserInputService" and the
    // local v3 declaration is gone.
    let mut stmts = vec![
        local("v3", string("UserInputService")),
        Stat::ExprStat(methodcall(name("game"), "GetService", vec![name("v3")])),
        Stat::ExprStat(methodcall(name("workspace"), "GetService", vec![name("v3")])),
        Stat::ExprStat(methodcall(name("somewhere"), "GetService", vec![name("v3")])),
    ];
    inline_pure_literals(&mut stmts);
    // v3 declaration should be gone since all reads were inlined
    // and there's no future reassignment.
    assert_eq!(stmts.len(), 3,
        "expected 3 stmts (decl removed); got {:?}", stmts);
    for s in &stmts {
        match s {
            Stat::ExprStat(Expr::MethodCall { args, .. }) => {
                assert!(matches!(&args[0], Expr::String(s) if s == "UserInputService"),
                    "expected UserInputService literal; got {:?}", args[0]);
            }
            other => panic!("expected method call; got {:?}", other),
        }
    }
}

// ─── Required test 3: number literal across binop reads ──────────

#[test]
fn b051b_number_literal_inlined_into_multiple_binop_reads() {
    // local k = 5
    // local a = x + k
    // local b = y + k
    // return a + b
    //
    // After: `local a = x + 5; local b = y + 5; return a + b`
    let mut stmts = vec![
        local("k", num(5.0)),
        local("a", Expr::BinOp {
            left: Box::new(name("x")),
            op: BinOp::Add,
            right: Box::new(name("k")),
        }),
        local("b", Expr::BinOp {
            left: Box::new(name("y")),
            op: BinOp::Add,
            right: Box::new(name("k")),
        }),
        ret(vec![Expr::BinOp {
            left: Box::new(name("a")),
            op: BinOp::Add,
            right: Box::new(name("b")),
        }]),
    ];
    inline_pure_literals(&mut stmts);
    // k declaration removed; a and b use literal 5.
    assert_eq!(stmts.len(), 3, "decl k must be removed; got {:?}", stmts);
    let check_rhs_is_5 = |s: &Stat| match s {
        Stat::Local { values, .. } => match &values[0] {
            Expr::BinOp { right, .. } => matches!(**right, Expr::Number(n) if (n - 5.0).abs() < 1e-9),
            _ => false,
        },
        _ => false,
    };
    assert!(check_rhs_is_5(&stmts[0]), "a should use 5; got {:?}", stmts[0]);
    assert!(check_rhs_is_5(&stmts[1]), "b should use 5; got {:?}", stmts[1]);
}

// ─── Required test 4: bool/nil literal at multiple sites ─────────

#[test]
fn b051b_bool_and_nil_literals_inlined_freely() {
    // local flag = true
    // f(flag)
    // g(flag)
    let mut stmts = vec![
        local("flag", Expr::Bool(true)),
        Stat::ExprStat(call(name("f"), vec![name("flag")])),
        Stat::ExprStat(call(name("g"), vec![name("flag")])),
    ];
    inline_pure_literals(&mut stmts);
    assert_eq!(stmts.len(), 2, "decl flag removed; got {:?}", stmts);
    for s in &stmts {
        match s {
            Stat::ExprStat(Expr::Call { args, .. }) => {
                assert!(matches!(args[0], Expr::Bool(true)),
                    "expected literal true; got {:?}", args[0]);
            }
            other => panic!("expected Call; got {:?}", other),
        }
    }
}

// ─── Required test 5: reassignment truncates inlining range ──────

#[test]
fn b051b_reassignment_truncates_range() {
    // local v3 = "Players"
    // game:GetService(v3)        -- inline → "Players"
    // v3 = "Workspace"           -- reassign
    // game:GetService(v3)        -- DO NOT inline (sees new value)
    //
    // After:
    //   local v3 = "Players"     -- kept (because of later reassign)
    //   game:GetService("Players")
    //   v3 = "Workspace"
    //   game:GetService(v3)
    let mut stmts = vec![
        local("v3", string("Players")),
        Stat::ExprStat(methodcall(name("game"), "GetService", vec![name("v3")])),
        assign_name("v3", string("Workspace")),
        Stat::ExprStat(methodcall(name("game"), "GetService", vec![name("v3")])),
    ];
    inline_pure_literals(&mut stmts);
    // Local must survive (reassignment ahead).
    assert_eq!(stmts.len(), 4, "all 4 stmts kept; got {:?}", stmts);
    // First call: literal "Players"
    match &stmts[1] {
        Stat::ExprStat(Expr::MethodCall { args, .. }) =>
            assert!(matches!(&args[0], Expr::String(s) if s == "Players"),
                "first call should hold literal; got {:?}", args[0]),
        other => panic!("expected MethodCall; got {:?}", other),
    }
    // Second call: still the Name(v3), NOT the literal
    match &stmts[3] {
        Stat::ExprStat(Expr::MethodCall { args, .. }) =>
            assert!(matches!(&args[0], Expr::Name(n) if n == "v3"),
                "second call should keep v3; got {:?}", args[0]),
        other => panic!("expected MethodCall; got {:?}", other),
    }
}

// ─── Required test 6: Capitalized name preserved ─────────────────

#[test]
fn b051b_capitalized_name_preserved() {
    // local Foo = "Bar"
    // print(Foo)
    // print(Foo)
    //
    // Capitalized names are kept (Konstant-style imports).
    let mut stmts = vec![
        local("Foo", string("Bar")),
        Stat::ExprStat(call(name("print"), vec![name("Foo")])),
        Stat::ExprStat(call(name("print"), vec![name("Foo")])),
    ];
    inline_pure_literals(&mut stmts);
    // Decl must survive untouched.
    assert!(matches!(&stmts[0], Stat::Local { names, .. } if names[0] == "Foo"),
        "Capitalized name must survive; got {:?}", stmts[0]);
    // Reads still reference Foo, not "Bar".
    match &stmts[1] {
        Stat::ExprStat(Expr::Call { args, .. }) =>
            assert!(matches!(&args[0], Expr::Name(n) if n == "Foo"),
                "read must remain Name(Foo); got {:?}", args[0]),
        other => panic!("expected Call; got {:?}", other),
    }
}

// ─── Required test 7: non-literal pure RHS NOT inlined ───────────

#[test]
fn b051b_non_literal_pure_rhs_not_multi_inlined() {
    // local v1 = t.field        (Field is pure but NOT a literal)
    // f(v1)
    // g(v1)
    //
    // Multi-use Field: the existing B0.45A single-use guard skips it
    // (count > 1) AND our new pass also skips because Field isn't
    // a literal.  Decl must survive, both reads still reference v1.
    let mut stmts = vec![
        local("v1", field(name("t"), "field")),
        Stat::ExprStat(call(name("f"), vec![name("v1")])),
        Stat::ExprStat(call(name("g"), vec![name("v1")])),
    ];
    inline_pure_literals(&mut stmts);
    assert!(matches!(&stmts[0], Stat::Local { names, .. } if names[0] == "v1"),
        "Field RHS must NOT be inlined at multiple sites; got {:?}", stmts[0]);
}

// ─── Required test 8: multi-value Local skipped ──────────────────

#[test]
fn b051b_multi_value_local_skipped() {
    // local a, b = 1, 2
    // f(a)
    // g(b)
    //
    // Multi-name local skipped (structural guard).
    let mut stmts = vec![
        Stat::Local {
            names: vec!["a".to_string(), "b".to_string()],
            values: vec![num(1.0), num(2.0)],
        },
        Stat::ExprStat(call(name("f"), vec![name("a")])),
        Stat::ExprStat(call(name("g"), vec![name("b")])),
    ];
    inline_pure_literals(&mut stmts);
    // Local must survive.
    assert!(matches!(&stmts[0], Stat::Local { names, .. } if names.len() == 2),
        "multi-value Local must survive; got {:?}", stmts[0]);
}

// ─── Required test 9: pure literal inside a loop body OK ─────────

#[test]
fn b051b_pure_literal_inlines_into_loop_body() {
    // local k = 42
    // for i = 1, 10 do
    //     use(k)
    // end
    //
    // Re-evaluating the literal 42 each iteration is harmless, so
    // inlining is safe and the local can be removed.
    let mut stmts = vec![
        local("k", num(42.0)),
        Stat::NumericFor {
            var: "i".to_string(),
            start: num(1.0),
            stop: num(10.0),
            step: None,
            body: vec![Stat::ExprStat(call(name("use"), vec![name("k")]))],
        },
    ];
    inline_pure_literals(&mut stmts);
    // k removed.
    assert_eq!(stmts.len(), 1, "decl k must be removed; got {:?}", stmts);
    // Inside the for-loop body, the call now holds literal 42.
    match &stmts[0] {
        Stat::NumericFor { body, .. } => {
            match &body[0] {
                Stat::ExprStat(Expr::Call { args, .. }) =>
                    assert!(matches!(args[0], Expr::Number(n) if (n - 42.0).abs() < 1e-9),
                        "expected literal 42; got {:?}", args[0]),
                other => panic!("expected Call inside for; got {:?}", other),
            }
        }
        other => panic!("expected NumericFor; got {:?}", other),
    }
}

// ─── Required test 10: Loop-body re-eval guard for impure RHS ────

#[test]
fn b051b_impure_rhs_in_loop_body_blocked_by_b045a() {
    // local v1 = makeThing()        -- impure call
    // for i = 1, 10 do use(v1) end
    //
    // Pure-literal pass doesn't fire (RHS isn't a literal).
    // B0.45A's loop guard correctly blocks the impure inlining.
    let mut stmts = vec![
        local("v1", call(name("makeThing"), vec![])),
        Stat::NumericFor {
            var: "i".to_string(),
            start: num(1.0),
            stop: num(10.0),
            step: None,
            body: vec![Stat::ExprStat(call(name("use"), vec![name("v1")]))],
        },
    ];
    inline_single_use_temps(&mut stmts);
    inline_pure_literals(&mut stmts);
    // local v1 must survive both passes.
    assert!(matches!(&stmts[0], Stat::Local { names, .. } if names[0] == "v1"),
        "impure RHS must NOT be inlined into loop; got {:?}", stmts[0]);
}

// ─── Required test 11: B0.45A single-use behavior preserved ──────

#[test]
fn b051b_b045a_single_use_still_works() {
    // local result = someFunc()
    // return result
    //
    // B0.45A inlines this; B0.51B doesn't touch it (RHS isn't a
    // literal).  Combined behavior must still produce inlined result.
    let mut stmts = vec![
        local("result", call(name("someFunc"), vec![])),
        ret(vec![name("result")]),
    ];
    inline_single_use_temps(&mut stmts);
    inline_pure_literals(&mut stmts);
    assert_eq!(stmts.len(), 1);
    match &stmts[0] {
        Stat::Return { values } =>
            assert!(matches!(&values[0], Expr::Call { .. }),
                "expected inlined call; got {:?}", values[0]),
        other => panic!("expected Return; got {:?}", other),
    }
}

// ─── Required test 12: pre-reassignment reads inlined, post kept ─

#[test]
fn b051b_partial_inline_with_kept_local_for_reassignment() {
    // local n = 1
    // print(n)         -- inline to 1
    // n = 2
    // print(n)         -- NOT inlined
    let mut stmts = vec![
        local("n", num(1.0)),
        Stat::ExprStat(call(name("print"), vec![name("n")])),
        assign_name("n", num(2.0)),
        Stat::ExprStat(call(name("print"), vec![name("n")])),
    ];
    inline_pure_literals(&mut stmts);
    // 4 stmts kept; first print holds literal 1; second still holds Name(n)
    assert_eq!(stmts.len(), 4, "all stmts kept; got {:?}", stmts);
    match &stmts[1] {
        Stat::ExprStat(Expr::Call { args, .. }) =>
            assert!(matches!(args[0], Expr::Number(n) if (n - 1.0).abs() < 1e-9),
                "first print should be literal 1; got {:?}", args[0]),
        _ => panic!("expected ExprStat Call"),
    }
    match &stmts[3] {
        Stat::ExprStat(Expr::Call { args, .. }) =>
            assert!(matches!(&args[0], Expr::Name(n) if n == "n"),
                "second print should still be Name(n); got {:?}", args[0]),
        _ => panic!("expected ExprStat Call"),
    }
}

// ─── Required test 13: GETIMPORT-style Field inlines via B0.45A ──

#[test]
fn b051b_getimport_style_field_handled_by_b045a_single_use() {
    // local v3 = game.Players       (Field — pure but NOT a literal)
    // svc:GetService(v3)            (single use)
    //
    // B0.45A handles single-use Field-of-Name (it's pure).
    // B0.51B is a no-op here (Field isn't a literal).
    let mut stmts = vec![
        local("v3", field(name("game"), "Players")),
        Stat::ExprStat(methodcall(name("svc"), "GetService", vec![name("v3")])),
    ];
    inline_single_use_temps(&mut stmts);
    inline_pure_literals(&mut stmts);
    // v3 inlined by B0.45A; method call now holds the Field expr.
    assert_eq!(stmts.len(), 1);
    match &stmts[0] {
        Stat::ExprStat(Expr::MethodCall { args, .. }) =>
            assert!(matches!(&args[0], Expr::Field { .. }),
                "expected Field arg; got {:?}", args[0]),
        other => panic!("expected ExprStat(MethodCall); got {:?}", other),
    }
}

// ─── Required test 14: nested block recursion ────────────────────

#[test]
fn b051b_recurses_into_nested_if_block() {
    // if cond then
    //     local v3 = "Inner"
    //     f(v3)
    // end
    //
    // Inner block should be processed independently — v3 inlined,
    // local removed.
    let mut stmts = vec![Stat::If {
        condition: Expr::Bool(true),
        then_body: vec![
            local("v3", string("Inner")),
            Stat::ExprStat(call(name("f"), vec![name("v3")])),
        ],
        elseif_clauses: vec![],
        else_body: None,
    }];
    inline_pure_literals(&mut stmts);
    match &stmts[0] {
        Stat::If { then_body, .. } => {
            assert_eq!(then_body.len(), 1,
                "nested local v3 should be removed; got {:?}", then_body);
            match &then_body[0] {
                Stat::ExprStat(Expr::Call { args, .. }) =>
                    assert!(matches!(&args[0], Expr::String(s) if s == "Inner"),
                        "expected literal; got {:?}", args[0]),
                other => panic!("expected Call; got {:?}", other),
            }
        }
        other => panic!("expected If; got {:?}", other),
    }
}

// ─── Sanity: nil literal handled ─────────────────────────────────

#[test]
fn b051b_nil_literal_inlined() {
    // local x = nil
    // f(x)
    // g(x)
    let mut stmts = vec![
        local("x", Expr::Nil),
        Stat::ExprStat(call(name("f"), vec![name("x")])),
        Stat::ExprStat(call(name("g"), vec![name("x")])),
    ];
    inline_pure_literals(&mut stmts);
    assert_eq!(stmts.len(), 2, "decl x removed; got {:?}", stmts);
    for s in &stmts {
        match s {
            Stat::ExprStat(Expr::Call { args, .. }) =>
                assert!(matches!(args[0], Expr::Nil)),
            _ => panic!("expected Call"),
        }
    }
}
