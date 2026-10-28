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
