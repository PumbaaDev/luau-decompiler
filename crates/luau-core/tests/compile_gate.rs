//! Does our output actually compile?
//!
//! ── WHY THIS IS THE BEST CHECK WE HAVE ──────────────────────────────────
//! Every other measure in this project is one I wrote, and twice a fix
//! silently blunted the check meant to catch it:
//!
//!   * marker counting scored three catastrophically broken modules as
//!     "0 defects in 3 of 4 categories"
//!   * `free_var_decls` declared dropped values at chunk top, which took
//!     `undefined_local` to 0 while leaving 532 files silently nil
//!
//! This check cannot be blunted, because I do not decide the outcome — the
//! Luau compiler does. Output either parses and compiles or it does not.
//! There is no heuristic, no threshold, and no judgement call.
//!
//! It is a LOWER BOUND on correctness, not a proof of it: code can compile
//! cleanly and still be semantically wrong (that is precisely what the
//! `Events.lua` name/body mismatch was). But a file that does not compile is
//! definitively broken, and no argument can rescue it.
//!
//! ── WHAT IT CANNOT DO ───────────────────────────────────────────────────
//! True round-trip verification — run the original and the recovered source
//! and compare behaviour — is impossible for this corpus. Roblox bytecode
//! references `game`, `workspace` and `Enum`, none of which exist in a
//! standalone Luau VM. So compiling is as far as local verification reaches.
//!
//! Run with:
//!   OUR_CORPUS=<dir> cargo test -p luau-core --release --test compile_gate \
//!     -- --nocapture

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// `luau-compile --binary` exits non-zero and prints a diagnostic on failure.
fn luau_compile() -> Option<PathBuf> {
    let candidates = [
        r"C:\Users\jep\Downloads\luau-decompiler-v2\tools\luau\luau-compile.exe",
        r"C:\Users\jep\.luau_tools\luau-compile.exe",
    ];
    candidates.iter().map(PathBuf::from).find(|p| p.exists())
}

/// The compiler's diagnostic, with the leading file path stripped.
///
/// The first version matched any line containing ':' — which is the file path
/// prefix on every diagnostic line. Result: 66 failures all reported as the
/// path, classified "other", and no idea what was actually wrong. A harness
/// that reports failures without their cause is barely better than a count.
///
/// Luau diagnostics look like:
///   C:\path\to\file.lua(12,34): Expected identifier, got '='
/// so the message begins after the LAST `): ` on the line.
fn first_error(out: &str) -> String {
    for line in out.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Strip `<path>(line,col): ` if present.
        let msg = match line.find("): ") {
            Some(i) => &line[i + 3..],
            None => line,
        };
        // Skip lines that are only a path (no diagnostic followed).
        if msg.is_empty() || msg == line && line.contains(":\\") && !line.contains(' ') {
            continue;
        }
        return msg.chars().take(110).collect();
    }
    "(no diagnostic)".to_string()
}

/// Group similar diagnostics so the report shows causes, not 600 lines.
fn classify(msg: &str) -> &'static str {
    let m = msg.to_ascii_lowercase();
    // The two dominant real causes, found once the diagnostic extractor was
    // fixed. Both are emitter bugs, not lifter bugs:
    //   * `...` emitted inside a function not declared vararg
    //   * a statement followed by a line starting with `(`, which Lua reads
    //     as a call on the previous expression
    if m.contains("vararg") {
        "vararg outside vararg fn"
    } else if m.contains("ambiguous syntax") {
        "ambiguous call/statement"
    } else if m.contains("expected identifier") {
        "expected identifier"
    } else if m.contains("expected 'end'") || m.contains("expected end") {
        "missing end"
    } else if m.contains("unexpected") {
        "unexpected token"
    } else if m.contains("expected ')'") || m.contains("expected ']'") || m.contains("expected '}'") {
        "unclosed bracket"
    } else if m.contains("expected") {
        "other expected-X"
    } else if m.contains("too many") || m.contains("exceeds") {
        "limit exceeded"
    } else {
        "other"
    }
}

