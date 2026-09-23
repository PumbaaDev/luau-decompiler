//! `luau_core::decompile` decodes v10-v14 by default, with NO environment
//! variable set, and without setting one.
//!
//! Before this lock the library rejected every v12 chunk ("Set
//! LUAU_ALLOW_V10PLUS=1 ...") unless the host process exported the variable,
//! while the CLI enabled it for itself. Live Roblox is v12, so `luau-wasm`,
//! `luau-server` and the C ABI refused the live client's scripts outright.
//!
//! Separate test binary from `v10_v14_support.rs`, which toggles the
//! process-global variable: this file must observe an untouched environment.

use std::path::PathBuf;

fn fixture(name: &str) -> Vec<u8> {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests/fixtures/v10plus");
    p.push(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("reading fixture {}: {}", p.display(), e))
}

fn env_is_clean() -> bool {
    std::env::var_os("LUAU_ALLOW_V10PLUS").is_none() && std::env::var_os("LUAU_V10PLUS_RAW").is_none()
}

#[test]
fn library_decodes_v12_without_touching_the_environment() {
    if !env_is_clean() {
        eprintln!("SKIP: LUAU_ALLOW_V10PLUS / LUAU_V10PLUS_RAW exported by the caller");
        return;
    }
    for name in ["callheavy_v12_fboff.luac", "callheavy_v12_fbon.luac", "loop_v12.luac"] {
        let bc = fixture(name);
        assert_eq!(bc[0], 12, "{name} must be v12");
        let src = luau_core::decompile(&bc).unwrap_or_else(|e| panic!("{name}: {e:#}"));
        assert!(!src.trim().is_empty(), "{name}: empty output");
        assert!(env_is_clean(), "decompile must not export the v10+ variables");
    }
    // The bare parser keeps its conservative default outside a decompile call.
    assert!(luau_core::parser::parse(&fixture("loop_v12.luac")).is_err());
}

#[test]
fn v10plus_policy_can_be_turned_off() {
    if !env_is_clean() {
        return;
    }
    let opts = luau_core::DecompileOptions { roblox_v10plus: false, ..Default::default() };
    let err = luau_core::decompile_with_options(&fixture("loop_v12.luac"), &opts)
        .expect_err("with the policy off, v12 must be rejected as before");
    assert!(format!("{err:#}").contains("LUAU_ALLOW_V10PLUS"));
}

#[test]
fn database_can_be_supplied_or_disabled() {
    // Bundled DB is present and parses (it is compiled in; a parse failure
    // would silently drop every client to detector-only decoding).
    let db = luau_core::bundled_opmap_db().expect("bundled opmap database must parse");
    assert!(!db.is_empty());
    // Upstream-compiled fixtures match no Roblox entry, so every choice must
    // take the same detector path here (the corpus tier of the CLI crate's
    // `library_parity` test exercises the Hit path on real client bytecode).
    let bc = fixture("loop_v12.luac");
    let base = luau_core::decompile(&bc).unwrap();
    for choice in [
        luau_core::OpmapDbChoice::Disabled,
        luau_core::OpmapDbChoice::Custom(db),
        luau_core::OpmapDbChoice::Bundled,
    ] {
        let opts = luau_core::DecompileOptions { opmap_db: choice, ..Default::default() };
        assert_eq!(luau_core::decompile_with_options(&bc, &opts).unwrap(), base, "{choice:?}");
    }
}
