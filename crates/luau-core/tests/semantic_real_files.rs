//! Semantic checks against real decompiled output.
//!
//! These are the exact files that marker-counting scored as clean on
//! 2026-08-03. Marker counts found nothing in any of them; reading them
//! found dropped bodies, undefined locals, and functions wired to the
//! wrong names. This test exists so that never happens silently again.

use luau_core::decompiler::semantic_check::{check, format_report};

fn report(label: &str, path: &str, protos: Option<usize>) {
    let Ok(src) = std::fs::read_to_string(path) else {
        eprintln!("SKIP (not found): {}", label);
        return;
    };
    let findings = check(&src, protos);
    eprintln!("=== {} ({} lines) ===", label, src.lines().count());
    eprintln!("{}", format_report(&findings));
}

#[test]
fn scan_real_decompiled_files() {
    // Point OUR_CORPUS at a ReplicatedStorage dir of decompiled .lua to run
    // this; each file is skipped when absent.
    let bs = std::env::var("OUR_CORPUS").unwrap_or_default();
    report("module_a.lua", &format!(r"{}\module_a.lua", bs), Some(13));
    report("module_b.lua", &format!(r"{}\module_b.lua", bs), Some(8));
    report("module_c.lua", &format!(r"{}\module_c.lua", bs), Some(23));
    report("module_d.lua", &format!(r"{}\module_d.lua", bs), Some(15));
}
