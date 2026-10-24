//! A generic-for nested in a `while true` body whose block list has pc gaps at
//! RETURN-terminated guard blocks was lifted flat: the iterator call became a
//! bare 3-value multi-assign (`import, result2, result3 = pairs(..)`), no `for`
//! header was emitted, and the loop variable was read but never bound
//! (`declared_never_assigned`), with the guard `return`s lost.
//!
//! Fixture: `fixtures/nested_for_flat_a.lua` compiled by upstream
//! `luau-compile --fflags=LuauEmitCallFeedback=false --binary -O1 -g1`. Same
//! shape as AdoptMe CharUtil `GetHumanoid`.

const FIXTURE: &[u8] = include_bytes!("fixtures/nested_for_flat_a.luac");

fn decompiled() -> String {
    crate::decompile(FIXTURE).expect("fixture decompiles")
}

#[test]
fn while_true_with_returns_around_generic_for_keeps_the_for_header() {
    let out = decompiled();
    assert!(out.contains(" in pairs(") && out.contains("for "), "for header lost:\n{out}");
    assert!(!out.contains("= pairs("), "iterator flattened to a multi-assign:\n{out}");
}

#[test]
fn while_true_with_returns_around_generic_for_keeps_every_return() {
    let out = decompiled();
    let returns = out.lines().filter(|l| l.trim_start().starts_with("return ")).count();
    // return h; return v; return nil; plus `return GetHumanoid` at module level.
    assert!(returns >= 4, "guard returns were dropped ({returns}):\n{out}");
    // The two guard returns inside the loop must be present as `return` of the
    // humanoid / the for variable (not turned into fallthrough).
    assert!(out.contains("return Humanoid\n") && out.contains("return v\n"), "{out}");
}

// Variant (b): the for sits in an if-arm and its body holds a lone `JUMP`
// (the `else break`) that the arm's block list leaves out, so the arm falls into
// two runs with FORGPREP in one and FORGLOOP in the other.
const FIXTURE_B: &[u8] = include_bytes!("fixtures/nested_for_flat_b.luac");

#[test]
fn if_arm_for_with_break_block_gap_keeps_the_for_header() {
    let out = crate::decompile(FIXTURE_B).expect("fixture decompiles");
    assert!(out.contains("in ipairs("), "for header lost:\n{out}");
    assert!(!out.contains("= ipairs("), "iterator flattened to a multi-assign:\n{out}");
    assert!(out.contains("break"), "the else-break was lost:\n{out}");
}

// Variant (c): a rotated `while true do for .. end; if done then break end; step() end`
// inside an if-arm. The for was emitted, then the whole loop re-lifted flat.
const FIXTURE_C: &[u8] = include_bytes!("fixtures/nested_for_flat_c.luac");

#[test]
fn rotated_while_true_with_for_is_not_lifted_twice() {
    let out = crate::decompile(FIXTURE_C).expect("fixture decompiles");
    assert!(!out.contains("= ipairs("), "flat copy of the for survived:\n{out}");
    assert_eq!(out.matches("ipairs(").count(), 1, "for lifted more than once:\n{out}");
    assert_eq!(out.matches("table.insert(").count(), 1, "body duplicated:\n{out}");
    assert!(out.contains("while true do"), "loop shape lost:\n{out}");
    assert!(out.contains("AdvanceToNextPageAsync"), "step call lost:\n{out}");
}

// Outer-local write: a local declared BEFORE a loop is assigned in both arms of
// a diamond inside a generic-for and read only after the loop (iterations that
// `continue` or `break` leave it alone). The arm-write seeds took the post-loop
// read for a merge read and minted a fresh `local sel<N>` for the arm writes, so
// the outer local never saw them. Same shape as a BloxFruits spawn-point script.
const FIXTURE_OUTER: &[u8] = include_bytes!("fixtures/nested_for_outer_write.luac");

#[test]
fn for_arm_writes_to_outer_local_are_not_redeclared() {
    let out = crate::decompile(FIXTURE_OUTER).expect("fixture decompiles");
    assert!(out.contains("in tbl.get("), "for header lost:\n{out}");
    assert!(!out.contains("local sel"), "arm write minted a fresh local:\n{out}");
    let assigns = out.lines().filter(|l| l.trim_start().starts_with("Key = ")).count();
    assert_eq!(assigns, 3, "writes to the outer local lost:\n{out}");
    assert!(out.contains("print(Key)"), "post-loop read of the outer local lost:\n{out}");
}
