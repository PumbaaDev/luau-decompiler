//! `if A then if B then X; if c then return end else Y; if d then return end end end`
//! where the then-arm ends in a nested `if .. return` whose dead trailing JUMP
//! skips the else arm (AdoptMe FurnitureEntryHelper `get_cost`).
//!
//! Bytecode (proto get_cost): `JUMPIFNOT R2` on `IsClient()` -> else arm; the
//! then-arm ends `RETURN; JUMP -> end` (the JUMP is dead). The else arm held
//! `assert(..)` and an inlined server ownership check. The lifter emitted
//! `gamepass:IsClient()` as a bare statement and dropped the whole test, the
//! else arm and its calls (`calls_not_emitted`, 5 calls missing).

const FIX: &[u8] = include_bytes!("fixtures/cne_ifelse_dead_jump_furniture_entry.luac");

#[test]
fn is_client_test_and_else_arm_survive() {
    let out = crate::decompile(FIX).expect("fixture decompiles");
    assert!(out.contains("assert("), "else arm assert lost:\n{out}");
    assert!(out.contains("get_store"), "else arm inlined check lost:\n{out}");
    assert!(out.contains("if ") && out.contains(":IsClient()"), "{out}");
}

/// StealAnEgg `BossMastery requestClaim`: the then arm of `if milestone` is an
/// OR-chain return guard (`JUMPIF R5 -> <shared RETURN>`), which the arm lifter
/// cannot express. Rebuilding the if/else there lost the claim body (pcall,
/// InvokeServer, ...) - it must be left to the old path.
const BOSS: &[u8] = include_bytes!("fixtures/cne_ifelse_shared_return_guard_bossmastery.luac");

#[test]
fn shared_return_or_guard_arm_is_not_rebuilt_and_loses_no_calls() {
    let out = crate::decompile(BOSS).expect("fixture decompiles");
    assert!(
        !out.contains("-- call recovery:"),
        "calls were lost:\n{}",
        out.lines().take(12).collect::<Vec<_>>().join("\n")
    );
    assert!(out.contains("AskClaimMilestone"), "claim body lost");
}

/// AdoptMe TradeHub `send_offer`: the then arm of `if success` holds two
/// guards, the second a constant compare (`if dialog ~= "Okay" then return
/// false end`) that jumps straight to the merge. Rebuilding the if/else dropped
/// that test, kept `return false` unconditionally and lost the whole post
/// counter tail (`post_counter_async`, `escrow_breadcrumbs`, 8 calls).
const TRADEHUB: &[u8] = include_bytes!("fixtures/cne_then_arm_xeqk_guard_to_merge_tradehub.luac");

#[test]
fn then_arm_constant_compare_guard_to_merge_keeps_the_tail() {
    let out = crate::decompile(TRADEHUB).expect("fixture decompiles");
    assert!(out.contains("post_counter_async"), "post counter call lost:\n{out}");
    assert!(out.contains("escrow_breadcrumbs"), "escrow call lost:\n{out}");
    assert!(out.contains("~= \"Okay\""), "Okay guard test lost:\n{out}");
}
