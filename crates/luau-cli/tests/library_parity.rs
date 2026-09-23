//! Library/CLI parity: `luau_core::decompile` must produce exactly the text
//! `luau-decompiler decompile <file>` prints.
//!
//! `luau-wasm`, `luau-server` and the `luau-ffi` C ABI all call
//! `luau_core::decompile`. Twice now the library has silently diverged from
//! the binary: first it lacked the measured opcode database (detector-only,
//! a 31-point clean-rate gap on the union corpus), then it lacked the CLI's
//! v10-v14 auto-enable, so every live Roblox v12 script the CLI decodes was
//! REFUSED by every non-CLI front end. Comparing against the real binary is the
//! only check that cannot drift with either side.
//!
//! Two tiers:
//! * always-on: the committed v12 fixtures (no game bytecode is committed);
//! * corpus: set `LUAU_PARITY_CORPUS` to one or more directories
//!   (`;`-separated on Windows, `:` elsewhere) of `.bin`/`.luac` files, and
//!   optionally `LUAU_PARITY_PER_DIR` (default 20) for how many files to
//!   sample evenly from each. Skipped when unset.

use std::path::{Path, PathBuf};
use std::process::Command;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The CLI's single-file decode, run as the shipped binary does. The child
/// runs from the workspace root so the CLI finds `opmap_db/` the way it does
/// in normal use.
fn cli_decompile(file: &Path) -> Result<String, String> {
    let out = Command::new(env!("CARGO_BIN_EXE_luau-decompiler"))
        .current_dir(workspace_root())
        .arg("decompile")
        .arg(file)
        .output()
        .map_err(|e| format!("spawn: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    String::from_utf8(out.stdout).map_err(|e| format!("non-UTF-8 stdout: {e}"))
}

/// Compare one file; `None` when both sides agree (including both failing).
fn mismatch(file: &Path) -> Option<String> {
    let data = std::fs::read(file).unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
    let lib = luau_core::decompile(&data).map_err(|e| format!("{e:#}"));
    let cli = cli_decompile(file);
    match (&lib, &cli) {
        (Ok(l), Ok(c)) if l == c => None,
        (Err(_), Err(_)) => None,
        (Ok(l), Ok(c)) => {
            let line = l
                .lines()
                .zip(c.lines())
                .position(|(a, b)| a != b)
                .map_or_else(|| "length".to_string(), |i| format!("line {}", i + 1));
            Some(format!("{}: outputs differ at {line}", file.display()))
        }
        (Err(e), Ok(_)) => Some(format!("{}: library failed, CLI decoded: {e}", file.display())),
        (Ok(_), Err(e)) => Some(format!("{}: CLI failed, library decoded: {e}", file.display())),
    }
}

#[test]
fn library_matches_cli_on_v12_fixtures() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../luau-core/tests/fixtures/v10plus");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("fixtures at {}: {e}", dir.display()))
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "luac"))
        .collect();
    files.sort();
    assert!(files.len() >= 3, "expected the v12 fixtures, found {}", files.len());

    let mut decoded = 0;
    for f in &files {
        if let Some(m) = mismatch(f) {
            panic!("{m}");
        }
        if luau_core::decompile(&std::fs::read(f).unwrap()).is_ok() {
            decoded += 1;
        }
    }
    // Agreement by both refusing would pass `mismatch`; the CLI decodes these,
    // so the library must too.
    assert_eq!(decoded, files.len(), "every v12 fixture must decode through the library");
}

fn sample(dir: &Path, n: usize) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "bin" || x == "luac"))
        .collect();
    files.sort();
    if files.len() <= n {
        return files;
    }
    let step = files.len() as f64 / n as f64;
    (0..n).map(|i| files[(i as f64 * step) as usize].clone()).collect()
}

#[test]
fn library_matches_cli_on_corpus_sample() {
    let Some(dirs) = std::env::var_os("LUAU_PARITY_CORPUS") else {
        eprintln!("SKIP: LUAU_PARITY_CORPUS not set");
        return;
    };
    let per_dir: usize = std::env::var("LUAU_PARITY_PER_DIR")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);

    let mut checked = 0usize;
    let mut decoded = 0usize;
    let mut failures = Vec::new();
    for dir in std::env::split_paths(&dirs) {
        let files = sample(&dir, per_dir);
        assert!(!files.is_empty(), "no .bin/.luac files in {}", dir.display());
        for f in files {
            checked += 1;
            if std::fs::read(&f).is_ok_and(|d| luau_core::decompile(&d).is_ok()) {
                decoded += 1;
            }
            if let Some(m) = mismatch(&f) {
                failures.push(m);
            }
        }
    }
    eprintln!(
        "library/CLI parity: {checked} files ({decoded} decoded by the library), {} mismatches",
        failures.len()
    );
    assert!(failures.is_empty(), "library != CLI:\n{}", failures.join("\n"));
}
