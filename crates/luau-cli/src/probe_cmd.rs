//! `probe emit` and `probe align` — read a client's opcode permutation off its
//! own compiler instead of guessing it.
//!
//! The workflow this implements has two halves, and only the second is the
//! decompiler's business:
//!
//! 1. `probe emit` writes a set of small Luau programs to a folder. You compile
//!    them twice: once with upstream `luau-compile` (numbering we know), once
//!    with the client whose numbering you want (whatever mechanism that client
//!    offers for compiling and dumping a chunk).
//!
//! 2. `probe align` reads both folders, pairs files by name, and lines the two
//!    instruction streams up. The permutation falls out of the comparison.
//!
//! Nothing here consults an answer key. The only inputs are source we wrote and
//! the two compilations of it.

use anyhow::{bail, Context, Result};
use luau_core::parser::alignment::{
    self, canonical_opcode_name, Alignment, Conflict, RejectReason, CANONICAL_OPCODE_COUNT,
    UNMAPPED,
};
use luau_core::parser::probe::{self, ProbeTier};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Write the probe source set (and its manifest) to a folder.
pub fn run_emit(out: &Path, tier: Option<&str>) -> Result<()> {
    let filter = match tier {
        None | Some("all") => None,
        Some(t) => Some(
            ProbeTier::parse(t)
                .with_context(|| format!("unknown tier '{}' (expected core, heavy or all)", t))?,
        ),
    };

    fs::create_dir_all(out)
        .with_context(|| format!("creating output folder {}", out.display()))?;

    let mut written = 0usize;
    let mut bytes = 0usize;
    for src in probe::probe_sources() {
        if let Some(t) = filter {
            if src.tier != t {
                continue;
            }
        }
        let text = src.source();
        let path = out.join(src.file_name());
        fs::write(&path, text.as_bytes())
            .with_context(|| format!("writing {}", path.display()))?;
        written += 1;
        bytes += text.len();
    }

    let manifest = out.join("manifest.json");
    fs::write(&manifest, probe::manifest_json(filter))
        .with_context(|| format!("writing {}", manifest.display()))?;

    println!("wrote {} probe sources ({} bytes) to {}", written, bytes, out.display());
    println!("      manifest: {}", manifest.display());
    println!();
    println!("Next:");
    println!("  1. compile each .luau with upstream luau-compile (-O1 -g1, binary output)");
    println!("     into one folder - this is the --canonical side");
    println!("  2. compile the SAME sources with the client whose opcode numbering you");
    println!("     want, and dump each chunk into another folder - the --client side");
    println!("  3. luau-decompiler probe align --canonical <dir> --client <dir> --out map.json");
    Ok(())
}

/// One aligned file, for reporting.
struct FileReport {
    name: String,
    protos_total: usize,
    protos_aligned: usize,
    protos_rejected: usize,
    instructions: usize,
    operand_words: u64,
    pinned: usize,
    first_reject: Option<String>,
    missing_expected: Vec<&'static str>,
}

pub struct AlignOptions<'a> {
    pub canonical: &'a Path,
    pub client: &'a Path,
    pub out: Option<&'a Path>,
    pub id: Option<&'a str>,
    pub min_pinned: usize,
    pub json: bool,
    pub allow_conflicts: bool,
}

