//! Library/CLI parity: `luau_core::decompile` must produce exactly the text
//! `luau-decompiler decompile <file>` prints, with ONE intended divergence.
//!
//! `luau-wasm`, `luau-server` and the `luau-ffi` C ABI all call the library.
//! Twice now the library has silently diverged from the binary: first it
//! lacked the measured opcode database (detector-only, a 31-point clean-rate
//! gap on the union corpus), then it lacked the CLI's v10-v14 auto-enable, so
//! every live Roblox v12 script the CLI decodes was REFUSED by every non-CLI
//! front end. Comparing against the real binary is the only check that cannot
//! drift with either side.
//!
//! The intended divergence: a v10-v14 chunk no measured database entry
//! verifies for is refused by the library default (detection on such a chunk
//! yields plausible-but-wrong code) while the CLI decodes it by detection.
//! For every such file the library's opt-in (`V10PlusMode::DetectorFallback`)
//! must then reproduce the CLI exactly.
//!
//! Two tiers:
//! * always-on: the committed v12 fixtures (upstream `luau-compile` output; no
//!   game bytecode is committed), which the database does not cover;
//! * corpus: set `LUAU_PARITY_CORPUS` to one or more directories
//!   (`;`-separated on Windows, `:` elsewhere) of `.bin`/`.luac` files, and
//!   optionally `LUAU_PARITY_PER_DIR` (default 20) for how many files to
//!   sample evenly from each, and `LUAU_PARITY_CUSTOM_DB` to a database JSON to
//!   also compare `OpmapDbChoice::Custom` with `--opmap-db`. Skipped when unset.

use luau_core::{DecompileOptions, OpmapDbChoice, V10PlusMode};
use std::path::{Path, PathBuf};
use std::process::Command;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Run the shipped binary. The child runs from the workspace root so the CLI
/// finds `opmap_db/` the way it does in normal use.
fn cli(args: &[&std::ffi::OsStr]) -> Result<String, String> {
    let out = Command::new(env!("CARGO_BIN_EXE_luau-decompiler"))
        .current_dir(workspace_root())
        .args(args)
        .output()
        .map_err(|e| format!("spawn: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    String::from_utf8(out.stdout).map_err(|e| format!("non-UTF-8 stdout: {e}"))
}

fn cli_decompile(file: &Path, db: Option<&Path>) -> Result<String, String> {
    let mut args: Vec<&std::ffi::OsStr> = Vec::new();
    if let Some(db) = db {
        args.push("--opmap-db".as_ref());
        args.push(db.as_os_str());
    }
    args.push("decompile".as_ref());
    args.push(file.as_os_str());
    cli(&args)
}

#[derive(Debug, PartialEq)]
enum Parity {
    /// Identical output (or both refused).
    Same,
    /// Library default refused an unmeasured v10+ chunk the CLI decoded, and
    /// the library's CLI-style opt-in reproduced the CLI exactly.
    IntendedRefusal,
}

fn first_difference(a: &str, b: &str) -> String {
    a.lines()
        .zip(b.lines())
        .position(|(x, y)| x != y)
        .map_or_else(|| "length".to_string(), |i| format!("line {}", i + 1))
}

fn compare(
    file: &Path,
    lib: Result<String, String>,
    cli: Result<String, String>,
) -> Result<(), String> {
    match (&lib, &cli) {
        (Ok(l), Ok(c)) if l == c => Ok(()),
        (Err(_), Err(_)) => Ok(()),
        (Ok(l), Ok(c)) => {
            Err(format!("{}: outputs differ at {}", file.display(), first_difference(l, c)))
        }
        (Err(e), Ok(_)) => Err(format!("{}: library failed, CLI decoded: {e}", file.display())),
        (Ok(_), Err(e)) => Err(format!("{}: CLI failed, library decoded: {e}", file.display())),
    }
}

fn parity(file: &Path) -> Result<Parity, String> {
    let data = std::fs::read(file).unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
    let lib = luau_core::decompile(&data).map_err(|e| format!("{e:#}"));
    let cli = cli_decompile(file, None);
    if let (Err(e), Ok(_)) = (&lib, &cli) {
        if e.starts_with("refusing Luau bytecode v") {
            let opts =
                DecompileOptions { v10plus: V10PlusMode::DetectorFallback, ..Default::default() };
            let opt_in = luau_core::decompile_with_options(&data, &opts).map_err(|e| format!("{e:#}"));
            compare(file, opt_in, cli)?;
            return Ok(Parity::IntendedRefusal);
        }
    }
    compare(file, lib, cli).map(|()| Parity::Same)
}

#[test]
fn library_refuses_unmeasured_v12_fixtures_and_opts_in_like_the_cli() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../luau-core/tests/fixtures/v10plus");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("fixtures at {}: {e}", dir.display()))
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "luac"))
        .collect();
    files.sort();
    assert!(files.len() >= 3, "expected the v12 fixtures, found {}", files.len());
    if std::env::var_os("LUAU_ALLOW_V10PLUS").is_some() {
        eprintln!("SKIP: LUAU_ALLOW_V10PLUS exported; the library defers to it");
        return;
    }
    for f in &files {
        // Upstream luau-compile output matches no Roblox entry: the library
        // must refuse it, and its opt-in must equal the CLI.
        assert_eq!(parity(f), Ok(Parity::IntendedRefusal), "{}", f.display());
    }
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
    let custom_path = std::env::var_os("LUAU_PARITY_CUSTOM_DB").map(PathBuf::from);
    let custom_db = custom_path.as_ref().map(|p| {
        luau_core::parser::opmap_db::OpmapDb::load(p)
            .unwrap_or_else(|e| panic!("custom db {}: {e:#}", p.display()))
            .0
    });

    let (mut checked, mut same, mut refused) = (0usize, 0usize, Vec::new());
    let mut failures = Vec::new();
    for dir in std::env::split_paths(&dirs) {
        let files = sample(&dir, per_dir);
        assert!(!files.is_empty(), "no .bin/.luac files in {}", dir.display());
        for f in files {
            checked += 1;
            match parity(&f) {
                Ok(Parity::Same) => same += 1,
                Ok(Parity::IntendedRefusal) => refused.push(f.display().to_string()),
                Err(m) => failures.push(m),
            }
            if let (Some(db), Some(path)) = (custom_db.as_ref(), custom_path.as_ref()) {
                let data = std::fs::read(&f).unwrap();
                let opts = DecompileOptions {
                    opmap_db: OpmapDbChoice::Custom(db),
                    v10plus: V10PlusMode::DetectorFallback,
                };
                let lib = luau_core::decompile_with_options(&data, &opts).map_err(|e| format!("{e:#}"));
                if let Err(m) = compare(&f, lib, cli_decompile(&f, Some(path))) {
                    failures.push(format!("custom db: {m}"));
                }
            }
        }
    }
    eprintln!(
        "library/CLI parity: {checked} files, {same} identical, {} intended refusals, {} mismatches",
        refused.len(),
        failures.len()
    );
    for r in &refused {
        eprintln!("  intended refusal (no measured entry; opt-in == CLI): {r}");
    }
    assert!(failures.is_empty(), "library != CLI:\n{}", failures.join("\n"));
}
