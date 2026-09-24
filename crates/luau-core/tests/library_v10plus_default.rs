//! The library's v10-v14 gate, observed with an untouched environment.
//!
//! Live Roblox ships v12. The library reads a v10-v14 chunk as Roblox client
//! bytecode only when a measured opcode-database entry verifies for it, and
//! REFUSES the rest by default: decoded by opcode detection under the Roblox
//! policy, stock `luau-compile` v12 (call feedback is on by default) comes out
//! as `local c = c .. c; return c` in place of a body of prints and calls.
//! The CLI decodes such chunks anyway; library callers opt into that with
//! `V10PlusMode::DetectorFallback`.
//!
//! The committed fixtures are all upstream `luau-compile` output (no game
//! bytecode is committed), so they exercise the refusal and the opt-ins. The
//! measured-hit path is covered on real client bytecode by the CLI crate's
//! `library_parity` corpus tier (`LUAU_PARITY_CORPUS`).
//!
//! Separate test binary from `v10_v14_support.rs`, which toggles the
//! process-global variable: this file must observe an untouched environment.

use luau_core::{DecompileOptions, V10PlusMode};
use std::path::PathBuf;

const UPSTREAM_V12: [&str; 3] =
    ["callheavy_v12_fboff.luac", "callheavy_v12_fbon.luac", "loop_v12.luac"];

fn fixture(name: &str) -> Vec<u8> {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests/fixtures/v10plus");
    p.push(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("reading fixture {}: {}", p.display(), e))
}

fn env_is_clean() -> bool {
    std::env::var_os("LUAU_ALLOW_V10PLUS").is_none() && std::env::var_os("LUAU_V10PLUS_RAW").is_none()
}

fn assert_refused(what: &str, name: &str, err: anyhow::Error) {
    let msg = format!("{err:#}");
    assert!(
        msg.contains("refusing Luau bytecode v12") && msg.contains("V10PlusMode::DetectorFallback"),
        "{what}({name}) must refuse with the v10+ reason and the opt-in, got: {msg}"
    );
}

#[test]
fn unmeasured_v12_is_refused_by_default() {
    if !env_is_clean() {
        eprintln!("SKIP: LUAU_ALLOW_V10PLUS / LUAU_V10PLUS_RAW exported by the caller");
        return;
    }
    for name in UPSTREAM_V12 {
        let bc = fixture(name);
        assert_eq!(bc[0], 12, "{name} must be v12");
        match luau_core::decompile(&bc) {
            Ok(src) => panic!("{name}: decoded without a measured map:\n{src}"),
            Err(e) => assert_refused("decompile", name, e),
        }
        match luau_core::decompile_with_opmap(&bc, None) {
            Ok(_) => panic!("{name}: decompile_with_opmap(None) decoded without a measured map"),
            Err(e) => assert_refused("decompile_with_opmap", name, e),
        }
        match luau_core::disassemble(&bc, true) {
            Ok(_) => panic!("{name}: disassemble listed without a measured map"),
            Err(e) => assert_refused("disassemble", name, e),
        }
        match luau_core::disassemble_with_opmap(&bc, None) {
            Ok(_) => panic!("{name}: disassemble_with_opmap listed without a measured map"),
            Err(e) => assert_refused("disassemble_with_opmap", name, e),
        }
        match luau_core::info(&bc) {
            Ok(_) => panic!("{name}: info read it without a measured map"),
            Err(e) => assert_refused("info", name, e),
        }
        assert!(env_is_clean(), "the gate must not export the v10+ variables");
    }
}

#[test]
fn detector_fallback_opts_into_the_cli_behaviour() {
    if !env_is_clean() {
        return;
    }
    let opts = DecompileOptions { v10plus: V10PlusMode::DetectorFallback, ..Default::default() };
    for name in UPSTREAM_V12 {
        let src = luau_core::decompile_with_options(&fixture(name), &opts)
            .unwrap_or_else(|e| panic!("{name}: {e:#}"));
        assert!(!src.trim().is_empty(), "{name}: empty output");
        assert!(env_is_clean(), "decompile must not export the v10+ variables");
    }
    // The bare parser keeps its conservative default outside a decompile call.
    assert!(luau_core::parser::parse(&fixture("loop_v12.luac")).is_err());
}

#[test]
fn off_defers_to_the_environment_gate() {
    if !env_is_clean() {
        return;
    }
    let opts = DecompileOptions { v10plus: V10PlusMode::Off, ..Default::default() };
    let err = luau_core::decompile_with_options(&fixture("loop_v12.luac"), &opts)
        .expect_err("with no policy and no environment, v12 must be rejected by the parser");
    assert!(format!("{err:#}").contains("LUAU_ALLOW_V10PLUS"));
}

#[test]
fn the_bundled_database_is_compiled_in() {
    // A parse failure would silently drop every client to detector-only
    // decoding and make the library refuse every live v12 script.
    let db = luau_core::bundled_opmap_db().expect("bundled opmap database must parse");
    assert!(!db.is_empty());
    assert!(
        db.entries.iter().any(|e| e.bytecode_version >= 12),
        "the bundled database must carry a v12 entry"
    );
}
