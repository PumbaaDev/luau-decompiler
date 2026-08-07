//! DUPCLOSURE must be assigned for chunks that attach protos to a table.
//!
//! ── THE DEFECT ──────────────────────────────────────────────────────────
//! `ReplicatedStorage.Utils.RefCounter` (8 protos) decompiled to:
//!
//!     local v1                     -- never assigned
//!     local v0 = {}
//!     if v0 ~= "__index" then ... end
//!     v0 = v0[1]
//!     v0.new             = v1
//!     v0.AddOnAction     = v1
//!     v0.AddOffAction    = v1
//!     v0.RemoveOnAction  = v1
//!     ...
//!     return v0
//!
//! All seven methods assigned the SAME unbound register. The source is the
//! ordinary metatable-class idiom:
//!
//!     local RefCounter = {}
//!     RefCounter.__index = RefCounter
//!     function RefCounter.new() ... end
//!     function RefCounter:AddOnAction() ... end
//!
//! Disassembling with --opmap shows the SETTABLEKS instructions present and
//! correct (`R0."new" R1`, `R0."AddOnAction" R1`, ...) but **no DUPCLOSURE
//! instruction anywhere**, and DUPCLOSURE absent from the chunk's opcode map.
//! Nothing ever loads R1, so every method reads an unbound register.
//!
//! This is the same failure class as CALL losing its byte to CAPTURE: a
//! detector claims DUPCLOSURE's byte on weak evidence, and because detectors
//! refuse bytes that are already mapped, the correct assignment can never
//! happen afterwards.
//!
//! Sampled across the `bodies_dropped` files in a freshly extracted corpus:
//! 8 of 12 had no DUPCLOSURE in their map.
//!
//! ── WHAT THIS ASSERTS ───────────────────────────────────────────────────
//! Behaviour, not implementation, so it stays valid across client builds with
//! different shuffles: a chunk with several protos that assigns fields to a
//! table must have a closure opcode mapped, and must not emit the same unbound
//! register as the value for every field.

use std::path::{Path, PathBuf};

const CORPUS: &str = r"C:\Users\jep\AppData\Local\Potassium\workspace\bc_extract_1786138100";

fn bytecode_files() -> Vec<PathBuf> {
    let dir = Path::new(CORPUS);
    if !dir.exists() {
        return Vec::new();
    }
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "bin"))
        .collect();
    v.sort();
    v
}

fn proto_count(src: &str) -> usize {
    src.lines()
        .take(24)
        .find_map(|l| {
            l.trim()
                .strip_prefix("-- Protos:")?
                .trim()
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        })
        .unwrap_or(0)
}

/// Same unbound register assigned to three or more distinct table fields.
/// That is the signature of a closure opcode that produced no value.
fn repeated_unbound_field_value(src: &str) -> Option<(String, usize)> {
    use std::collections::HashMap;
    let code: Vec<&str> = src.lines().filter(|l| !l.trim_start().starts_with("--")).collect();

    // Which generated names are ever given a value?
    let mut assigned = std::collections::HashSet::new();
    for l in &code {
        let t = l.trim();
        if let Some(rest) = t.strip_prefix("local ") {
            if let Some((names, _)) = rest.split_once('=') {
                for n in names.split(',') {
                    assigned.insert(n.trim().to_string());
                }
            }
        } else if let Some((lhs, _)) = t.split_once(" = ") {
            if !lhs.contains('.') && !lhs.contains('[') {
                assigned.insert(lhs.trim().to_string());
            }
        }
    }

    // Count `something.Field = vN` where vN was never assigned.
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for l in &code {
        let t = l.trim();
        let Some((lhs, rhs)) = t.split_once(" = ") else { continue };
        if !lhs.contains('.') {
            continue;
        }
        let rhs = rhs.trim();
        let looks_generated = rhs.strip_prefix('v').is_some_and(|r| {
            !r.is_empty() && r.chars().all(|c| c.is_ascii_digit())
        });
        if looks_generated && !assigned.contains(rhs) {
            *counts.entry(rhs).or_insert(0) += 1;
        }
    }
    counts
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .filter(|(_, n)| *n >= 3)
        .map(|(name, n)| (name.to_string(), n))
}

#[test]
fn closure_opcode_is_mapped_for_multi_proto_chunks() {
    let files = bytecode_files();
    if files.is_empty() {
        eprintln!("SKIP: corpus not present");
        return;
    }

    let mut missing = Vec::new();
    for path in &files {
        let Ok(bytes) = std::fs::read(path) else { continue };
        let Ok(src) = luau_core::decompile(&bytes) else { continue };
        if !src.lines().any(|l| l.contains("SHUFFLE MAP")) {
            continue;
        }
        if proto_count(&src) < 4 {
            continue;
        }
        let header: Vec<&str> = src.lines().take_while(|l| l.starts_with("--")).collect();
        let has_dup = header.iter().any(|l| l.ends_with("DUPCLOSURE"));
        let has_new = header.iter().any(|l| l.ends_with("NEWCLOSURE"));
        if !has_dup && !has_new {
            missing.push(
                path.file_stem().unwrap_or_default().to_string_lossy().to_string(),
            );
        }
    }

    assert!(
        missing.is_empty(),
        "no closure opcode mapped in {} multi-proto chunk(s): {}\n\
         A chunk with several protos must load them somehow. If neither \
         DUPCLOSURE nor NEWCLOSURE is mapped, the byte was claimed by another \
         detector and every closure load decodes as something else.",
        missing.len(),
        missing.iter().take(12).cloned().collect::<Vec<_>>().join(", ")
    );
}

/// The consequence test — the one that would have caught RefCounter.
#[test]
fn table_fields_are_not_all_the_same_unbound_register() {
    let files = bytecode_files();
    if files.is_empty() {
        eprintln!("SKIP: corpus not present");
        return;
    }

    let mut broken = Vec::new();
    for path in &files {
        let Ok(bytes) = std::fs::read(path) else { continue };
        let Ok(src) = luau_core::decompile(&bytes) else { continue };
        if let Some((reg, n)) = repeated_unbound_field_value(&src) {
            broken.push(format!(
                "{} ({} fields all = {})",
                path.file_stem().unwrap_or_default().to_string_lossy(),
                n,
                reg
            ));
        }
    }

    assert!(
        broken.is_empty(),
        "{} chunk(s) assign the same UNBOUND register to 3+ table fields: {}\n\
         That is the signature of a closure opcode producing no value: every \
         `Class.Method = <closure>` collapses to the same never-assigned name, \
         so the module exports nothing callable.",
        broken.len(),
        broken.iter().take(12).cloned().collect::<Vec<_>>().join("; ")
    );
}
