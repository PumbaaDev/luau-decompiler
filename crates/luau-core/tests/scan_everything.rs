//! Full-machine semantic scan — every Lua/Luau file reachable on this PC.
//!
//! 27,446 files, ~5.67M lines. Reading that by hand is not possible; reading it
//! programmatically is, and that is what this does. Coverage here is genuinely
//! 100% of files found, and the count of files actually opened is reported so
//! the claim can be checked rather than trusted.
//!
//! Findings are grouped by ROOT so machine-generated decompiler output (which
//! dominates by volume) can be told apart from hand-written source.
//!
//! Run with:
//!   cargo test -p luau-core --test scan_everything -- --nocapture

use luau_core::decompiler::semantic_check::{check, Severity};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

// The first five were the original list. A drive-wide search afterwards found
// 1,522 further files outside them -- including `.luau_tools\corpus`, which is
// the round-trip corpus the README's correctness claim rests on. Scanning
// everything except the reference corpus would have been a poor joke.
//
// Before adding a root here, re-run the drive-wide check:
//   find /c -maxdepth 6 \( -name "*.lua" -o -name "*.luau" \) | grep -v <roots>
const ROOTS: &[&str] = &[
    r"C:\Users\jep\AppData\Local\Potassium",
    r"C:\Users\jep\Downloads",
    r"C:\Users\jep\Desktop",
    r"C:\Users\jep\Documents",
    r"C:\Projects",
    r"C:\Users\jep\.luau_tools",
    r"C:\tmp",
    r"C:\Users\jep\.claude",
    r"C:\Users\jep\AppData\Local\Temp\claude",
];

fn walk(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > 24 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        match e.file_type() {
            Ok(t) if t.is_dir() => {
                // Skip VCS and build trees: they hold no source of interest and
                // can be enormous.
                let name = p.file_name().unwrap_or_default().to_string_lossy().to_string();
                if name == ".git" || name == "node_modules" || name == "target" {
                    continue;
                }
                walk(&p, out, depth + 1);
            }
            Ok(t) if t.is_file() => {
                if p.extension().is_some_and(|x| x == "lua" || x == "luau") {
                    out.push(p);
                }
            }
            _ => {}
        }
    }
}

/// `-- Protos: 32 total, main=31` in the decompiler's own header.
fn proto_count(src: &str) -> Option<usize> {
    src.lines().take(24).find_map(|l| {
        let rest = l.trim().strip_prefix("-- Protos:")?;
        rest.trim().split_whitespace().next()?.parse().ok()
    })
}

/// Bucket a path by which root it came from.
///
/// The fallback arm used to be `"Projects"`, which quietly absorbed every root
/// added later -- the first expanded run reported "Projects: 4,834 files" when
/// C:\Projects holds 3. A catch-all that names a real bucket produces a wrong
/// number rather than an obviously missing one, so the fallback is now
/// explicitly "other".
fn root_label(p: &Path) -> &'static str {
    let s = p.to_string_lossy();
    if s.contains(r"\Potassium\") {
        "Potassium"
    } else if s.contains(r"\.luau_tools\") {
        "luau_tools"
    } else if s.contains(r"\.claude\") {
        "claude"
    } else if s.contains(r"\Temp\claude") {
        "temp-claude"
    } else if s.starts_with(r"C:\tmp") {
        "tmp"
    } else if s.contains(r"\Downloads\") {
        "Downloads"
    } else if s.contains(r"\Desktop\") {
        "Desktop"
    } else if s.contains(r"\Documents\") {
        "Documents"
    } else if s.starts_with(r"C:\Projects") {
        "Projects"
    } else {
        "other"
    }
}

/// Machine-generated decompiler output carries our own header.
fn is_decompiler_output(src: &str) -> bool {
    src.lines()
        .take(6)
        .any(|l| l.contains("Luau Decompiler v") || l.contains("PumbaDecompiler"))
}