/// Derive a permutation from two folders (or two files) of the same programs.
pub fn run_align(opts: &AlignOptions) -> Result<()> {
    let pairs = collect_pairs(opts.canonical, opts.client)?;
    if pairs.is_empty() {
        bail!(
            "no file-name matches between {} and {}\n\
             hint: both sides must contain the same program names, e.g. p01_arith.luac",
            opts.canonical.display(),
            opts.client.display()
        );
    }

    let mut reports: Vec<FileReport> = Vec::new();
    let mut alignments: Vec<Alignment> = Vec::new();
    let mut skipped: Vec<(String, String)> = Vec::new();

    for (name, canon_path, client_path) in &pairs {
        let canon_bytes = fs::read(canon_path)
            .with_context(|| format!("reading {}", canon_path.display()))?;
        let client_bytes = fs::read(client_path)
            .with_context(|| format!("reading {}", client_path.display()))?;

        let canon_chunk = match luau_core::parser::parse(&canon_bytes) {
            Ok(c) => c,
            Err(e) => {
                skipped.push((name.clone(), format!("reference will not parse: {}", e)));
                continue;
            }
        };
        let client_chunk = match luau_core::parser::parse(&client_bytes) {
            Ok(c) => c,
            Err(e) => {
                skipped.push((name.clone(), format!("client dump will not parse: {}", e)));
                continue;
            }
        };

        let missing = probe::probe_source(name)
            .map(|s| probe::missing_expected(s, &canon_chunk))
            .unwrap_or_default();

        match alignment::align_pair(&canon_chunk, &client_chunk) {
            Ok(a) => {
                reports.push(FileReport {
                    name: name.clone(),
                    protos_total: a.protos_total,
                    protos_aligned: a.protos_aligned,
                    protos_rejected: a.protos_rejected.len(),
                    instructions: a.instructions_aligned,
                    operand_words: a.operand_words_checked,
                    pinned: a.pinned(),
                    first_reject: a.protos_rejected.first().map(|r| {
                        format!("proto {}: {}", r.proto, describe_reject(&r.reason))
                    }),
                    missing_expected: missing,
                });
                alignments.push(a);
            }
            Err(e) => skipped.push((name.clone(), e.to_string())),
        }
    }

    let solution = alignment::union_alignments(&alignments);

    if opts.json {
        print!("{}", report_json(&reports, &solution, &skipped));
    } else {
        print_human(&reports, &solution, &skipped);
    }

    if !alignment::is_partial_bijection(&solution.map) {
        bail!("derived map is not a partial bijection - refusing to emit it");
    }

    let pinned = solution.pinned();
    if let Some(out) = opts.out {
        if !solution.conflicts.is_empty() && !opts.allow_conflicts {
            bail!(
                "{} contradiction(s) between files - refusing to write {}.\n\
                 Contradictions mean the inputs are not all from one build, or one\n\
                 alignment is wrong. Re-dump from a single client, or pass\n\
                 --allow-conflicts to write the map with the contradicted bytes left blank.",
                solution.conflicts.len(),
                out.display()
            );
        }
        if pinned < opts.min_pinned {
            bail!(
                "only {} opcodes pinned, below the --min-pinned floor of {} - refusing to \
                 write {}.\nA thin map is worse than no map: it would be installed as exact.",
                pinned,
                opts.min_pinned,
                out.display()
            );
        }
        let doc = solution_json(&solution, opts.id, &reports);
        fs::write(out, doc).with_context(|| format!("writing {}", out.display()))?;
        if !opts.json {
            println!();
            println!("wrote {} ({} opcodes pinned)", out.display(), pinned);
            println!("import it with: luau-decompiler opmap-db import {} --db <db.json>", out.display());
        }
    }

    Ok(())
}

fn describe_reject(reason: &RejectReason) -> String {
    match reason {
        RejectReason::CodeLenMismatch { known, unknown } => format!(
            "{} instructions in the reference, {} in the client dump",
            known, unknown
        ),
        RejectReason::NonCanonicalOpcode { offset, byte } => format!(
            "reference byte 0x{:02X} at word {} is not a canonical opcode \
             (is the --canonical side really upstream Luau?)",
            byte, offset
        ),
        RejectReason::OperandDivergence { offset } => format!(
            "operands differ at word {} - the two compilers emitted different code here",
            offset
        ),
        RejectReason::TruncatedAux { offset } => {
            format!("AUX-carrying opcode at the last word ({})", offset)
        }
        RejectReason::InternalFunctionConflict { shuffled_byte } => format!(
            "byte 0x{:02X} carried two different opcodes within one proto",
            shuffled_byte
        ),
        RejectReason::InternalInjectivityConflict { internal_op } => format!(
            "{} arrived at two different bytes within one proto",
            luau_core::parser::opcodes::LuauOpcode::from_u8(*internal_op).name()
        ),
    }
}

fn describe_conflict(c: &Conflict) -> String {
    match c {
        Conflict::Function {
            shuffled_byte,
            first,
            second,
        } => format!(
            "byte 0x{:02X} read as both {} and {}",
            shuffled_byte,
            luau_core::parser::opcodes::LuauOpcode::from_u8(*first).name(),
            luau_core::parser::opcodes::LuauOpcode::from_u8(*second).name()
        ),
        Conflict::Injectivity {
            internal_op,
            first,
            second,
        } => format!(
            "{} read at both 0x{:02X} and 0x{:02X}",
            luau_core::parser::opcodes::LuauOpcode::from_u8(*internal_op).name(),
            first,
            second
        ),
    }
}

