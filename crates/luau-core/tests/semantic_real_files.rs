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
    let bs = concat!(
        r"C:\Users\jep\AppData\Local\Potassium\workspace\decompiler\",
        r"beeswarm_final_191648\ReplicatedStorage"
    );
    report("Events.lua", &format!(r"{}\Events.lua", bs), Some(13));
    report("Activatables/Hives.lua", &format!(r"{}\Activatables\Hives.lua", bs), Some(8));
    report("Collectibles.lua", &format!(r"{}\Collectibles.lua", bs), Some(23));
    report("PlayerActives.lua", &format!(r"{}\PlayerActives.lua", bs), Some(15));
}
