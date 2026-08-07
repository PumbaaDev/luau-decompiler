//! Corpus-wide semantic scan — the real quality baseline.
//!
//! Marker counting said the corpus was in decent shape. Reading four files by
//! hand found 16 provably wrong defects that every marker check scored as
//! clean. This walks all 1,273 files so the baseline is measured rather than
//! assumed.
//!
//! Run with:
//!   cargo test -p luau-core --test semantic_corpus_scan -- --nocapture

use luau_core::decompiler::semantic_check::{check, Severity};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else if p.extension().is_some_and(|x| x == "lua" || x == "luau") {
            out.push(p);
        }
    }
}

/// Protos are recorded in the header comment the decompiler emits:
///   `-- Protos: 32 total, main=31`
fn proto_count(src: &str) -> Option<usize> {
    src.lines().take(20).find_map(|l| {
        let rest = l.trim().strip_prefix("-- Protos:")?;
        rest.trim().split_whitespace().next()?.parse().ok()
    })
}

#[test]
fn scan_whole_corpus() {
    let root = Path::new(
        r"C:\Users\jep\AppData\Local\Potassium\workspace\decompiler\beeswarm_final_191648",
    );
    if !root.exists() {
        eprintln!("SKIP: corpus not present at {}", root.display());
        return;
    }

    let mut files = Vec::new();
    walk(root, &mut files);
    files.sort();

    let total = files.len();
    let mut clean = 0usize;
    let mut by_check: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut files_with: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut worst: Vec<(usize, String, String)> = Vec::new();
    let mut total_wrong = 0usize;

    for path in &files {
        let Ok(src) = std::fs::read_to_string(path) else { continue };
        let findings = check(&src, proto_count(&src));
        let wrong: Vec<_> = findings
            .iter()
            .filter(|f| f.severity == Severity::Wrong)
            .collect();

        if wrong.is_empty() {
            clean += 1;
            continue;
        }
        total_wrong += wrong.len();

        let mut seen_here: BTreeMap<&'static str, usize> = BTreeMap::new();
        for f in &wrong {
            *by_check.entry(f.check).or_insert(0) += 1;
            *seen_here.entry(f.check).or_insert(0) += 1;
        }
        for k in seen_here.keys() {
            *files_with.entry(k).or_insert(0) += 1;
        }

        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        let summary = seen_here
            .iter()
            .map(|(k, v)| format!("{}x{}", k, v))
            .collect::<Vec<_>>()
            .join(", ");
        worst.push((wrong.len(), rel, summary));
    }

    worst.sort_by(|a, b| b.0.cmp(&a.0));

    let broken = total - clean;
    eprintln!();
    eprintln!("========================================================");
    eprintln!("  SEMANTIC BASELINE — {} files", total);
    eprintln!("========================================================");
    eprintln!(
        "  clean                {:>5}  ({:.1}%)",
        clean,
        100.0 * clean as f64 / total as f64
    );
    eprintln!(
        "  provably wrong       {:>5}  ({:.1}%)",
        broken,
        100.0 * broken as f64 / total as f64
    );
    eprintln!("  total defects        {:>5}", total_wrong);
    eprintln!();
    eprintln!("  BY CHECK          files   defects");
    for (k, n) in &by_check {
        eprintln!(
            "    {:<26} {:>4}   {:>6}",
            k,
            files_with.get(k).copied().unwrap_or(0),
            n
        );
    }
    eprintln!();
    eprintln!("  WORST 15 FILES");
    for (n, rel, summary) in worst.iter().take(15) {
        let short = if rel.len() > 58 { &rel[rel.len() - 58..] } else { rel };
        eprintln!("    {:>3}  {:<58} {}", n, short, summary);
    }
    eprintln!("========================================================");
}
