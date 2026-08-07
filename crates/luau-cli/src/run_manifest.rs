//! Per-run output folders with a written manifest.
//!
//! ── WHY ──────────────────────────────────────────────────────────────────
//! Decompiling into a folder that already holds output from a previous binary
//! silently mixes generations. On 2026-08-03 a run showed a mix of old-map and
//! new-map files and every percentage taken from it was meaningless, because
//! nothing recorded which binary produced which file.
//!
//! So every run gets:
//!   * its own timestamped folder — nothing is ever written alongside output
//!     from a different build;
//!   * a `MANIFEST.md` recording the decompiler version, the git commit, the
//!     opcode-map source, the inputs, and the measured result.
//!
//! The manifest is the point. A folder of .lua files with no provenance
//! cannot be compared against anything, and a number you cannot attribute to a
//! binary is not a measurement.

use anyhow::Result;
use std::path::{Path, PathBuf};

/// What a run needs to describe itself afterwards.
pub struct RunInfo {
    pub input: PathBuf,
    pub out_dir: PathBuf,
    pub started: String,
    pub decompiler_version: String,
    pub git_commit: String,
    pub git_dirty: bool,
    /// Where the opcode map came from: detection, a pooled cache, or an exact
    /// database entry. Recorded because it changes the output substantially.
    pub opmap_source: String,
    pub total_inputs: usize,
    pub ok: u32,
    pub failed: u32,
    pub elapsed_secs: f64,
    /// Filled in when the semantic checker ran over the output.
    pub semantic: Option<SemanticSummary>,
}

pub struct SemanticSummary {
    pub files_checked: usize,
    pub files_clean: usize,
    pub total_defects: usize,
    /// (check name, files affected, defect count), worst first.
    pub by_check: Vec<(String, usize, usize)>,
}

/// A UTC timestamp usable in a folder name: `20260803_154212`.
fn stamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Civil-from-days, so no chrono dependency is pulled in for one string.
    let days = (now / 86_400) as i64;
    let secs_of_day = now % 86_400;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{:04}{:02}{:02}_{:02}{:02}{:02}",
        y,
        m,
        d,
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60
    )
}

/// Human-readable UTC timestamp for the manifest body.
pub fn run_timestamp_human() -> String {
    let s = stamp();
    // `20260803_154212` -> `2026-08-03 15:42:12 UTC`
    if s.len() == 15 {
        format!(
            "{}-{}-{} {}:{}:{} UTC",
            &s[0..4], &s[4..6], &s[6..8], &s[9..11], &s[11..13], &s[13..15]
        )
    } else {
        s
    }
}

