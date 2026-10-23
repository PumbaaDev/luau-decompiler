//! An upvalue read into a register, then OVERWRITTEN from another register
//! while the register still holds the old value, then read through the
//! register: the register is a snapshot and must be declared as one.
//!
//! StealAnEgg `MutationConsumableFeedback` openScene, pc 0-9:
//! `GETUPVAL R4 U0; JUMPXEQKNIL R4; LOADNIL R5; SETUPVAL R5 U0;
//! GETTABLEKS R5 R4."Trove"; NAMECALL Destroy`. Printed under the upvalue's
//! name it became `v29 = nil; local t = v29.Trove`, an index of nil.

const MUTATION_FEEDBACK: &[u8] = include_bytes!("fixtures/upval_snapshot_mutation_feedback.luac");

fn norm(s: &str) -> String {
    s.lines().map(|l| l.trim_end()).collect::<Vec<_>>().join("\n")
}

#[test]
fn overwritten_upvalue_is_read_through_its_snapshot() {
    let out = norm(&crate::decompile(MUTATION_FEEDBACK).expect("fixture decompiles"));
    let want = norm(
        "local function openScene(self, arg2, arg3, arg4)
    local v292 = v29
    if v292 ~= nil then
        v29 = nil
        local CenterOffset2 = v292.Trove
        CenterOffset2:Destroy()
    end",
    );
    assert!(out.contains(&want), "missing:\n{want}\n--- in output ---\n{out}");
    assert!(!out.contains("v29 = nil\n        local CenterOffset2 = v29.Trove"));
}
