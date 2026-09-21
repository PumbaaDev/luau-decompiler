//! CALL must be assigned, and must not be stolen by CAPTURE.
//!
//! ── THE BUG ─────────────────────────────────────────────────────────────
//! `CameraModule` (32 protos) decompiled to ~10 lines ending `return {}` —
//! the whole module body gone. The lifter was blameless; it faithfully lifted
//! a corrupted instruction stream.
//!
//! In that chunk's detected opcode map, `CALL` was **never assigned at all**,
//! while `0x9F` was assigned to `CAPTURE`. Proof that `0x9F` is really `CALL`
//! comes from inside the chunk, structurally:
//!
//!   instr 30-35:  GETIMPORT(game) -> LOADK("Players")
//!                 -> GETTABLEKS("GetService") -> 0x9F
//!                 i.e. `game:GetService("Players")`; slot four can only be CALL.
//!   instr 342-344: 0x9F immediately after NAMECALL. In Luau, NAMECALL is
//!                 ALWAYS followed by CALL.
//!
//! `CAPTURE` may legally follow only NEWCLOSURE/DUPCLOSURE. Neither site
//! qualifies, so the assignment is impossible rather than merely unlikely.
//!
//! ── WHY IT HAPPENED ─────────────────────────────────────────────────────
//!   1. `detect_closure_capture` force-assigns CAPTURE on `count >= 1` —
//!      a single coincidental match wins the byte.
//!   2. Its guard is only "A <= 2 and every capture shares one opcode byte".
//!      CALL satisfies that routinely: the function register is usually a
//!      low slot. The comment claims CAPTURE "always follows NEWCLOSURE" but
//!      the code never checks that the preceding instruction IS a closure op —
//!      it infers closure and capture from the same weak pattern.
//!   3. Ordering seals it: `detect_closure_capture` is at opmap.rs:1588,
//!      `detect_call` at opmap.rs:2463. `detect_call` skips already-mapped
//!      bytes (`if ctx.is_mapped(op) { continue; }`), so its proper
//!      C-distribution discriminant — which would reject 0x9F — never runs.
//!
//! A module body is overwhelmingly calls. Decode every call as a no-op and
//! the body evaporates.
//!
//! ── WHAT THIS TEST ASSERTS ──────────────────────────────────────────────
//! Behaviour, not implementation: for real chunks, CALL must be assigned, and
//! the decompiled body must contain actual calls. It does not pin the byte
//! value, so it stays valid across client builds with different shuffles.

use std::path::{Path, PathBuf};

// Bytecode dir comes from the BC_CORPUS env var; the test skips when unset.

/// Decompile with the measured opmap database when it covers this chunk,
/// exactly as `luau-cli` does. Falls back to detector inference only when the
/// database has no entry, which is the same order of precedence the CLI uses.
fn decompile_like_product(bytes: &[u8]) -> Option<String> {
    let db_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../opmap_db/roblox_v9_live.json");
    if let Ok((db, _warnings)) = luau_core::parser::opmap_db::OpmapDb::load(&db_path) {
        if let Ok(chunk) = luau_core::parser::parse(bytes) {
            if let Some(entry_id) = match db.lookup(&chunk) {
                luau_core::parser::opmap_db::DbLookup::Hit { ref entry_id, .. } => {
                    Some(entry_id.clone())
                }
                _ => None,
            } {
                if let Some(entry) = db.get(&entry_id).cloned() {
                    if let Ok((src, _)) = luau_core::decompile_with_plan(
                        bytes,
                        &luau_core::DecodePlan { prior: None, exact: Some(&entry) },
                    ) {
                        return Some(src);
                    }
                }
            }
        }
    }
    luau_core::decompile(bytes).ok()
}

fn bytecode_files() -> Vec<PathBuf> {
    let bc = std::env::var("BC_CORPUS").unwrap_or_default();
    let dir = Path::new(&bc);
    if !dir.exists() {
        return Vec::new();
    }
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "bin"))
        .collect();
    v.sort();
    v
}

/// The shuffle map is printed in the header as `--   0xNN -> NN NAME`.
fn map_has_opcode(source: &str, name: &str) -> bool {
    source
        .lines()
        .take_while(|l| l.starts_with("--"))
        .any(|l| l.split_whitespace().last() == Some(name))
}

