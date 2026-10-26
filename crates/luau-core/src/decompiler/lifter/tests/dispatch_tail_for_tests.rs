//! A generic-for in the LAST arm of a string-dispatch chain inside a for-loop
//! body was lifted flat: the Linear span holding the chain's `JUMPXEQKS` tests
//! escapes its own end (the arms end in an unconditional JUMP over the next
//! case), the escaped span is lifted flat up to the loop end, and the flat walker
//! drops FORGPREP/FORGLOOP. The iterator CALL was left as a bare 3-value assign
//! (`a, b, c = pairs(t)`) and the counting loop body vanished.
//!
//! Fixture: BloxFruits `ReplicatedStorage.Util.TweenModelAnubis` (v12, bundled
//! measured opmap). The counted loop is `for _ in pairs(tbl) do n += 1 end`.

const FIXTURE: &[u8] = include_bytes!("fixtures/dispatch_tail_for_tween_model.luac");

fn decompiled() -> String {
    crate::decompile(FIXTURE).expect("fixture decompiles")
}

#[test]
fn dispatch_tail_generic_for_keeps_its_for_header() {
    let out = decompiled();
    assert!(out.contains(" in pairs(tbl17) do"), "for header lost:\n{out}");
    assert!(!out.contains("= pairs("), "iterator flattened to a multi-assign:\n{out}");
}

#[test]
fn dispatch_tail_generic_for_keeps_its_body_counter() {
    let out = decompiled();
    assert!(out.contains("CFrame_21 += 1"), "loop body (the counter) lost:\n{out}");
}

// Variant: `repeat <nested generic-for> until not changed` inside an if-arm.
// The structurer consumed the for first and then met the JUMPBACK latch with
// the loop header already behind it, so the body was lifted a second time flat
// inside a `while changed do` (bare `a, b, c = pairs(t)` assigns, loop
// variables never bound, first iteration emitted twice).
//
// Fixture: `fixtures/repeat_until_for_body.lua` compiled by upstream
// `luau-compile --fflags=LuauEmitCallFeedback=false --binary -O1 -g1`. Same
// shape as RIVALS ChattingController `get_text_chat_message_properties`.
const REPEAT_FIXTURE: &[u8] = include_bytes!("fixtures/repeat_until_for_body.luac");

#[test]
fn repeat_until_with_leading_generic_for_is_one_repeat_loop() {
    let out = crate::decompile(REPEAT_FIXTURE).expect("fixture decompiles");
    assert!(out.contains("repeat\n"), "repeat loop lost:\n{out}");
    assert!(out.contains("until not "), "until test lost:\n{out}");
    assert!(!out.contains("while "), "tail test rebuilt as a while:\n{out}");
    assert!(!out.contains("= pairs("), "iterator flattened to a multi-assign:\n{out}");
    assert_eq!(out.matches("in pairs(").count(), 2, "for loops duplicated or lost:\n{out}");
    assert_eq!(out.matches("string.find(").count(), 1, "body duplicated:\n{out}");
}
