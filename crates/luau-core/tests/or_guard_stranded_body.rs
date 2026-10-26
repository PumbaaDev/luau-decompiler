//! A stranded `if a or b or c:IsA("T") then <body> return end` guard must be
//! lifted with its body.
//!
//! The if/else structurer visits blocks in reverse postorder and used to emit
//! such a body AFTER the continuation's trailing return, where dead-code
//! elimination silently deleted it (calls vanished, `calls_not_emitted`). The
//! OR-guard detector declined three shapes: an entry block with several
//! predecessors (a `for` loop body head), a link holding one call (`IsA`), and
//! a body with an inner short-circuit. A call link must fold into the `or`
//! operand rather than split the chain, which would lift the body twice and
//! duplicate its calls.
//!
//! Real Roblox bytecode (the ni_bc_2026-10-25 corpus). No game bytecode is
//! committed; each test skips when its file is absent. Override the directory
//! with `LUAU_NI_BC_DIR`.

use luau_core::{decompile_with_options, DecompileOptions, V10PlusMode};
use std::path::PathBuf;

const RECOVERY_MARKER: &str = "call recovery:";

fn decompile_corpus_file(name: &str) -> Option<String> {
    let dir = std::env::var_os("LUAU_NI_BC_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("C:/Users/jep/AppData/Local/Potassium/workspace/ni_bc_2026-10-25"));
    let bytes = std::fs::read(dir.join(name)).ok()?;
    let opts = DecompileOptions { v10plus: V10PlusMode::DetectorFallback, ..Default::default() };
    Some(decompile_with_options(&bytes, &opts).expect("decompile"))
}

fn assert_no_dropped_calls(src: &str, what: &str) {
    assert!(!src.contains(RECOVERY_MARKER), "{what}: calls were dropped from the output");
}

#[test]
fn tiers_for_loop_guard_keeps_its_body() {
    let Some(src) = decompile_corpus_file("ReplicatedStorage.Shared.Modules.Tiers__49b6cbdf.luac")
    else {
        eprintln!("SKIP: bytecode absent");
        return;
    };
    assert_no_dropped_calls(&src, "Tiers");
}

#[test]
fn duration_inner_short_circuit_guard_keeps_its_body() {
    let Some(src) =
        decompile_corpus_file("ReplicatedStorage.CmdrClient.Types.Duration__fb6bbecc.luac")
    else {
        eprintln!("SKIP: bytecode absent");
        return;
    };
    assert_no_dropped_calls(&src, "Duration");
    assert!(src.contains("MakeFuzzyFinder(\"\")"), "guard body call missing");
}

#[test]
fn coral_spot_call_link_guard_folds_into_condition() {
    let Some(src) = decompile_corpus_file(
        "Players.PLAYER.PlayerScripts.Client.Components._mediumModel.Com_CoralSpot__72dec5c8.luac",
    ) else {
        eprintln!("SKIP: bytecode absent");
        return;
    };
    assert_no_dropped_calls(&src, "CoralSpot");
    assert!(
        src.contains("not ModelName or not ModelName:IsA(\"PVInstance\")"),
        "IsA link was not folded into the or-chain"
    );
}

#[test]
fn call_link_guard_does_not_duplicate_the_body() {
    let Some(src) =
        decompile_corpus_file("ReplicatedStorage.Shared.Librairies.CollectFlight__5e89aa4a.luac")
    else {
        eprintln!("SKIP: bytecode absent");
        return;
    };
    // Splitting the chain at the IsA link lifted the guard body twice
    // (onCancel/onFinish callbacks and Disconnect repeated).
    assert_eq!(src.matches("tbl3.onCancel(").count(), 1, "onCancel call duplicated");
    assert_eq!(src.matches("tbl3.onFinish(").count(), 3, "onFinish call duplicated");
}
