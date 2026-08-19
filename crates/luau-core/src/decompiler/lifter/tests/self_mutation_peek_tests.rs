//! The rebind peek must not fire on self-mutation.
//!
//! `store_complex` asks `ctx.reg_name` for a fresh hint on every write. When
//! the uniquifier has bumped its counter the hint differs (`Lvl3` -> `Lvl4`),
//! which used to read as a semantic rebind and emit
//! `local Lvl4 = Lvl3 + 2` inside a branch arm - so the value never reached the
//! `Lvl3` used after it. 213_Tornado lost a `+ 2` that way.
//!
//! The predicate that separates an accumulate from a rebind is pinned here.
//! Two earlier attempts failed on exactly the cases these tests fix in place:
//!
//!   * comparing NAME STEMS collapsed `max3`/`max6`/`max9` in RootCamera's
//!     rk4Integrator into one `max`, summing one value four times;
//!   * accepting ANY expression that mentions the name rewrote
//!     `local VREnabled6 = VRService.VREnabled` into
//!     `VRService = VRService.VREnabled`, destroying the service reference.
//!
//! Only a BinOp whose operand is the carried name is an accumulate. This is the
//! same predicate `check_branch_local_discarded` uses to RECOGNISE the family,
//! so the checker and the fix agree on what the defect is.

use crate::ast::{BinOp, Expr};
use crate::decompiler::lifter::expr_references_name;

/// Mirrors the guard in `store_complex`.
fn is_accumulate(value: &Expr, carried: &str) -> bool {
    matches!(value, Expr::BinOp { .. }) && expr_references_name(value, carried)
}

fn name(n: &str) -> Expr {
    Expr::Name(n.to_string())
}

fn binop(l: Expr, op: BinOp, r: Expr) -> Expr {
    Expr::BinOp { op, left: Box::new(l), right: Box::new(r) }
}

#[test]
fn arithmetic_on_itself_is_an_accumulate() {
    // `Lvl3 = Lvl3 + 2`
    let v = binop(name("Lvl3"), BinOp::Add, Expr::Number(2.0));
    assert!(is_accumulate(&v, "Lvl3"));
}

#[test]
fn concat_onto_itself_is_an_accumulate() {
    // `Op = Op .. " Bees)"` - 734_StatModifiers_BaseConversionRate
    let v = binop(name("Op"), BinOp::Concat, Expr::String(" Bees)".into()));
    assert!(is_accumulate(&v, "Op"));
}

#[test]
fn nested_reference_still_counts() {
    // `Op = (Op .. " (") .. Color`
    let inner = binop(name("Op"), BinOp::Concat, Expr::String(" (".into()));
    let v = binop(inner, BinOp::Concat, name("Color"));
    assert!(is_accumulate(&v, "Op"));
}

#[test]
fn a_field_read_of_itself_is_not_an_accumulate() {
    // `VRService.VREnabled` NARROWS the service into a bool; the service is
    // still needed. Accepting this destroyed 25_RootCamera.
    let v = Expr::Field { object: Box::new(name("VRService")), field: "VREnabled".into() };
    assert!(!is_accumulate(&v, "VRService"));
}

#[test]
fn arithmetic_on_a_different_name_is_not_an_accumulate() {
    // `max5 = arg2 + max` reads ANOTHER register's value.
    let v = binop(name("arg2"), BinOp::Add, name("max"));
    assert!(!is_accumulate(&v, "max5"));
}

#[test]
fn a_fresh_value_is_not_an_accumulate() {
    let v = binop(Expr::Number(1.0), BinOp::Add, Expr::Number(2.0));
    assert!(!is_accumulate(&v, "Lvl3"));
}

// ── Accumulating through a MOVE alias ─────────────────────────────────────
//
// Luau's CONCAT needs consecutive operand registers, so `desc = desc .. x`
// first MOVEs the accumulator into a scratch register. The arm then reads
//
//     Pool = Name2                              -- the MOVE
//     local Name3 = (Pool .. self.Pool) .. " "  -- extends the SCRATCH name
//
// so the direct guard cannot see the accumulate. 819_StatReqs lost its pool
// name this way; 748_CogsPerRound had its Add branch return a fresh local
// while its Mul branch, which needs no alias, correctly wrote back to `Op`.

use crate::ast::Stat;

/// Mirrors the alias arm of the guard in `store_complex`.
fn is_accumulate_via_alias(prev: Option<&Stat>, value: &Expr, carried: &str) -> bool {
    if !matches!(value, Expr::BinOp { .. }) {
        return false;
    }
    if let Some(Stat::Assign { targets, values }) = prev {
        if let (Some(Expr::Name(alias)), Some(Expr::Name(src))) =
            (targets.first(), values.first())
        {
            return src == carried && expr_references_name(value, alias);
        }
    }
    false
}

fn move_stmt(dst: &str, src: &str) -> Stat {
    Stat::Assign { targets: vec![name(dst)], values: vec![name(src)] }
}

#[test]
fn a_concat_through_a_move_alias_is_an_accumulate() {
    // Pool = Name2 ; Name2 = (Pool .. x) .. " "
    let prev = move_stmt("Pool", "Name2");
    let v = binop(binop(name("Pool"), BinOp::Concat, name("x")), BinOp::Concat, name("sp"));
    assert!(is_accumulate_via_alias(Some(&prev), &v, "Name2"));
}

#[test]
fn an_alias_of_a_different_register_does_not_count() {
    // The MOVE copied something else, so the extension is not ours.
    let prev = move_stmt("Pool", "Unrelated");
    let v = binop(name("Pool"), BinOp::Concat, name("x"));
    assert!(!is_accumulate_via_alias(Some(&prev), &v, "Name2"));
}

#[test]
fn without_a_preceding_move_there_is_no_alias() {
    let v = binop(name("Pool"), BinOp::Concat, name("x"));
    assert!(!is_accumulate_via_alias(None, &v, "Name2"));
}

#[test]
fn a_non_binop_through_an_alias_still_does_not_count() {
    // `X = X.field` narrows; the alias must not smuggle that past the BinOp gate.
    let prev = move_stmt("Pool", "Name2");
    let v = Expr::Field { object: Box::new(name("Pool")), field: "Size".into() };
    assert!(!is_accumulate_via_alias(Some(&prev), &v, "Name2"));
}
