//! A generic-for inside the else arm of an if/elseif chain whose first Linear
//! span is extended by `linear_escape_else_target` must still be lifted as a
//! loop: the flat lifter used to drop FORGPREP/FORGLOOP, emitting the iterator
//! CALL as a bare 3-value local and reading never-bound loop variables.
//!
//! Real Roblox bytecode (sleitnick log module, SetLogLevel). No game bytecode
//! is committed; the test skips when the file is absent. Override the location
//! with `LUAU_FLATFOR_BC`.

use luau_core::{decompile_with_options, DecompileOptions, V10PlusMode};
use std::path::PathBuf;

fn bytecode() -> Option<Vec<u8>> {
    let p = std::env::var_os("LUAU_FLATFOR_BC").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(
            "C:/Users/jep/Desktop/StealAnEgg_v12_bytecode/ReplicatedStorage_Packages__Index_sleitnick_log_0_1_1_log.luac",
        )
    });
    std::fs::read(p).ok()
}

#[test]
fn generic_for_in_extended_else_arm_is_lifted_as_a_loop() {
    let Some(bytes) = bytecode() else {
        eprintln!("SKIP: bytecode absent");
        return;
    };
    let opts = DecompileOptions { v10plus: V10PlusMode::DetectorFallback, ..Default::default() };
    let src = decompile_with_options(&bytes, &opts).expect("decompile");
    assert!(
        !src.contains("= pairs(\"Debug\")"),
        "iterator CALL was flattened to a local instead of a for header"
    );
    assert!(src.contains(" in pairs("), "no generic-for header emitted");
}
