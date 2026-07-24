//! Compare two Luau source files.
//!
//! Produces:
//!   1. A line-level Myers diff (standard O(ND) LCS reconstruction, small
//!      and self-contained — no new deps).
//!   2. Jaccard similarity: |A ∩ B| / |A ∪ B| over whitespace-normalized
//!      non-blank lines. 1.0 = identical, 0.0 = no overlap.
//!   3. Exact-equal flag and summary counts.

use crate::ansi::Colors;
use std::collections::HashSet;

/// Full comparison result.
#[derive(Debug)]
pub struct CompareReport {
    pub identical: bool,
    pub jaccard: f64,
    pub lines_a: usize,
    pub lines_b: usize,
    pub added: usize,
    pub removed: usize,
    pub unchanged: usize,
    pub diff: Vec<DiffOp>,
}

/// One row in the diff output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffOp {
    Same(String),
    Add(String),
    Remove(String),
}

/// Main entry point.
pub fn compare(a: &str, b: &str) -> CompareReport {
    let a_lines: Vec<&str> = a.lines().collect();
    let b_lines: Vec<&str> = b.lines().collect();

    let identical = a == b;
    let jaccard = jaccard_similarity(&a_lines, &b_lines);
    let diff = lcs_diff(&a_lines, &b_lines);

    let mut added = 0usize;
    let mut removed = 0usize;
    let mut unchanged = 0usize;
    for op in &diff {
        match op {
            DiffOp::Add(_) => added += 1,
            DiffOp::Remove(_) => removed += 1,
            DiffOp::Same(_) => unchanged += 1,
        }
    }

    CompareReport {
        identical,
        jaccard,
        lines_a: a_lines.len(),
        lines_b: b_lines.len(),
        added,
        removed,
        unchanged,
        diff,
    }
}

/// Jaccard similarity over whitespace-trimmed non-blank lines.
fn jaccard_similarity(a: &[&str], b: &[&str]) -> f64 {
    let set_a: HashSet<String> = a
        .iter()
        .map(|l| normalize_line(l))
        .filter(|l| !l.is_empty())
        .collect();
    let set_b: HashSet<String> = b
        .iter()
        .map(|l| normalize_line(l))
        .filter(|l| !l.is_empty())
        .collect();

    if set_a.is_empty() && set_b.is_empty() {
        return 1.0;
    }
    let inter = set_a.intersection(&set_b).count();
    let union = set_a.union(&set_b).count();
    if union == 0 {
        0.0
    } else {
        inter as f64 / union as f64
    }
}