fn print_human(reports: &[FileReport], solution: &Alignment, skipped: &[(String, String)]) {
    let width = reports
        .iter()
        .map(|r| r.name.len())
        .chain(skipped.iter().map(|(n, _)| n.len()))
        .max()
        .unwrap_or(10)
        .max(7);

    println!("{:<width$}  protos   insns   pinned  notes", "file", width = width);
    println!("{}", "-".repeat(width + 40));
    for r in reports {
        let mut note = String::new();
        if r.protos_rejected > 0 {
            note.push_str(&format!("{} rejected", r.protos_rejected));
            if let Some(ref f) = r.first_reject {
                note.push_str(&format!(" ({})", f));
            }
        }
        if !r.missing_expected.is_empty() {
            if !note.is_empty() {
                note.push_str("; ");
            }
            note.push_str(&format!(
                "reference is missing {}",
                r.missing_expected.join(", ")
            ));
        }
        println!(
            "{:<width$}  {:>3}/{:<3}  {:>6}  {:>6}  {}",
            r.name,
            r.protos_aligned,
            r.protos_total,
            r.instructions,
            r.pinned,
            note,
            width = width
        );
    }
    for (name, why) in skipped {
        println!("{:<width$}  {:>3}/{:<3}  {:>6}  {:>6}  SKIPPED: {}", name, 0, 0, 0, 0, why, width = width);
    }

    let words: u64 = reports.iter().map(|r| r.operand_words).sum();
    let rejected: usize = reports.iter().map(|r| r.protos_rejected).sum();
    println!();
    println!(
        "aligned {} of {} files, {} protos rejected, {} operand words verified",
        reports.len(),
        reports.len() + skipped.len(),
        rejected,
        words
    );
    println!(
        "PINNED {} of {} canonical opcodes",
        solution.pinned(),
        CANONICAL_OPCODE_COUNT
    );

    let unpinned = solution.unpinned_names();
    if !unpinned.is_empty() {
        println!("unpinned: {}", unpinned.join(" "));
        let forceable: Vec<&&str> = unpinned
            .iter()
            .filter(|n| !matches!(***n, _ if is_unforceable(n)))
            .collect();
        if forceable.is_empty() {
            println!("          (all four are opcodes no compiler emits - this is the ceiling)");
        }
    }
    if !solution.conflicts.is_empty() {
        println!();
        println!("{} CONTRADICTION(S) - these bytes were left unpinned:", solution.conflicts.len());
        for c in solution.conflicts.iter().take(12) {
            println!("  {}", describe_conflict(c));
        }
        println!("  a contradiction means the inputs are not all from one build");
    }
}

fn is_unforceable(name: &str) -> bool {
    matches!(name, "NOP" | "BREAK" | "NATIVECALL" | "COVERAGE")
}

fn report_json(reports: &[FileReport], solution: &Alignment, skipped: &[(String, String)]) -> String {
    let files: Vec<serde_json::Value> = reports
        .iter()
        .map(|r| {
            serde_json::json!({
                "name": r.name,
                "protos_total": r.protos_total,
                "protos_aligned": r.protos_aligned,
                "protos_rejected": r.protos_rejected,
                "instructions": r.instructions,
                "operand_words": r.operand_words,
                "pinned": r.pinned,
                "first_reject": r.first_reject,
                "reference_missing_expected": r.missing_expected,
            })
        })
        .collect();
    let doc = serde_json::json!({
        "files": files,
        "skipped": skipped.iter().map(|(n, w)| serde_json::json!({"name": n, "reason": w})).collect::<Vec<_>>(),
        "pinned": solution.pinned(),
        "canonical_total": CANONICAL_OPCODE_COUNT,
        "unpinned": solution.unpinned_names(),
        "conflicts": solution.conflicts.iter().map(describe_conflict).collect::<Vec<_>>(),
    });
    serde_json::to_string_pretty(&doc).unwrap_or_default() + "\n"
}

