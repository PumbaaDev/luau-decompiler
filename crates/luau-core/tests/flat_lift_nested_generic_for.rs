//! A generic-for nested in an if-arm inside an outer for-loop body must keep
//! its header when the body is lifted flat. The flat walker used to treat the
//! inner FORGPREP/FORGLOOP as no-ops, so the iterator CALL (results=3) became a
//! bare 3-value assignment and the loop variables were read unbound.
//!
//! Real Roblox bytecode (StealAnEgg SammyEvent CollectSystem, stepPulls). No
//! game bytecode is committed; the test skips when the file is absent. Override
//! the location with `LUAU_FLATFOR_NESTED_BC`.

use luau_core::{decompile_with_options, DecompileOptions, V10PlusMode};
use std::path::PathBuf;

fn bytecode() -> Option<Vec<u8>> {
    let p = std::env::var_os("LUAU_FLATFOR_NESTED_BC").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(
            "C:/Users/jep/Desktop/StealAnEgg_v12_bytecode/Players_superjepy_PlayerScripts_Game_AdminAbuseClient_Events_SammyEvent_CollectSystem.luac",
        )
    });
    std::fs::read(p).ok()
}

#[test]
fn generic_for_nested_in_if_arm_of_loop_body_keeps_its_header() {
    let Some(bytes) = bytecode() else {
        eprintln!("SKIP: bytecode absent");
        return;
    };
    let opts = DecompileOptions { v10plus: V10PlusMode::DetectorFallback, ..Default::default() };
    let src = decompile_with_options(&bytes, &opts).expect("decompile");
    assert!(
        !src.lines().any(|l| l.contains(":GetDescendants()") && l.contains(", result") && !l.contains(" in ")),
        "GetDescendants iterator CALL was flattened to a multi-assign instead of a for header"
    );
    assert!(src.contains(" in k:GetDescendants()"), "no generic-for header over GetDescendants");
}
