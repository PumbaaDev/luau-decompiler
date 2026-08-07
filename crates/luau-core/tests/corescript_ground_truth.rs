//! Regression suite against PUBLISHED Roblox CoreScript source.
//!
//! ── WHY THIS IS DIFFERENT FROM EVERY OTHER CHECK HERE ───────────────────
//! Every other quality measure in this project is circular in some way:
//!
//!   * the semantic checks are mine, and twice a fix of mine silently
//!     blunted the check meant to catch it (marker counting; then
//!     free_var_decls masking dropped values as declared-but-nil)
//!   * the Potassium reference is a second opinion, not ground truth — it
//!     loses functions entirely on 4 files where we do not
//!
//! Seven wrong theories were formed in a single session by validating
//! against sources that were themselves unvalidated.
//!
//! Roblox CoreScripts are DIFFERENT: the source is published, so the correct
//! answer is knowable rather than inferred. Nothing in this repository can
//! make this suite lie.
//!
//! ── WHAT IS ASSERTED, AND WHY NOT TEXT ──────────────────────────────────
//! Structural FACTS about the published source — function names, counts,
//! rough size — not source text. Three reasons:
//!
//!   1. Decompilation is lossy by definition. Text equality would fail on
//!      formatting and prove nothing.
//!   2. Names surviving is the strongest signal available: recovering
//!      `getRelativeVelocity` by name means constants, the string table and
//!      the closure/declaration path all worked.
//!   3. No third-party source is vendored into this repository.
//!
//! ── ADDING A CASE ───────────────────────────────────────────────────────
//! Read the published source, record its function names and line count, and
//! add a `CoreScriptCase`. Record only what was ACTUALLY read — an entry
//! guessed from a filename is worse than no entry, because it manufactures
//! confidence.
//!
//! Run with:
//!   BC_CORPUS=<dir> cargo test -p luau-core --release \
//!     --test corescript_ground_truth -- --nocapture

use std::path::{Path, PathBuf};

struct CoreScriptCase {
    /// Substring identifying the .bin in the corpus.
    corpus_match: &'static str,
    /// Human name for reporting.
    name: &'static str,
    /// Where the published source was read from.
    source_url: &'static str,
    /// Every function name the published source declares.
    expected_functions: &'static [&'static str],
    /// Approximate line count of the published source.
    source_lines: usize,
}

/// Verified cases only. Each `expected_functions` list was read from the
/// published source, not guessed.
const CASES: &[CoreScriptCase] = &[CoreScriptCase {
    corpus_match: "RbxCharacterSounds.bin",
    name: "RbxCharacterSounds",
    source_url: "https://github.com/MaximumADHD/Roblox-Client-Tracker/blob/roblox/scripts/PlayerScripts/StarterPlayerScriptsCommon/RbxCharacterSounds.lua",
    expected_functions: &[
        "loadFlag",
        "map",
        "getRelativeVelocity",
        "playSound",
        "stopSound",
        "playSoundIf",
        "setSoundLooped",
        "shallowCopy",
        "initializeSoundSystem",
    ],
    source_lines: 380,
}];

fn corpus_dir() -> PathBuf {
    PathBuf::from(std::env::var("BC_CORPUS").unwrap_or_else(|_| {
        r"C:\Users\jep\AppData\Local\Potassium\workspace\bc_extract_1786138100".to_string()
    }))
}

fn find_bin(dir: &Path, needle: &str) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.file_name().is_some_and(|n| n.to_string_lossy().contains(needle)))
}

/// Is `name` declared as a function anywhere in the output?
///
/// Accepts every form the emitter may legitimately choose — `local function f`,
/// `function t.f`, `function t:f`, `local f = function(`, `f = function(` —
/// because which one is correct depends on the original source, and this suite
/// tests RECOVERY, not emitter style.
fn declares_function(src: &str, name: &str) -> bool {
    src.lines().filter(|l| !l.trim_start().starts_with("--")).any(|l| {
        let t = l.trim();
        t.starts_with(&format!("local function {}(", name))
            || t.starts_with(&format!("function {}(", name))
            || t.contains(&format!(".{}(", name)) && t.starts_with("function ")
            || t.contains(&format!(":{}(", name)) && t.starts_with("function ")
            || t.starts_with(&format!("local {} = function(", name))
            || t.starts_with(&format!("{} = function(", name))
    })
}

#[test]
fn corescripts_recover_their_published_functions() {
    let dir = corpus_dir();
    if !dir.exists() {
        eprintln!("SKIP: corpus not present at {}", dir.display());
        return;
    }

    let mut total_expected = 0usize;
    let mut total_found = 0usize;
    let mut failures: Vec<String> = Vec::new();

    eprintln!();
    eprintln!("================================================================");
    eprintln!("  CORESCRIPT GROUND TRUTH  ({} case(s))", CASES.len());
    eprintln!("================================================================");

    for case in CASES {
        let Some(path) = find_bin(&dir, case.corpus_match) else {
            eprintln!("  {} — SKIP, not in corpus", case.name);
            continue;
        };
        let Ok(bytes) = std::fs::read(&path) else { continue };
        let Ok(src) = luau_core::decompile(&bytes) else {
            failures.push(format!("{}: decompile failed outright", case.name));
            continue;
        };

        let code_lines = src.lines().filter(|l| !l.trim_start().starts_with("--")).count();
        let mut missing: Vec<&str> = Vec::new();
        for f in case.expected_functions {
            total_expected += 1;
            if declares_function(&src, f) {
                total_found += 1;
            } else {
                missing.push(f);
            }
        }

        let pct = 100.0 * (case.expected_functions.len() - missing.len()) as f64
            / case.expected_functions.len().max(1) as f64;
        eprintln!();
        eprintln!("  {}", case.name);
        eprintln!("    source        {} lines, {} functions", case.source_lines, case.expected_functions.len());
        eprintln!("    ours          {} lines", code_lines);
        eprintln!(
            "    recovered     {}/{} functions ({:.0}%)",
            case.expected_functions.len() - missing.len(),
            case.expected_functions.len(),
            pct
        );
        if !missing.is_empty() {
            eprintln!("    MISSING       {}", missing.join(", "));
            failures.push(format!("{}: missing {}", case.name, missing.join(", ")));
        }
        eprintln!("    source        {}", case.source_url);
    }

    eprintln!();
    eprintln!(
        "  TOTAL: {}/{} functions recovered across {} case(s)",
        total_found, total_expected, CASES.len()
    );
    eprintln!("================================================================");

    assert!(
        failures.is_empty(),
        "CoreScript ground-truth regressions:\n  {}\n\
         These names are in the PUBLISHED source, so a miss is a definite \
         defect — not a difference of opinion between decompilers.",
        failures.join("\n  ")
    );
}
