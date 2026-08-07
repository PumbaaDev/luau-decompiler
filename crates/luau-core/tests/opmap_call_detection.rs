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

const BYTECODE_DIR: &str = r"C:\Users\jep\AppData\Local\Potassium\workspace\decompiler";

fn bytecode_files() -> Vec<PathBuf> {
    let dir = Path::new(BYTECODE_DIR);
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
        let Ok(src) = luau_core::decompile(&bytes) else { continue };
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
        let Ok(src) = luau_core::decompile(&bytes) else { continue };
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
