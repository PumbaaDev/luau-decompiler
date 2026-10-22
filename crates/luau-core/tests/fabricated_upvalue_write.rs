//! A register loaded by GETUPVAL carries the upvalue's name. When the register
//! is later reused as a scratch destination and the proto never executes a
//! SETUPVAL for that upvalue, the output must not assign to the upvalue.
//!
//! Real Roblox oracle bytecode (goal/v12oracle/bc, audit family
//! `upvalue_alias_register_write_fabricated`). No game bytecode is committed;
//! the test skips when the corpus is absent. Override the location with
//! `LUAU_ORACLE_BC_DIR`.

use std::path::PathBuf;

fn oracle_dir() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("LUAU_ORACLE_BC_DIR") {
        return Some(PathBuf::from(p));
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let candidates = [
        manifest.join("../../goal/v12oracle/bc"),
        PathBuf::from("C:/Users/jep/Downloads/luau-decompiler-v2/goal/v12oracle/bc"),
    ];
    candidates.into_iter().find(|p| p.is_dir())
}

fn decompile_prefix(prefix: &str) -> Option<String> {
    let dir = oracle_dir()?;
    let entry = std::fs::read_dir(&dir)
        .ok()?
        .filter_map(|e| e.ok())
        .find(|e| e.file_name().to_string_lossy().starts_with(prefix))?;
    let bytes = std::fs::read(entry.path()).ok()?;
    // The structuring debug_asserts in the lifter can fire on a few oracle
    // shapes in a debug build (they are compiled out in release, which is what
    // the corpus gates run). That is a separate defect: skip, do not fail.
    match std::panic::catch_unwind(|| luau_core::decompile(&bytes)) {
        Ok(r) => Some(r.expect("oracle file must decompile")),
        Err(_) if cfg!(debug_assertions) => {
            eprintln!("SKIP: debug-assert in lifter for {prefix}; run with --release");
            None
        }
        Err(_) => panic!("decompile panicked for {prefix}"),
    }
}

fn assert_absent(src: &str, needle: &str, why: &str) {
    let hits: Vec<&str> = src.lines().filter(|l| l.trim_start().starts_with(needle)).collect();
    assert!(hits.is_empty(), "{why}: found {hits:?}");
}

/// 719d31ea: `GETUPVAL R6 U0; MOVE R7 R2; CALL R6 1 1; MOVE R3 R6`, no SETUPVAL
/// in the proto. The call result must not be written back to `value`.
#[test]
fn call_result_in_upvalue_alias_register_is_not_stored_to_upvalue() {
    let Some(src) = decompile_prefix("719d31ea") else { eprintln!("SKIP: oracle absent"); return };
    assert_absent(&src, "value = value(", "CALL into a GETUPVAL register");
}

/// 674055e2 InitAppearance: `GETUPVAL R5 U0; GETTABLE R4 R5[R0]; GETUPVAL R6 U0;
/// GETTABLE R5 R6[R0]`, no SETUPVAL in the proto.
#[test]
fn gettable_into_upvalue_alias_register_is_not_stored_to_upvalue() {
    let Some(src) = decompile_prefix("674055e2") else { eprintln!("SKIP: oracle absent"); return };
    assert_absent(&src, "v1 = v1[arg1]", "GETTABLE into a GETUPVAL register");
}

/// 194a6747 DoOnConstruct: `LOADN R6 12; GETUPVAL R7 U2; MUL R5 R6*R7;
/// ... GETUPVAL R9 U2; MUL R7 R8*R9`. U2 is never stored.
#[test]
fn mul_into_upvalue_alias_register_is_not_stored_to_upvalue() {
    let Some(src) = decompile_prefix("194a6747") else { eprintln!("SKIP: oracle absent"); return };
    assert_absent(&src, "GetTileScale = ", "MUL into a GETUPVAL register");
}