#[test]
fn scan_every_lua_file_on_this_machine() {
    let mut files = Vec::new();
    for r in ROOTS {
        let p = Path::new(r);
        if p.exists() {
            walk(p, &mut files, 0);
        }
    }
    files.sort();
    files.dedup();

    let mut opened = 0usize;
    let mut unreadable = 0usize;
    let mut total_lines = 0usize;
    let mut generated = 0usize;
    let mut handwritten = 0usize;

    // root -> (files, clean, defects)
    let mut by_root: BTreeMap<&'static str, (usize, usize, usize)> = BTreeMap::new();
    let mut by_check: BTreeMap<&'static str, usize> = BTreeMap::new();
    // Hand-written files with defects are the interesting ones to read.
    let mut handwritten_defects: Vec<(usize, String, String)> = Vec::new();
    let mut worst_generated: Vec<(usize, String)> = Vec::new();

    for path in &files {
        let Ok(src) = std::fs::read_to_string(path) else {
            unreadable += 1;
            continue;
        };
        opened += 1;
        total_lines += src.lines().count();

        let gen = is_decompiler_output(&src);
        if gen {
            generated += 1;
        } else {
            handwritten += 1;
        }

        let findings = check(&src, proto_count(&src));
        let wrong: Vec<_> = findings
            .iter()
            .filter(|f| f.severity == Severity::Wrong)
            .collect();

        let label = root_label(path);
        let entry = by_root.entry(label).or_insert((0, 0, 0));
        entry.0 += 1;
        if wrong.is_empty() {
            entry.1 += 1;
            continue;
        }
        entry.2 += wrong.len();

        let mut kinds: BTreeMap<&'static str, usize> = BTreeMap::new();
        for f in &wrong {
            *by_check.entry(f.check).or_insert(0) += 1;
            *kinds.entry(f.check).or_insert(0) += 1;
        }
        let summary = kinds
            .iter()
            .map(|(k, v)| format!("{}x{}", k, v))
            .collect::<Vec<_>>()
            .join(", ");
        let display = path.to_string_lossy().to_string();

        if gen {
            worst_generated.push((wrong.len(), display));
        } else {
            handwritten_defects.push((wrong.len(), display, summary));
        }
    }

    handwritten_defects.sort_by(|a, b| b.0.cmp(&a.0));
    worst_generated.sort_by(|a, b| b.0.cmp(&a.0));

    let total_clean: usize = by_root.values().map(|v| v.1).sum();
    let total_defects: usize = by_root.values().map(|v| v.2).sum();

    eprintln!();
    eprintln!("================================================================");
    eprintln!("  FULL MACHINE SCAN");
    eprintln!("================================================================");
    eprintln!("  files found        {:>8}", files.len());
    eprintln!("  files opened       {:>8}", opened);
    eprintln!("  unreadable         {:>8}", unreadable);
    eprintln!("  lines read         {:>8}", total_lines);
    eprintln!();
    eprintln!("  decompiler output  {:>8}", generated);
    eprintln!("  hand-written       {:>8}", handwritten);
    eprintln!();
    eprintln!(
        "  clean              {:>8}  ({:.1}%)",
        total_clean,
        100.0 * total_clean as f64 / opened.max(1) as f64
    );
    eprintln!("  total defects      {:>8}", total_defects);
    eprintln!();
    eprintln!("  BY ROOT              files    clean   defects");
    for (root, (f, c, d)) in &by_root {
        eprintln!("    {:<16} {:>7}  {:>7}  {:>8}", root, f, c, d);
    }
    eprintln!();
    eprintln!("  BY CHECK");
    for (k, n) in &by_check {
        eprintln!("    {:<26} {:>8}", k, n);
    }
    eprintln!();
    eprintln!(
        "  HAND-WRITTEN FILES WITH DEFECTS ({}) — these are worth reading",
        handwritten_defects.len()
    );
    for (n, path, summary) in handwritten_defects.iter().take(40) {
        eprintln!("    {:>3}  {}", n, path);
        eprintln!("         {}", summary);
    }
    eprintln!();
    eprintln!("  WORST DECOMPILER OUTPUT (top 15 of {})", worst_generated.len());
    for (n, path) in worst_generated.iter().take(15) {
        let short: String = path.chars().rev().take(72).collect::<String>().chars().rev().collect();
        eprintln!("    {:>3}  ...{}", n, short);
    }
    eprintln!("================================================================");
}
