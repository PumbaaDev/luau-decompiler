//! Compare our output against Potassium's own decompiler.
//!
//! ── WHY THIS MATTERS ────────────────────────────────────────────────────
//! Every quality measure in this project so far has been my own heuristic
//! checks, and twice a fix silently blunted the check that was supposed to
//! catch it (marker counting; then `free_var_decls` masking dropped values as
//! declared-but-nil). A second, independent decompiler is a real oracle: it
//! cannot be blunted by changes to this codebase.
//!
//! Potassium decompiles the same 628 scripts from the same live client. Where
//! it recovers a function and we do not, that is our defect, not a
//! disagreement about style.
//!
//! ── MATCHING ────────────────────────────────────────────────────────────
//! `getscripts()` does NOT return a stable order between calls — index 100 is
//! a different script in each run — so files are matched on the script PATH
//! portion of the name, never the index prefix.
//!
//! Run with:
//!   REF_CORPUS=<dir> OUR_CORPUS=<dir> cargo test -p luau-core --release \
//!     --test compare_reference -- --nocapture

use std::collections::HashMap;
use std::path::PathBuf;

/// Strip the unstable `NNN_` index prefix; what remains identifies the script.
fn path_key(p: &std::path::Path) -> String {
    let stem = p.file_stem().unwrap_or_default().to_string_lossy();
    match stem.find('_') {
        Some(i) if stem[..i].chars().all(|c| c.is_ascii_digit()) => stem[i + 1..].to_string(),
        _ => stem.to_string(),
    }
}

fn load(dir: &str) -> HashMap<String, PathBuf> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "lua"))
        .map(|p| (path_key(&p), p))
        .collect()
}

/// Count `function` declarations, ignoring comment lines.
fn count_functions(src: &str) -> usize {
    src.lines()
        .filter(|l| {
            let t = l.trim();
            !t.starts_with("--")
                && (t.starts_with("function ")
                    || t.starts_with("local function ")
                    || t.contains("= function("))
        })
        .count()
}

fn code_lines(src: &str) -> usize {
    src.lines().filter(|l| !l.trim_start().starts_with("--")).count()
}

#[test]
fn ours_recovers_what_the_reference_recovers() {
    let (Ok(refd), Ok(ourd)) = (std::env::var("REF_CORPUS"), std::env::var("OUR_CORPUS")) else {
        eprintln!("SKIP: set REF_CORPUS and OUR_CORPUS");
        return;
    };
    let reference = load(&refd);
    let ours = load(&ourd);

    let mut pairs = 0usize;
    let mut fn_ref_total = 0usize;
    let mut fn_our_total = 0usize;
    let mut line_ratio_sum = 0f64;
    let mut missing_all_functions: Vec<(usize, String)> = Vec::new();
    let mut severe: Vec<(f64, String, usize, usize)> = Vec::new();
    // Reverse direction: where the REFERENCE loses and we do not.
    let mut ref_total_loss: Vec<(usize, String)> = Vec::new();
    let mut ref_stub: Vec<(usize, usize, String)> = Vec::new();

    for (key, our_path) in &ours {
        let Some(ref_path) = reference.get(key) else { continue };
        let (Ok(our_src), Ok(ref_src)) =
            (std::fs::read_to_string(our_path), std::fs::read_to_string(ref_path))
        else {
            continue;
        };
        let ref_lines = code_lines(&ref_src);
        if ref_lines < 6 {
            continue; // trivial file, nothing to compare
        }
        pairs += 1;

        let rf = count_functions(&ref_src);
        let of = count_functions(&our_src);
        fn_ref_total += rf;
        fn_our_total += of;
        line_ratio_sum += code_lines(&our_src) as f64 / ref_lines as f64;

        // The reference found functions and we found none: total loss.
        if rf >= 2 && of == 0 {
            missing_all_functions.push((rf, key.clone()));
        } else if rf >= 4 && (of as f64) < (rf as f64 * 0.5) {
            severe.push((of as f64 / rf as f64, key.clone(), of, rf));
        }

        // THE REVERSE DIRECTION — deliberately measured.
        //
        // The first version of this test only looked for OUR losses, which
        // cannot answer "which decompiler is better", only "where do we lose".
        // A comparison that can only find faults on one side is not a
        // comparison. The reference is not ground truth either; it has its own
        // failures, and they belong in the same table.
        if of >= 2 && rf == 0 {
            ref_total_loss.push((of, key.clone()));
        }
        // Reference produced almost nothing at all.
        if ref_lines < 12 && code_lines(&our_src) > ref_lines * 3 {
            ref_stub.push((ref_lines, code_lines(&our_src), key.clone()));
        }
    }

    missing_all_functions.sort_by(|a, b| b.0.cmp(&a.0));
    severe.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());

    eprintln!();
    eprintln!("================================================================");
    eprintln!("  OURS vs POTASSIUM REFERENCE");
    eprintln!("================================================================");
    eprintln!("  matched pairs            {:>6}", pairs);
    eprintln!("  functions (reference)    {:>6}", fn_ref_total);
    eprintln!("  functions (ours)         {:>6}", fn_our_total);
    if fn_ref_total > 0 {
        eprintln!(
            "  function recovery        {:>5.1}%",
            100.0 * fn_our_total as f64 / fn_ref_total as f64
        );
    }
    if pairs > 0 {
        eprintln!("  mean line ratio          {:>5.2}", line_ratio_sum / pairs as f64);
    }
    eprintln!();
    eprintln!(
        "  TOTAL FUNCTION LOSS ({} files — reference found functions, we found none)",
        missing_all_functions.len()
    );
    for (n, key) in missing_all_functions.iter().take(15) {
        let short: String = key.chars().rev().take(62).collect::<String>().chars().rev().collect();
        eprintln!("    ref {:>3} fns, ours 0   {}", n, short);
    }
    eprintln!();
    eprintln!("  SEVERE PARTIAL LOSS ({} files, under 50% of reference)", severe.len());
    for (_, key, of, rf) in severe.iter().take(10) {
        let short: String = key.chars().rev().take(56).collect::<String>().chars().rev().collect();
        eprintln!("    {:>3}/{:<3} fns   {}", of, rf, short);
    }

    ref_total_loss.sort_by(|a, b| b.0.cmp(&a.0));
    ref_stub.sort_by(|a, b| b.1.cmp(&a.1));
    eprintln!();
    eprintln!("  ---- REVERSE DIRECTION: where the REFERENCE loses ----");
    eprintln!(
        "  REFERENCE TOTAL LOSS ({} files — we found functions, reference found none)",
        ref_total_loss.len()
    );
    for (n, key) in ref_total_loss.iter().take(15) {
        let short: String = key.chars().rev().take(62).collect::<String>().chars().rev().collect();
        eprintln!("    ours {:>3} fns, ref 0   {}", n, short);
    }
    eprintln!();
    eprintln!("  REFERENCE NEAR-EMPTY ({} files, ref under 12 lines and <1/3 of ours)", ref_stub.len());
    for (rl, ol, key) in ref_stub.iter().take(10) {
        let short: String = key.chars().rev().take(56).collect::<String>().chars().rev().collect();
        eprintln!("    ref {:>3} lines vs ours {:>4}   {}", rl, ol, short);
    }
    eprintln!("================================================================");
}