/// The derived map, in the shape `opmap-db import` accepts.
fn solution_json(solution: &Alignment, id: Option<&str>, reports: &[FileReport]) -> String {
    let mut mappings = BTreeMap::new();
    for (b, &internal) in solution.map.iter().enumerate() {
        if internal == UNMAPPED {
            continue;
        }
        mappings.insert(
            format!("0x{:02X}", b),
            luau_core::parser::opcodes::LuauOpcode::from_u8(internal).name(),
        );
    }
    let unpinned = solution.unpinned_names();
    let doc = serde_json::json!({
        "format": "luau-opmap-probe",
        "format_version": 1,
        "id": id.unwrap_or("unnamed"),
        "provenance": {
            "method": "probe-align",
            "producer": concat!("luau-decompiler ", env!("CARGO_PKG_VERSION")),
            "probe_set_version": probe::PROBE_SET_VERSION,
            "probe_programs": reports.len(),
        },
        "coverage": {
            "pinned": solution.pinned(),
            "canonical_total": CANONICAL_OPCODE_COUNT,
            "unpinned": unpinned,
            "conflicts": solution.conflicts.len(),
            "protos_aligned": solution.protos_aligned,
            "protos_rejected": solution.protos_rejected.len(),
            "operand_words_verified": solution.operand_words_checked,
        },
        "semantics": unary_semantics_json(solution),
        "mappings": mappings,
    });
    serde_json::to_string_pretty(&doc).unwrap_or_default() + "\n"
}

/// Record whether this client's compiler really emits NOT / MINUS / LENGTH for
/// `not x`, `-x` and `#x`.
///
/// This is an OBSERVATION, not a deduction. The probe set contains exactly
/// those three expressions; if the client's compiler produced the opcode we
/// call NOT for `not x`, then for this build NOT means `not`. If it produced
/// something else, the byte never gets pinned to NOT and the field stays
/// `passthrough`, which is what the decompiler already assumes.
fn unary_semantics_json(solution: &Alignment) -> serde_json::Value {
    let observed = |name: &str| -> &'static str {
        let pinned = (0..CANONICAL_OPCODE_COUNT as u8).any(|c| {
            canonical_opcode_name(c) == Some(name)
                && alignment::canonical_to_internal(c)
                    .map(|i| solution.inv[i as usize] != UNMAPPED)
                    .unwrap_or(false)
        });
        if pinned {
            "operator"
        } else {
            "passthrough"
        }
    };
    serde_json::json!({
        "unary_not": observed("NOT"),
        "unary_minus": observed("MINUS"),
        "unary_length": observed("LENGTH"),
    })
}

/// Pair files across two folders by stem. Accepts two plain files too.
fn collect_pairs(canonical: &Path, client: &Path) -> Result<Vec<(String, PathBuf, PathBuf)>> {
    if canonical.is_file() && client.is_file() {
        let name = stem(canonical);
        return Ok(vec![(name, canonical.to_path_buf(), client.to_path_buf())]);
    }
    if !canonical.is_dir() {
        bail!("{} is not a folder or file", canonical.display());
    }
    if !client.is_dir() {
        bail!("{} is not a folder or file", client.display());
    }

    let index = |dir: &Path| -> Result<BTreeMap<String, PathBuf>> {
        let mut m = BTreeMap::new();
        for entry in fs::read_dir(dir)? {
            let p = entry?.path();
            if !p.is_file() {
                continue;
            }
            if p.extension().and_then(|e| e.to_str()) == Some("json") {
                continue;
            }
            m.insert(stem(&p), p);
        }
        Ok(m)
    };

    let a = index(canonical)?;
    let b = index(client)?;
    let mut out = Vec::new();
    for (name, ap) in a {
        if let Some(bp) = b.get(&name) {
            out.push((name, ap, bp.clone()));
        }
    }
    Ok(out)
}

/// File stem with every extension removed, so `p01_arith.shuf.luac` and
/// `p01_arith.luac` pair up.
fn stem(p: &Path) -> String {
    let mut s = p
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string();
    while let Some(dot) = s.rfind('.') {
        s.truncate(dot);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stem_strips_every_extension() {
        assert_eq!(stem(Path::new("dir/p01_arith.shuf.luac")), "p01_arith");
        assert_eq!(stem(Path::new("p01_arith.luac")), "p01_arith");
        assert_eq!(stem(Path::new("p01_arith")), "p01_arith");
    }

    #[test]
    fn unforceable_set_is_exactly_the_four() {
        for n in ["NOP", "BREAK", "NATIVECALL", "COVERAGE"] {
            assert!(is_unforceable(n));
        }
        for n in ["LOADKX", "JUMPX", "RETURN", "ADD"] {
            assert!(!is_unforceable(n));
        }
    }

    #[test]
    fn semantics_are_reported_from_observation_not_assumption() {
        let empty = Alignment::empty();
        let v = unary_semantics_json(&empty);
        assert_eq!(v["unary_not"], "passthrough");
        assert_eq!(v["unary_minus"], "passthrough");
        assert_eq!(v["unary_length"], "passthrough");
    }
}
