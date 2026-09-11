//! Semantic scan of a FRESHLY decompiled corpus.
//!
//! Every previous corpus number in this project measured output from binaries
//! that predated the fixes being evaluated — so the percentages described the
//! past, not current capability. `FULL_MACHINE_SCAN.md` had to label its central
//! claim a *prediction* for exactly that reason.
//!
//! This scans a corpus extracted and decompiled with the current binary, so the
//! result is a measurement of the code as it stands today.
//!
//! Point it with:  FRESH_CORPUS=<dir> cargo test -p luau-core --release \
//!                   --test scan_fresh_corpus -- --nocapture

use luau_core::decompiler::semantic_check::{check, Severity};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn proto_count(src: &str) -> Option<usize> {
    src.lines().take(24).find_map(|l| {
        let rest = l.trim().strip_prefix("-- Protos:")?;
        rest.trim().split_whitespace().next()?.parse().ok()
    })
}

#[test]
fn scan_fresh_corpus() {
    let Ok(dir) = std::env::var("FRESH_CORPUS") else {
        eprintln!("SKIP: set FRESH_CORPUS=<dir>");
        return;
    };
    let root = Path::new(&dir);
    if !root.exists() {
        eprintln!("SKIP: {} does not exist", dir);
        return;
    }

    let mut files: Vec<PathBuf> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "lua"))
        .collect();
    files.sort();

    let mut opened = 0usize;
    let mut clean = 0usize;
    let mut total_defects = 0usize;
    let mut total_lines = 0usize;
    let mut by_check: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut files_with: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut worst: Vec<(usize, String, String)> = Vec::new();

    for path in &files {
        let Ok(src) = std::fs::read_to_string(path) else { continue };
        opened += 1;
        total_lines += src.lines().count();

        let findings = check(&src, proto_count(&src));
        let wrong: Vec<_> = findings
            .iter()
            .filter(|f| f.severity == Severity::Wrong)
            .collect();
        if wrong.is_empty() {
            clean += 1;
            continue;
        }
        total_defects += wrong.len();

        let mut kinds: BTreeMap<&'static str, usize> = BTreeMap::new();
        for f in &wrong {
            *by_check.entry(f.check).or_insert(0) += 1;
            *kinds.entry(f.check).or_insert(0) += 1;
        }
        for k in kinds.keys() {
            *files_with.entry(k).or_insert(0) += 1;
        }
        worst.push((
            wrong.len(),
            path.file_name().unwrap_or_default().to_string_lossy().to_string(),
            kinds.iter().map(|(k, v)| format!("{}x{}", k, v)).collect::<Vec<_>>().join(", "),
        ));
    }

    worst.sort_by(|a, b| b.0.cmp(&a.0));

    eprintln!();
    eprintln!("================================================================");
    eprintln!("  FRESH CORPUS — decompiled with the CURRENT binary");
    eprintln!("================================================================");
    eprintln!("  {}", dir);
    eprintln!();
    eprintln!("  files              {:>7}", opened);
    eprintln!("  lines              {:>7}", total_lines);
    eprintln!(
        "  clean              {:>7}  ({:.1}%)",
        clean,
        100.0 * clean as f64 / opened.max(1) as f64
    );
    eprintln!("  provably wrong     {:>7}", opened - clean);
    eprintln!("  total defects      {:>7}", total_defects);
    eprintln!();
    eprintln!("  BY CHECK                   files   defects");
    for (k, n) in &by_check {
        eprintln!(
            "    {:<24} {:>6}   {:>7}",
            k,
            files_with.get(k).copied().unwrap_or(0),
            n
        );
    }
    eprintln!();
    eprintln!("  WORST 20 FILES");
    for (n, name, summary) in worst.iter().take(20) {
        let short = if name.len() > 54 { &name[..54] } else { name };
        eprintln!("    {:>3}  {:<54} {}", n, short, summary);
    }
    eprintln!("================================================================");
}