#[test]
fn our_output_compiles() {
    let Ok(dir) = std::env::var("OUR_CORPUS") else {
        eprintln!("SKIP: set OUR_CORPUS=<dir>");
        return;
    };
    let Some(compiler) = luau_compile() else {
        eprintln!("SKIP: luau-compile.exe not found");
        return;
    };

    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "lua"))
        .collect();
    files.sort();
    if files.is_empty() {
        eprintln!("SKIP: no .lua files in {}", dir);
        return;
    }

    let mut ok = 0usize;
    let mut failed: Vec<(String, String)> = Vec::new();

    for path in &files {
        let out = Command::new(&compiler)
            .arg("--binary")
            .arg(path)
            .output();
        let Ok(out) = out else { continue };
        if out.status.success() {
            ok += 1;
        } else {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            failed.push((
                path.file_stem().unwrap_or_default().to_string_lossy().to_string(),
                first_error(&text),
            ));
        }
    }

    let total = ok + failed.len();
    let mut by_kind: BTreeMap<&'static str, usize> = BTreeMap::new();
    for (_, msg) in &failed {
        *by_kind.entry(classify(msg)).or_insert(0) += 1;
    }

    eprintln!();
    eprintln!("================================================================");
    eprintln!("  COMPILE GATE — the Luau compiler decides, not us");
    eprintln!("================================================================");
    eprintln!("  {}", dir);
    eprintln!();
    eprintln!("  files              {:>6}", total);
    eprintln!(
        "  compiles           {:>6}  ({:.1}%)",
        ok,
        100.0 * ok as f64 / total.max(1) as f64
    );
    eprintln!("  does NOT compile   {:>6}", failed.len());
    eprintln!();
    if !by_kind.is_empty() {
        eprintln!("  FAILURE KINDS");
        let mut kinds: Vec<_> = by_kind.iter().collect();
        kinds.sort_by(|a, b| b.1.cmp(a.1));
        for (k, n) in kinds {
            eprintln!("    {:<24} {:>5}", k, n);
        }
        eprintln!();
        eprintln!("  FIRST 15 FAILURES");
        for (name, msg) in failed.iter().take(15) {
            let short: String = name.chars().rev().take(52).collect::<String>().chars().rev().collect();
            eprintln!("    {}", short);
            eprintln!("        {}", msg);
        }
    }
    eprintln!("================================================================");
}

/// The reference decompiler run through the same gate.
///
/// Without this, "N% of ours compiles" has no scale. If the reference scores
/// similarly, the failures are corpus difficulty; if it scores far higher,
/// they are our defects.
#[test]
fn reference_output_compiles() {
    let Ok(dir) = std::env::var("REF_CORPUS") else {
        eprintln!("SKIP: set REF_CORPUS=<dir>");
        return;
    };
    let Some(compiler) = luau_compile() else {
        eprintln!("SKIP: luau-compile.exe not found");
        return;
    };
    let files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "lua"))
        .collect();

    let mut ok = 0usize;
    let mut bad = 0usize;
    for path in &files {
        let Ok(out) = Command::new(&compiler).arg("--binary").arg(path).output() else { continue };
        if out.status.success() { ok += 1 } else { bad += 1 }
    }
    let total = ok + bad;
    eprintln!();
    eprintln!("  REFERENCE (Potassium) through the same gate:");
    eprintln!(
        "    compiles {}/{} ({:.1}%)",
        ok,
        total,
        100.0 * ok as f64 / total.max(1) as f64
    );
}

/// Sanity: the gate must reject code that is obviously broken.
/// Without this, a silently-passing harness would report 100% forever.
#[test]
fn gate_rejects_known_bad_source() {
    let Some(compiler) = luau_compile() else {
        eprintln!("SKIP: luau-compile.exe not found");
        return;
    };
    let dir = std::env::temp_dir().join("luau_compile_gate_selftest");
    let _ = std::fs::create_dir_all(&dir);

    let bad = dir.join("bad.lua");
    std::fs::write(&bad, "local x = = 1\n").expect("write");
    let out = Command::new(&compiler).arg("--binary").arg(&bad).output().expect("run");
    assert!(
        !out.status.success(),
        "gate accepted syntactically invalid source — the harness is not testing anything"
    );

    let good = dir.join("good.lua");
    std::fs::write(&good, "local function f(a) return a + 1 end\nreturn f\n").expect("write");
    let out2 = Command::new(&compiler).arg("--binary").arg(&good).output().expect("run");
    assert!(
        out2.status.success(),
        "gate rejected valid source — it would report false failures"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
