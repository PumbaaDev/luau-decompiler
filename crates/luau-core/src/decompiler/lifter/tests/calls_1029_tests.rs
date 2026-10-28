//! `calls_not_emitted` fixes of 2026-10-29.

/// AdoptMe `MoneyTreeUseHelper.render`: a `repeat` whose if/elseif/else arms
/// each JUMP to a mid-body tail (`task.wait(3)` and the exit tests). Per-block
/// lifting read every arm JUMP as `continue`, so the arms (six calls) and the
/// tail vanished.
const MONEY: &[u8] = include_bytes!("fixtures/calls1029_while_true_join_tail_moneytree.luac");

#[test]
fn while_true_arms_joining_a_mid_body_tail_keep_their_calls() {
    let out = crate::decompile(MONEY).expect("fixture decompiles");
    assert!(!out.contains("call recovery:"), "calls still lost:\n{out}");
    let start = out.find("local function fn()").expect("loop function present");
    let body = &out[start..];
    assert_eq!(body.matches("set_model_transparency(").count(), 6, "arm calls lost:\n{out}");
    assert!(body.contains("task.wait(3)"), "tail wait lost:\n{out}");
    assert!(!body.contains("continue"), "arm JUMPs still read as continue:\n{out}");
}

/// BloxFruits `AnimateEntrance`: an inlined boolean helper
/// (`a and f(b) and not c`) feeding `if not ok then return end` compiles to
/// `LOADB true; JUMP; LOADB false; JUMPIF R +1; RETURN`. The calls inside the
/// chain made it an impure value join, so the test read as the proto's final
/// return and the rest of each touch handler (about 20 calls each) was dropped.
const ENTRANCE: &[u8] = include_bytes!("fixtures/calls1029_inlined_bool_guard_animateentrance.luac");

#[test]
fn inlined_boolean_guard_becomes_return_guards_and_keeps_the_body() {
    let out = crate::decompile(ENTRANCE).expect("fixture decompiles");
    assert!(!out.contains("call recovery:"), "calls still lost:\n{out}");
    assert!(out.contains("AddTag(LocalPlayer, \"Teleporting\")"), "handler body lost:\n{out}");
    assert!(
        out.contains("if not Busy7 then\n") && out.contains("if Value4 then\n"),
        "guards not rebuilt as return guards:\n{out}"
    );
}
