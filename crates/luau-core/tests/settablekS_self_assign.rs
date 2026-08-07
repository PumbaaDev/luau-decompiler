//! `SETTABLEKS Rn.key Rn` — a table storing a register into itself — is
//! meaningless, and its presence means the byte is not really SETTABLEKS.
//!
//! ── THE DEFECT ──────────────────────────────────────────────────────────
//! `ReplicatedStorage.Stickers.StickerPlacer`, proto 11:
//!
//!     0: LOADK       R2 K0 "GetCanvasTouchPoint"   -- R2 = a STRING
//!     1: SETTABLEKS  R2."GetCanvasTouchPoint" R2   -- R2[key] = R2
//!     3: MOVE        R3 R1
//!     4: CALL        R2 args=1 results=4           -- calls R2
//!
//! `CALL args=1` means ONE argument, so this is a plain call `f(arg)` and R2
//! must hold a FUNCTION. Instructions 0-1 do not produce one.
//!
//! The reference decompiler settles what the source actually was:
//!
//!     local v39, v40, v41, v42 = u1.GetCanvasTouchPoint(p38);
//!
//! -- a field READ from an upvalue. So instruction 1 is GETTABLEKS, not
//! SETTABLEKS, and the byte (0x4D in this chunk) is mis-assigned.
//!
//! The lifter is blameless: given `SETTABLEKS Rn.k Rn` it faithfully emitted
//! `{GetCanvasTouchPoint = "GetCanvasTouchPoint"}`. This is an opcode-mapping
//! fault, the same family as the CALL and DUPCLOSURE byte theft.
//!
//! ── WHY A RULE AND NOT A BYTE FIX ───────────────────────────────────────
//! The permutation is per-client-build and shared across all 628 chunks, so
//! hard-coding "0x4D is GETTABLEKS" fits one file and risks the other 627.
//! Every fix that has held up in this project changed a DETECTOR RULE; the
//! ones that went wrong reasoned from a single instance.
//!
//! The discriminant is structural and needs no reference: a real SETTABLEKS
//! never has A == B. Storing a register into a field of ITSELF is not
//! something a compiler emits. A candidate byte that produces them is not
//! SETTABLEKS.
//!
//! Run with:
//!   BC_CORPUS=<dir> cargo test -p luau-core --release \
//!     --test settablekS_self_assign -- --nocapture

use std::path::{Path, PathBuf};

fn corpus() -> Vec<PathBuf> {
    let dir = std::env::var("BC_CORPUS").unwrap_or_else(|_| {
        r"C:\Users\jep\AppData\Local\Potassium\workspace\bc_extract_1786138100".to_string()
    });
    let p = Path::new(&dir);
    if !p.exists() {
        return Vec::new();
    }
    let mut v: Vec<PathBuf> = std::fs::read_dir(p)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "bin"))
        .collect();
    v.sort();
    v
}

/// Count `Tbl.field = Tbl` self-assignments in decompiled source, and the
/// `{ key = "key" }` tables the lifter builds from them.
///
/// Both shapes are downstream of the same bad decode. A table whose every
/// entry is `X = "X"` is what `SETTABLEKS Rn."X" Rn` becomes once the lifter
/// folds the self-store into a constructor.
fn self_assign_artifacts(src: &str) -> usize {
    let mut n = 0;
    for raw in src.lines() {
        let t = raw.trim();
        if t.starts_with("--") {
            continue;
        }
        // `something.Field = something`  (same name both sides)
        if let Some((lhs, rhs)) = t.split_once(" = ") {
            let rhs = rhs.trim().trim_end_matches(',');
            if let Some((base, _field)) = lhs.trim().rsplit_once('.') {
                if base.trim() == rhs && !rhs.is_empty() {
                    n += 1;
                    continue;
                }
            }
            // `Key = "Key"` — the constructor form of the same defect
            let key = lhs.trim();
            if rhs.len() >= 2 && rhs.starts_with('"') && rhs.ends_with('"') {
                if &rhs[1..rhs.len() - 1] == key {
                    n += 1;
                }
            }
        }
    }
    n
}

#[test]
fn no_self_assigning_settablekS() {
    let files = corpus();
    if files.is_empty() {
        eprintln!("SKIP: bytecode corpus not present");
        return;
    }

    let mut offenders: Vec<(String, usize)> = Vec::new();
    let mut scanned = 0usize;
    let mut total = 0usize;

    for path in &files {
        let Ok(bytes) = std::fs::read(path) else { continue };
        let Ok(src) = luau_core::decompile(&bytes) else { continue };
        scanned += 1;
        let n = self_assign_artifacts(&src);
        if n > 0 {
            total += n;
            offenders.push((
                path.file_stem().unwrap_or_default().to_string_lossy().to_string(),
                n,
            ));
        }
    }
    offenders.sort_by(|a, b| b.1.cmp(&a.1));

    eprintln!();
    eprintln!("  scanned {} chunks", scanned);
    eprintln!("  chunks with self-assign artifacts: {}", offenders.len());
    eprintln!("  total artifacts: {}", total);
    for (name, n) in offenders.iter().take(12) {
        let short: String = name.chars().take(56).collect();
        eprintln!("    {:>4}  {}", n, short);
    }

    assert!(
        offenders.is_empty(),
        "{} chunk(s) contain {} self-assignment artifacts. `SETTABLEKS Rn.k Rn` \
         is meaningless — a compiler never stores a register into a field of \
         itself — so the byte assigned to SETTABLEKS is really another opcode \
         (GETTABLEKS in the cases checked against the reference).",
        offenders.len(),
        total
    );
}