/// Collapse runs of whitespace and trim — so trivial indentation/formatting
/// differences don't register as "different lines" for the similarity score.
fn normalize_line(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut last_was_ws = true;
    for ch in line.chars() {
        if ch.is_whitespace() {
            if !last_was_ws {
                out.push(' ');
            }
            last_was_ws = true;
        } else {
            out.push(ch);
            last_was_ws = false;
        }
    }
    // drop trailing space
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// LCS-based diff. O(m*n) time, O(m*n) space — fine for the
/// few-thousand-line Luau files we care about.
fn lcs_diff(a: &[&str], b: &[&str]) -> Vec<DiffOp> {
    let m = a.len();
    let n = b.len();
    if m == 0 {
        return b.iter().map(|l| DiffOp::Add(l.to_string())).collect();
    }
    if n == 0 {
        return a.iter().map(|l| DiffOp::Remove(l.to_string())).collect();
    }

    // Build LCS table.
    let mut dp = vec![vec![0u32; n + 1]; m + 1];
    for i in 1..=m {
        for j in 1..=n {
            if a[i - 1] == b[j - 1] {
                dp[i][j] = dp[i - 1][j - 1] + 1;
            } else {
                dp[i][j] = dp[i - 1][j].max(dp[i][j - 1]);
            }
        }
    }

    // Backtrack.
    let mut ops: Vec<DiffOp> = Vec::new();
    let mut i = m;
    let mut j = n;
    while i > 0 || j > 0 {
        if i > 0 && j > 0 && a[i - 1] == b[j - 1] {
            ops.push(DiffOp::Same(a[i - 1].to_string()));
            i -= 1;
            j -= 1;
        } else if j > 0 && (i == 0 || dp[i][j - 1] >= dp[i - 1][j]) {
            ops.push(DiffOp::Add(b[j - 1].to_string()));
            j -= 1;
        } else {
            ops.push(DiffOp::Remove(a[i - 1].to_string()));
            i -= 1;
        }
    }
    ops.reverse();
    ops
}

/// Print the report and return an exit code (0 = identical, 1 = differ).
pub fn print_report(
    a_label: &str,
    b_label: &str,
    r: &CompareReport,
    stats_only: bool,
    max_lines: usize,
    c: &Colors,
) -> i32 {
    println!(
        "{}Compare{}  {}{}{}  vs  {}{}{}",
        c.bold, c.reset, c.cyan, a_label, c.reset, c.cyan, b_label, c.reset
    );
    println!(
        "  {}lines{}   a={}  b={}",
        c.dim, c.reset, r.lines_a, r.lines_b
    );
    println!(
        "  {}changes{} {}+{}{}  {}-{}{}  {}={}{}",
        c.dim, c.reset,
        c.green, r.added, c.reset,
        c.red, r.removed, c.reset,
        c.blue, r.unchanged, c.reset
    );
    let pct = r.jaccard * 100.0;
    let colored_pct = match () {
        _ if r.jaccard >= 0.95 => c.green,
        _ if r.jaccard >= 0.70 => c.yellow,
        _ => c.red,
    };
    println!(
        "  {}jaccard{} {}{:.4}{} ({}{:.2}%{} line overlap)",
        c.dim, c.reset, colored_pct, r.jaccard, c.reset, colored_pct, pct, c.reset
    );
    println!(
        "  {}status{}  {}",
        c.dim,
        c.reset,
        if r.identical {
            format!("{}identical{}", c.green, c.reset)
        } else {
            format!("{}differ{}", c.yellow, c.reset)
        }
    );

    if !stats_only && !r.identical {
        println!();
        println!("{}--- {}{}", c.red, a_label, c.reset);
        println!("{}+++ {}{}", c.green, b_label, c.reset);
        let limit = if max_lines == 0 { usize::MAX } else { max_lines };
        let mut printed = 0usize;
        let mut truncated = 0usize;
        for op in &r.diff {
            if printed >= limit {
                truncated += 1;
                continue;
            }
            match op {
                DiffOp::Same(s) => {
                    println!(" {}", s);
                }
                DiffOp::Add(s) => {
                    println!("{}+{}{}", c.green, s, c.reset);
                }
                DiffOp::Remove(s) => {
                    println!("{}-{}{}", c.red, s, c.reset);
                }
            }
            printed += 1;
        }
        if truncated > 0 {
            println!(
                "{}... {} more diff lines suppressed (use --max-lines 0 to show all){}",
                c.dim, truncated, c.reset
            );
        }
    }

    if r.identical {
        0
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_reports_identical() {
        let src = "a\nb\nc\n";
        let r = compare(src, src);
        assert!(r.identical);
        assert_eq!(r.added, 0);
        assert_eq!(r.removed, 0);
        assert!((r.jaccard - 1.0).abs() < 1e-9);
    }

    #[test]
    fn completely_different_reports_zero_similarity() {
        let r = compare("a\nb\nc", "x\ny\nz");
        assert!(!r.identical);
        assert!(r.jaccard < 1e-9);
        assert_eq!(r.added, 3);
        assert_eq!(r.removed, 3);
        assert_eq!(r.unchanged, 0);
    }

    #[test]
    fn partial_overlap_between_0_and_1() {
        let r = compare("a\nb\nc\nd", "a\nb\nx\ny");
        assert!(!r.identical);
        assert!(r.jaccard > 0.0 && r.jaccard < 1.0);
        // {a,b,c,d} ∩ {a,b,x,y} = {a,b} (2), union = 6 → 2/6 ≈ 0.333
        let expected = 2.0 / 6.0;
        assert!(
            (r.jaccard - expected).abs() < 1e-9,
            "jaccard={} expected≈{}",
            r.jaccard,
            expected
        );
    }

    #[test]
    fn normalize_ignores_whitespace() {
        let r = compare("  a  \n  b  \n", "a\nb\n");
        // Lines literally differ in indent so diff will show edits, but
        // Jaccard over normalized lines should be 1.0.
        assert!((r.jaccard - 1.0).abs() < 1e-9);
    }

    #[test]
    fn empty_files_are_identical() {
        let r = compare("", "");
        assert!(r.identical);
        assert!((r.jaccard - 1.0).abs() < 1e-9);
        assert_eq!(r.diff.len(), 0);
    }

    #[test]
    fn empty_vs_nonempty_has_zero_jaccard() {
        let r = compare("", "a\nb\n");
        assert!(!r.identical);
        // empty set vs {a,b}: inter=0, union=2 → 0.0
        assert!((r.jaccard - 0.0).abs() < 1e-9);
        assert_eq!(r.added, 2);
        assert_eq!(r.removed, 0);
    }

    #[test]
    fn diff_preserves_unchanged_lines() {
        let r = compare("a\nb\nc\n", "a\nX\nc\n");
        // Should have 2 Same and 1 Add and 1 Remove
        assert_eq!(r.unchanged, 2);
        assert_eq!(r.added, 1);
        assert_eq!(r.removed, 1);
    }

    #[test]
    fn diff_handles_pure_addition() {
        let r = compare("a\n", "a\nb\nc\n");
        assert_eq!(r.unchanged, 1);
        assert_eq!(r.added, 2);
        assert_eq!(r.removed, 0);
    }

    #[test]
    fn diff_handles_pure_removal() {
        let r = compare("a\nb\nc\n", "a\n");
        assert_eq!(r.unchanged, 1);
        assert_eq!(r.added, 0);
        assert_eq!(r.removed, 2);
    }

    #[test]
    fn normalize_line_collapses_whitespace() {
        assert_eq!(normalize_line("  a   b  "), "a b");
        assert_eq!(normalize_line("\t\tfoo\t"), "foo");
        assert_eq!(normalize_line(""), "");
        assert_eq!(normalize_line("   "), "");
    }
}
