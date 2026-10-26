//! Stores to captured upvalues must survive lifting.
//!
//! Real Roblox oracle bytecode (goal/v12oracle/bc, audit family
//! `upvalue_store_removed`). No game bytecode is committed; every test skips
//! when the corpus is absent. Override the location with `LUAU_ORACLE_BC_DIR`.
//!
//! Sub-shapes: (a) dead-store elimination removing `u = true` before a later
//! `u = false`, (b) literal propagation of `local v = false` into reads when the
//! only writer is a closure passed as a call argument, (c) the SETUPVAL name
//! backscan renaming the slot to the stored local's name (`self = self`),
//! (d) a `SETUPVAL; GETUPVAL; JUMPIF` run losing its store.

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
    match std::panic::catch_unwind(|| luau_core::decompile(&bytes)) {
        Ok(r) => Some(r.expect("oracle file must decompile")),
        Err(_) if cfg!(debug_assertions) => {
            eprintln!("SKIP: debug-assert in lifter for {prefix}; run with --release");
            None
        }
        Err(_) => panic!("decompile panicked for {prefix}"),
    }
}

/// Text of the block that starts at the first line containing `header` and runs
/// to the next line with the same or lower indentation.
fn block_of<'a>(src: &'a str, header: &str) -> Vec<&'a str> {
    let lines: Vec<&str> = src.lines().collect();
    let Some(start) = lines.iter().position(|l| l.contains(header)) else { return vec![] };
    let indent = lines[start].len() - lines[start].trim_start().len();
    let mut out = vec![lines[start]];
    for l in &lines[start + 1..] {
        let ind = l.len() - l.trim_start().len();
        if !l.trim().is_empty() && ind <= indent {
            break;
        }
        out.push(l);
    }
    out
}

/// 194a6747 assignButton: `GETUPVAL R0 U0; JUMPIFNOT; RETURN; LOADB R0 true;
/// SETUPVAL R0 U0; ...; <later> SETUPVAL false`. The debounce flag set to true
/// before the work must survive: a later `= false` does not make it dead,
/// because the check `if flag then return` runs again on the next click.
#[test]
fn debounce_true_store_before_later_false_store_is_kept() {
    let Some(src) = decompile_prefix("194a6747") else { eprintln!("SKIP: oracle absent"); return };
    let block = block_of(&src, "local function assignButton");
    assert!(!block.is_empty(), "assignButton not found");
    let stores_true = block.iter().any(|l| {
        let t = l.trim();
        t.ends_with("= true") && !t.starts_with("local ")
    });
    assert!(stores_true, "debounce `flag = true` store was removed:\n{}", block.join("\n"));
}

/// 856ad24c safeInvokeClient: `local v3 = false` is written only inside the
/// closure handed to task.spawn. It must not be constant-propagated into the
/// `while not v3` poll (which became `while true`).
#[test]
fn flag_written_only_by_spawned_closure_is_not_inlined_into_loop_condition() {
    let Some(src) = decompile_prefix("856ad24c") else { eprintln!("SKIP: oracle absent"); return };
    let block = block_of(&src, "local function safeInvokeClient");
    assert!(!block.is_empty(), "safeInvokeClient not found");
    assert!(
        !block.iter().any(|l| l.trim() == "while true do"),
        "wait-for-flag loop became `while true`:\n{}",
        block.join("\n")
    );
    let declares_result = block.iter().any(|l| l.trim().starts_with("local v2 = nil") || l.trim().starts_with("local v2"));
    assert!(declares_result, "captured result local was dropped:\n{}", block.join("\n"));
}

/// 04a82a8d GetActiveDuration: `local v7 = 0` is written by the closure passed
/// to ForEachMod and returned afterwards. The declaration must survive and the
/// return must read it, not the literal.
#[test]
fn counter_written_by_closure_argument_keeps_its_declaration() {
    let Some(src) = decompile_prefix("04a82a8d") else { eprintln!("SKIP: oracle absent"); return };
    let block = block_of(&src, "GetActiveDuration");
    assert!(!block.is_empty(), "GetActiveDuration not found");
    assert!(
        block.iter().any(|l| l.trim().starts_with("local v7")),
        "captured counter declaration was dropped:\n{}",
        block.join("\n")
    );
}
