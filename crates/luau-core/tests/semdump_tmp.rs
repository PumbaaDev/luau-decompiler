//! Throwaway: dump per-file semantic findings for one run dir. Delete after use.
use luau_core::decompiler::semantic_check::{check, Severity};

#[test]
fn semdump() {
    let dir_owned = std::env::var("SEMDUMP_DIR").unwrap_or_else(|_| {
        r"C:\Users\jep\Downloads\luau-decompiler-v2\target\full1143\decompiled\run_20260809_232523"
            .to_string()
    });
    let dir = std::path::Path::new(&dir_owned);
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "lua"))
        .collect();
    files.sort();
    for p in &files {
        let src = std::fs::read_to_string(p).unwrap();
        let protos = src.lines().take(20).find_map(|l| {
            l.trim()
                .strip_prefix("-- Protos:")?
                .trim()
                .split_whitespace()
                .next()?
                .parse::<usize>()
                .ok()
        });
        for f in check(&src, protos) {
            if f.severity == Severity::Wrong {
                println!(
                    "{}\t{}\t{:?}\t{}",
                    p.file_name().unwrap().to_string_lossy(),
                    f.check,
                    f.line,
                    f.detail
                );
            }
        }
    }
}
