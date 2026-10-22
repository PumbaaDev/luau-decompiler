//! A v10+ chunk decoded by detectors alone must fail loudly.
//!
//! With no measured opcode-database entry the detectors pin opcodes from
//! structure and mis-assign core ones; the output then looks plausible (string
//! constants as integers, undefined `vN`, duplicated local functions) while the
//! header used to report "98% of instruction words" pinned. The fallback must
//! either refuse, or carry an UNTRUSTED banner and never print a percentage.
//!
//! The v14 chunks are live game bytecode and are not committed. Point
//! `LUAU_V14_LIVE_DIR` at a folder of `.luac` files (default: `goal/v14_live`
//! at the repo root); the test skips when the folder is absent.

use luau_core::{DecompileOptions, V10PlusMode};
use std::path::PathBuf;

fn live_dir() -> Option<PathBuf> {
    let dir = std::env::var_os("LUAU_V14_LIVE_DIR").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../goal/v14_live")
    });
    dir.is_dir().then_some(dir)
}

#[test]
fn unmeasured_v14_fallback_refuses_or_carries_untrusted_banner() {
    if std::env::var_os("LUAU_ALLOW_UNTRUSTED_DECODE").is_some() {
        return;
    }
    let Some(dir) = live_dir() else {
        eprintln!("SKIP: no v14 live corpus (set LUAU_V14_LIVE_DIR)");
        return;
    };
    let opts = DecompileOptions { v10plus: V10PlusMode::DetectorFallback, ..Default::default() };
    let measured = DecompileOptions { v10plus: V10PlusMode::MeasuredOnly, ..Default::default() };
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).expect("read live dir").flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("luac") {
            continue;
        }
        let bc = std::fs::read(&path).expect("read bytecode");
        if bc.first().is_none_or(|&v| v < 10) {
            continue;
        }
        checked += 1;
        // A chunk the measured database verifies is decoded exactly, with no
        // banner (ShrineFusionController since the roblox_v14_live entry).
        if luau_core::decompile_with_options(&bc, &measured).is_ok() {
            continue;
        }
        match luau_core::decompile_with_options(&bc, &opts) {
            Err(e) => {
                let msg = format!("{e:#}");
                assert!(msg.contains("refusing Luau bytecode"), "{}: {msg}", path.display());
            }
            Ok(src) => {
                assert!(src.contains("UNTRUSTED DECODE"), "{}: no untrusted banner", path.display());
                assert!(
                    !src.contains("of instruction words"),
                    "{}: still reports a pinned percentage",
                    path.display()
                );
            }
        }
    }
    assert!(checked > 0, "no v10+ .luac files under {}", dir.display());
}