fn git(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Create a fresh timestamped run folder under `base`.
///
/// `keep` previous run folders are retained; older ones are removed. Only
/// folders this tool created (matching `run_<timestamp>`) are ever touched —
/// an arbitrary directory handed in by the user is never deleted.
pub fn prepare_run_dir(base: &Path, keep: usize) -> Result<PathBuf> {
    std::fs::create_dir_all(base)?;

    let mut runs: Vec<PathBuf> = std::fs::read_dir(base)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("run_"))
        })
        .collect();
    runs.sort();

    // Remove the oldest, keeping `keep`. Deliberately narrow: only our own
    // `run_*` folders, never the base directory or anything else in it.
    if runs.len() > keep {
        for old in &runs[..runs.len() - keep] {
            if let Err(e) = std::fs::remove_dir_all(old) {
                eprintln!("  warning: could not remove old run {}: {}", old.display(), e);
            } else {
                eprintln!("  removed old run: {}", old.file_name().unwrap_or_default().to_string_lossy());
            }
        }
    }

    let dir = base.join(format!("run_{}", stamp()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Gather build/version facts for the manifest.
pub fn collect_build_info() -> (String, String, bool) {
    let version = env!("CARGO_PKG_VERSION").to_string();
    let commit = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let dirty = git(&["status", "--porcelain"])
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    (version, commit, dirty)
}

/// Write `MANIFEST.md` into the run folder.
pub fn write_manifest(info: &RunInfo) -> Result<PathBuf> {
    let path = info.out_dir.join("MANIFEST.md");
    let mut m = String::new();

    m.push_str("# Decompile run\n\n");
    m.push_str(&format!("**{}**\n\n", info.started));
    m.push_str(
        "This folder holds the output of ONE decompile run and nothing else. \
         Output from different binaries is never mixed, because a percentage \
         taken from a mixed folder cannot be attributed to anything.\n\n",
    );

    m.push_str("## Build\n\n");
    m.push_str("| | |\n|---|---|\n");
    m.push_str(&format!("| decompiler version | `{}` |\n", info.decompiler_version));
    m.push_str(&format!(
        "| git commit | `{}`{} |\n",
        info.git_commit,
        if info.git_dirty { " **+ uncommitted changes**" } else { "" }
    ));
    if info.git_dirty {
        m.push_str(
            "\n> The working tree had uncommitted changes when this ran, so the \
             commit above does **not** fully describe the binary. Treat these \
             results as unreproducible until the tree is clean.\n",
        );
    }
    m.push('\n');

    m.push_str("## Inputs\n\n");
    m.push_str("| | |\n|---|---|\n");
    m.push_str(&format!("| source | `{}` |\n", info.input.display()));
    m.push_str(&format!("| bytecode files found | {} |\n", info.total_inputs));
    m.push_str(&format!("| opcode map | {} |\n", info.opmap_source));
    m.push('\n');

    m.push_str("## Result\n\n");
    m.push_str("| | |\n|---|---|\n");
    m.push_str(&format!("| decompiled | {} |\n", info.ok));
    m.push_str(&format!("| failed | {} |\n", info.failed));
    m.push_str(&format!("| elapsed | {:.1}s |\n", info.elapsed_secs));
    m.push('\n');

    if let Some(s) = &info.semantic {
        let pct = if s.files_checked > 0 {
            100.0 * s.files_clean as f64 / s.files_checked as f64
        } else {
            0.0
        };
        m.push_str("## Semantic check\n\n");
        m.push_str(
            "Output is checked for defects that are *provably wrong* — code that \
             would error, or that runs as a different program than the bytecode \
             describes. This is deliberately not a count of marker strings: on \
             2026-08-03 marker counting scored four badly broken files as clean, \
             including one whose entire module body had been discarded.\n\n",
        );
        m.push_str("| | |\n|---|---|\n");
        m.push_str(&format!("| files checked | {} |\n", s.files_checked));
        m.push_str(&format!("| clean | {} ({:.1}%) |\n", s.files_clean, pct));
        m.push_str(&format!("| total defects | {} |\n", s.total_defects));
        m.push('\n');

        if !s.by_check.is_empty() {
            m.push_str("| check | files | defects |\n|---|---:|---:|\n");
            for (name, files, count) in &s.by_check {
                m.push_str(&format!("| `{}` | {} | {} |\n", name, files, count));
            }
            m.push('\n');
        }

        m.push_str("### What the checks mean\n\n");
        m.push_str("| check | meaning |\n|---|---|\n");
        m.push_str("| `undefined_local` | a name is read or written but never declared — the output errors at runtime. Usually a captured upvalue whose declaration was lost. |\n");
        m.push_str("| `bodies_dropped` | the chunk has far more protos than the output has functions — function bodies were discarded. |\n");
        m.push_str("| `name_body_mismatch` | a function carries a body that identifies itself as a different function, so calling one runs another. |\n");
        m.push_str("| `discarded_table_write` | a table built in a loop and never read — a table assignment whose target was lost, so every write is thrown away. |\n");
        m.push_str("| `property_called_as_method` | `script:Parent()` — a property read emitted as a method call, which errors. |\n");
        m.push('\n');
    } else {
        m.push_str("## Semantic check\n\nNot run for this batch.\n\n");
    }

    m.push_str("## Reproducing this run\n\n");
    m.push_str("```bash\n");
    m.push_str(&format!("git checkout {}\n", info.git_commit));
    m.push_str("cargo build --release -p luau-cli\n");
    m.push_str(&format!(
        "./target/release/luau-decompiler batch \"{}\"\n",
        info.input.display()
    ));
    m.push_str("```\n");

    std::fs::write(&path, m)?;
    Ok(path)
}