/// CALL is the most common opcode in any non-trivial Luau chunk. A map that
/// never assigns it has mis-assigned its byte to something else.
#[test]
fn call_is_assigned_for_every_shuffled_chunk() {
    let files = bytecode_files();
    if files.is_empty() {
        eprintln!("SKIP: no bytecode available");
        return;
    }

    let mut missing = Vec::new();
    for path in &files {
        let Ok(bytes) = std::fs::read(path) else { continue };
        // DECODE THE WAY THE PRODUCT DOES. This called `luau_core::decompile`,
        // which runs detector-only opmap inference. On CameraModule that pins
        // 33 of the 40 opcode bytes the chunk uses, fills 4 by bijection and
        // leaves 3 UNMAPPED, and the header it emits says so outright: "treat
        // the mapping as provisional". Three unresolved opcodes mis-decode the
        // stream, and the output collapses from 577 lines and 25 functions to
        // 93 lines and 0 -- which is precisely the "module bodies were lost"
        // this test then reported.
        //
        // The loss was in the harness, not the emitter: every real invocation
        // resolves the measured database entry first (luau-cli/src/main.rs:358,
        // :718). Decoding here without it tested a path the decompiler never
        // takes and blamed the result on the emitter.
        let src = match decompile_like_product(&bytes) {
            Some(s) => s,
            None => continue,
        };
        // Only chunks that actually reported a shuffle map are in scope.
        if !src.lines().any(|l| l.contains("SHUFFLE MAP")) {
            continue;
        }
        let name = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
        if !map_has_opcode(&src, "CALL") {
            let stolen_by = if map_has_opcode(&src, "CAPTURE") { " (CAPTURE is assigned)" } else { "" };
            missing.push(format!("{}{}", name, stolen_by));
        }
    }

    assert!(
        missing.is_empty(),
        "CALL was never assigned in {} chunk(s): {}\n\
         CALL is the most frequent opcode in real bytecode; if it is unassigned \
         its byte has been claimed by another detector, and every call in the \
         chunk decodes as something else.",
        missing.len(),
        missing.join(", ")
    );
}

/// The consequence test: a large module must emit calls, not an empty table.
///
/// This is the check that would have caught the original defect. CameraModule
/// has 32 protos and ~20 named functions attached via DUPCLOSURE/SETTABLEKS;
/// producing `return {}` is a total loss of the module.
#[test]
fn large_modules_emit_calls_not_empty_tables() {
    let files = bytecode_files();
    if files.is_empty() {
        eprintln!("SKIP: no bytecode available");
        return;
    }

    let mut broken = Vec::new();
    for path in &files {
        let Ok(bytes) = std::fs::read(path) else { continue };
        // Same reason as the call site above: decode with the measured DB,
        // which is what every real invocation does.
        let src = match decompile_like_product(&bytes) {
            Some(s) => s,
            None => continue,
        };
        let name = path.file_stem().unwrap_or_default().to_string_lossy().to_string();

        let protos: usize = src
            .lines()
            .take(20)
            .find_map(|l| {
                l.trim()
                    .strip_prefix("-- Protos:")?
                    .trim()
                    .split_whitespace()
                    .next()?
                    .parse()
                    .ok()
            })
            .unwrap_or(0);
        if protos < 10 {
            continue; // only meaningful for substantial chunks
        }

        let code: Vec<&str> = src.lines().filter(|l| !l.trim_start().starts_with("--")).collect();
        // A call site looks like `name(` or `obj:method(`.
        let call_sites = code
            .iter()
            .filter(|l| l.contains('(') && !l.trim_start().starts_with("function"))
            .count();
        let functions = code
            .iter()
            .filter(|l| {
                let t = l.trim();
                t.starts_with("function ") || t.starts_with("local function ")
            })
            .count();

        if call_sites < 5 || functions * 4 < protos.saturating_sub(1) {
            broken.push(format!(
                "{} ({} protos -> {} functions, {} call sites)",
                name, protos, functions, call_sites
            ));
        }
    }

    assert!(
        broken.is_empty(),
        "module bodies were lost in {} chunk(s): {}\n\
         A chunk with many protos must decompile to many functions and many \
         call sites. Near-zero output means the instruction stream was decoded \
         wrongly — most often CALL mapped to another opcode.",
        broken.len(),
        broken.join("; ")
    );
}
