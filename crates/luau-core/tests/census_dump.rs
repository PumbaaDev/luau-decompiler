//! Census dump — machine-readable listing of every semantic finding.
//!
//! Not a gate: this exists so the 218-defect census can be grouped and
//! diffed offline. Prints TSV to stderr: file<TAB>check<TAB>line<TAB>detail.
//!
//! Run with:
//!   OUR_CORPUS=<dir> cargo test -p luau-core --release --test census_dump -- --nocapture

use luau_core::decompiler::semantic_check::{check, Severity};
use std::path::PathBuf;

#[test]
fn dump_all_findings() {
    let Ok(dir) = std::env::var("OUR_CORPUS") else {
        eprintln!("SKIP: set OUR_CORPUS=<dir>");
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

    for path in &files {
        let Ok(src) = std::fs::read_to_string(path) else { continue };
        let protos = src.lines().take(20).find_map(|l| {
            l.trim()
                .strip_prefix("-- Protos:")?
                .trim()
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        });
        let stem = path.file_stem().unwrap_or_default().to_string_lossy();
        for f in check(&src, protos) {
            if f.severity != Severity::Wrong {
                continue;
            }
            eprintln!(
                "TSV\t{}\t{}\t{}\t{}",
                stem,
                f.check,
                f.line.map(|l| l.to_string()).unwrap_or_default(),
                f.detail.replace('\t', " ").replace('\n', " ")
            );
        }
    }
}
