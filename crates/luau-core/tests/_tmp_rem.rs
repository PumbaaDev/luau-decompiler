//! THROWAWAY: enumerate the remaining defects with file:line.
use std::path::PathBuf;
#[test]
fn remaining() {
    let Ok(dir) = std::env::var("PROBE_DIR") else { eprintln!("SKIP"); return };
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir).expect("dir").flatten()
        .map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "lua")).collect();
    files.sort();
    let (mut rep, mut truth) = (0usize, 0usize);
    for p in &files {
        let Ok(src) = std::fs::read_to_string(p) else { continue };
        for f in luau_core::decompiler::semantic_check::check(&src, None) {
            rep += 1;
            if let Some(c) = f.detail.strip_prefix("... and ") {
                if let Some(n) = c.split_whitespace().next().and_then(|s| s.parse::<usize>().ok()) {
                    truth += n;
                    eprintln!("REM {} | {} | +{} more", f.check, p.file_name().unwrap().to_string_lossy(), n);
                    continue;
                }
            }
            truth += 1;
            eprintln!("REM {} | {} | line {:?} | {}", f.check,
                p.file_name().unwrap().to_string_lossy(), f.line, f.detail.chars().take(70).collect::<String>());
        }
    }
    eprintln!("REPORTED {}  TRUE {}", rep, truth);
}
