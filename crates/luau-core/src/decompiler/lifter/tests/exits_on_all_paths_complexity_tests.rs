//! Regression guard for the `exits_on_all_paths` exponential blow-up.
//!
//! `eliminate_dead_code` Phase 2 calls `exits_on_all_paths` to decide whether a
//! statement list unconditionally exits. The original implementation, on a bare
//! `if C then return end` guard (no else), did:
//!
//! ```ignore
//! if exits_on_all_paths(&stmts[idx + 1..]) { return true; }
//! // ...then fall through and let the outer `for` loop re-scan idx+1..
//! ```
//!
//! Both the recursive call AND the continuing loop walked the same suffix, so a
//! chain of guards re-analysed each suffix `2^k` times: `W(m) = 2·W(m-1)+P`. A
//! live v12 `abilityLocal` proto (741 nodes, built almost entirely from
//! `if cond then return end` walls) drove `exits_on_all_paths` past **100 million
//! calls** and hung the decompiler. The fix returns the suffix result directly
//! (`return exits_on_all_paths(&stmts[idx + 1..])`), which is exactly equivalent
//! — a fall-through guard-if makes the list exit-on-all-paths iff its suffix does
//! — and O(n).
//!
//! These tests pin BOTH properties: the boolean result is unchanged on the
//! shapes the `return`-vs-fall-through rewrite could plausibly affect, and a
//! deep guard chain (which the old code could not finish) completes.

use crate::ast::{Expr, Stat};

/// `if <true> then <then_body> end` with no elseif and no else.
fn guard(then_body: Vec<Stat>) -> Stat {
    Stat::If {
        condition: Expr::Bool(true),
        then_body,
        elseif_clauses: Vec::new(),
        else_body: None,
    }
}

fn ret() -> Stat {
    Stat::Return { values: Vec::new() }
}

/// A statement that never exits, used as a fall-through tail so a list does NOT
/// exit on all paths.
fn non_exit() -> Stat {
    Stat::ExprStat(Expr::Bool(true))
}

#[test]
fn exit_predicate_matches_expected_on_guard_shapes() {
    let eoap = super::super::exits_on_all_paths;

    // A bare return exits.
    assert!(eoap(&[ret()]));
    // A bare guard falls through on the false branch -> does NOT exit.
    assert!(!eoap(&[guard(vec![ret()])]));
    // Guard followed by an unconditional return -> exits (both paths).
    assert!(eoap(&[guard(vec![ret()]), ret()]));
    // Guard followed by a non-exiting tail -> does NOT exit.
    assert!(!eoap(&[guard(vec![ret()]), non_exit()]));
    // Two guards then a return -> exits.
    assert!(eoap(&[guard(vec![ret()]), guard(vec![ret()]), ret()]));
    // Two guards then a non-exiting tail -> does NOT exit.
    assert!(!eoap(&[guard(vec![ret()]), guard(vec![ret()]), non_exit()]));
    // if/else where both branches exit -> exits.
    assert!(eoap(&[Stat::If {
        condition: Expr::Bool(true),
        then_body: vec![ret()],
        elseif_clauses: Vec::new(),
        else_body: Some(vec![ret()]),
    }]));
    // if/else where the else does NOT exit -> does NOT exit.
    assert!(!eoap(&[Stat::If {
        condition: Expr::Bool(true),
        then_body: vec![ret()],
        elseif_clauses: Vec::new(),
        else_body: Some(vec![non_exit()]),
    }]));
    // Empty list never exits.
    assert!(!eoap(&[]));
}

#[test]
fn deep_guard_chain_terminates() {
    // 200 stacked `if true then return end` guards then a non-exiting tail.
    // The old suffix-rescanning code was O(2^n): 2^200 calls is not finishable
    // in the age of the universe, so if this ever regresses the test binary
    // hangs (a CI timeout), not just fails. The fixed code is O(n).
    let mut stmts: Vec<Stat> = (0..200).map(|_| guard(vec![ret()])).collect();
    stmts.push(non_exit());

    // Ends in a fall-through, so it does NOT exit on all paths.
    assert!(!super::super::exits_on_all_paths(&stmts));

    // And the all-exit variant (trailing return) resolves to true.
    let mut all_exit: Vec<Stat> = (0..200).map(|_| guard(vec![ret()])).collect();
    all_exit.push(ret());
    assert!(super::super::exits_on_all_paths(&all_exit));
}
