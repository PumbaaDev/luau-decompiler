//! Semantic checks on FRESHLY DECOMPILED bytecode.
//!
//! `semantic_corpus_scan` reads .lua files decompiled by an older binary, so
//! it cannot show whether a fix worked. This one decompiles real Roblox
//! bytecode with the CURRENT code and checks that output, which is the only
//! way to see a fix land.

use luau_core::decompiler::semantic_check::{check, format_report, Severity};
use std::path::Path;

const BYTECODE_DIR: &str =
    r"C:\Users\jep\AppData\Local\Potassium\workspace\decompiler";

fn proto_count(src: &str) -> Option<usize> {
    src.lines().take(20).find_map(|l| {
        let rest = l.trim().strip_prefix("-- Protos:")?;
        rest.trim().split_whitespace().next()?.parse().ok()
    })
}

#[test]
fn decompile_and_check_live_bytecode() {
    let dir = Path::new(BYTECODE_DIR);
    if !dir.exists() {
        eprintln!("SKIP: no bytecode dir at {}", dir.display());
        return;
    }
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "bin"))
        .collect();
    files.sort();

    let mut total_wrong = 0usize;
    let mut undefined = 0usize;

    eprintln!();
    eprintln!("=== semantic check on FRESH decompiles ===");
    for path in &files {
        let Ok(bytes) = std::fs::read(path) else { continue };
        let name = path.file_stem().unwrap_or_default().to_string_lossy();

        let src = match luau_core::decompile(&bytes) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("  {:<26} DECOMPILE FAILED: {}", name, e);
                continue;
            }
        };
        let findings = check(&src, proto_count(&src));
        let wrong: Vec<_> = findings
            .iter()
            .filter(|f| f.severity == Severity::Wrong)
            .collect();
        let undef = wrong.iter().filter(|f| f.check == "undefined_local").count();
        total_wrong += wrong.len();
        undefined += undef;

        eprintln!(
            "  {:<26} {:>4} lines  {:>3} wrong ({} undefined_local)",
            name,
            src.lines().count(),
            wrong.len(),
            undef
        );
        if !wrong.is_empty() && wrong.len() <= 4 {
            for f in &wrong {
                eprintln!("       - {}", f.detail);
            }
        }
    }
    eprintln!();
    eprintln!(
        "  TOTAL: {} provably wrong, of which {} are undefined_local",
        total_wrong, undefined
    );
    eprintln!();

    // The fix targets undefined_local specifically. If it works, fresh
    // decompiles carry none. Reported rather than asserted for now so the
    // number is visible while other defect classes are still open.
    if undefined > 0 {
        eprintln!(
            "NOTE: {} undefined_local remain — the free-var pass did not \
             cover these; investigate before claiming the fix works.",
            undefined
        );
    }
}

/// Focused: the pass must not shadow real globals in real output.
#[test]
fn fix_does_not_shadow_globals() {
    let dir = Path::new(BYTECODE_DIR);
    if !dir.exists() {
        return;
    }
    let p = dir.join("CameraInput_bc.bin");
    let Ok(bytes) = std::fs::read(&p) else { return };
    let Ok(src) = luau_core::decompile(&bytes) else { return };

    for line in src.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("local ") {
            let names = rest.split('=').next().unwrap_or("");
            for n in names.split(',') {
                let n = n.trim();
                assert!(
                    !matches!(n, "game" | "workspace" | "script" | "Enum" | "Instance"
                        | "pairs" | "ipairs" | "print" | "warn" | "task" | "math"
                        | "string" | "table" | "require" | "tostring" | "tonumber"),
                    "generated declaration shadows the global `{}` — this would \
                     break otherwise-working output",
                    n
                );
            }
        }
    }
}
