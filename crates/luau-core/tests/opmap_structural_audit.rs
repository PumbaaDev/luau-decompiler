//! Structural audit: does the production opcode map silently mis-decode chunks?
//!
//! The premise, from docs/GENERATED_NAME_ORIGINS.md: "zero unmapped" does not
//! mean "correctly mapped". A byte assigned the WRONG opcode is silent — no
//! unresolved count, a plausible instruction, full coverage reported. The
//! reproducer `258_..._SunBear2Init` shows 541 constants but zero constant-load
//! opcodes under the production map, which is impossible for valid bytecode.
//!
//! This test measures that impossibility across the whole corpus, using facts
//! that cannot lie:
//!
//!   * The CONSTANT TABLE is parsed structurally from the chunk header — no
//!     opcode map involved. It tells us, per proto, exactly which constant
//!     TYPES exist (Import, Closure, Table, String, Number).
//!
//!   * Three Luau invariants have exactly ONE consumer opcode each:
//!       - an Import  constant is loaded ONLY by GETIMPORT
//!       - a  Closure constant is loaded ONLY by DUPCLOSURE
//!       - a  Table   template  is loaded ONLY by DUPTABLE
//!     So "Import constant present, but zero GETIMPORT decoded" is a proven
//!     contradiction: the map is wrong for that proto. The constant side is
//!     map-free ground truth; only the opcode side depends on the map, and that
//!     is exactly the side under suspicion.
//!
//! The opcode side is read from `disassemble_with_opmap` — the SAME production
//! function the `disassemble --opmap --opmap-cache` path uses, i.e. the exact
//! view the reproducer evidence in the doc came from. The pool is built the same
//! way `batch` builds it: one ballot per file, pooled before decoding.
//!
//! Run: set AUDIT_CORPUS to a folder of .bin files, then
//!   cargo test -p luau-core --release --test opmap_structural_audit -- --nocapture

use luau_core::parser::consensus::{decode_book, encode_ballot, ConsensusConfig};
use luau_core::parser::types::Constant;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Per-proto constant-type census — parsed structurally, map-free.
#[derive(Default, Clone)]
struct Census {
    import: usize,
    closure: usize,
    table: usize,
    string: usize,
    number: usize,
    total: usize,
}

/// Per-proto opcode tally, read under the production map.
#[derive(Default, Clone)]
struct OpTally {
    getimport: usize,
    dupclosure: usize,
    duptable: usize,
    // The doc's "constant-load" set, verbatim.
    loadk: usize,
    loadn: usize,
    loadb: usize,
    loadkx: usize,
    // Any opcode that references the constant table at all.
    const_consuming: usize,
    total_insns: usize,
}

const CONST_CONSUMING: &[&str] = &[
    "LOADK", "LOADKX", "GETIMPORT", "GETGLOBAL", "SETGLOBAL", "GETTABLEKS",
    "SETTABLEKS", "NAMECALL", "DUPTABLE", "DUPCLOSURE", "JUMPXEQKNIL",
    "JUMPXEQKB", "JUMPXEQKN", "JUMPXEQKS", "FASTCALL2K",
];

fn is_opcode_name(tok: &str) -> bool {
    !tok.is_empty()
        && tok.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && tok.chars().next().is_some_and(|c| c.is_ascii_uppercase())
}

/// Parse `disassemble_with_opmap` text into a per-proto opcode tally.
///
/// Proto boundaries come from the `; === Proto N ...` headers the disassembler
/// prints; instruction lines have the fixed shape
/// `  <pc>: [0xNN] [Lnn] NAME  operands`.
fn tally_by_proto(disasm: &str) -> BTreeMap<usize, OpTally> {
    let mut out: BTreeMap<usize, OpTally> = BTreeMap::new();
    let mut cur: Option<usize> = None;

    for line in disasm.lines() {
        if let Some(rest) = line.strip_prefix("; === Proto ") {
            if let Some(n) = rest.split_whitespace().next().and_then(|s| s.parse::<usize>().ok()) {
                cur = Some(n);
                out.entry(n).or_default();
            }
            continue;
        }
        // Instruction line: leading spaces, digits, ':', ' [0x..]'
        let t = line.trim_start();
        let Some(colon) = t.find(':') else { continue };
        if colon == 0 || !t[..colon].chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let after = t[colon + 1..].trim_start();
        let Some(after) = after.strip_prefix("[0x") else { continue };
        // Skip past the raw byte "NN] "
        let Some(close) = after.find(']') else { continue };
        let mut fields = after[close + 1..].trim_start();
        // Optional [Lnn] line marker
        if let Some(l) = fields.strip_prefix("[L") {
            if let Some(rb) = l.find(']') {
                fields = l[rb + 1..].trim_start();
            }
        }
        let name = fields.split_whitespace().next().unwrap_or("");
        if !is_opcode_name(name) {
            continue;
        }
        let Some(p) = cur else { continue };
        let e = out.entry(p).or_default();
        e.total_insns += 1;
        match name {
            "GETIMPORT" => e.getimport += 1,
            "DUPCLOSURE" => e.dupclosure += 1,
            "DUPTABLE" => e.duptable += 1,
            "LOADK" => e.loadk += 1,
            "LOADN" => e.loadn += 1,
            "LOADB" => e.loadb += 1,
            "LOADKX" => e.loadkx += 1,
            _ => {}
        }
        if CONST_CONSUMING.contains(&name) {
            e.const_consuming += 1;
        }
    }
    out
}

fn census_by_proto(chunk: &luau_core::parser::types::Chunk) -> Vec<Census> {
    chunk
        .protos
        .iter()
        .map(|p| {
            let mut c = Census::default();
            for k in &p.constants {
                c.total += 1;
                match k {
                    Constant::Import(_) => c.import += 1,
                    Constant::Closure(_) => c.closure += 1,
                    Constant::Table(_) => c.table += 1,
                    Constant::String(_) => c.string += 1,
                    Constant::Number(_) => c.number += 1,
                    _ => {}
                }
            }
            c
        })
        .collect()
}

/// The corpus to audit, or `None` when `AUDIT_CORPUS` is unset. Behaves like
/// `mint_trace`: a no-op when its env switch is off, so the default `cargo test`
/// suite stays green without a private corpus on disk.
fn corpus_dir() -> Option<PathBuf> {
    std::env::var_os("AUDIT_CORPUS")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn bin_files(dir: &PathBuf) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("read corpus dir")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("bin"))
        .collect();
    v.sort();
    v
}

/// Build the pool exactly as `batch` does: one ballot per file.
fn build_pool(files: &[PathBuf]) -> String {
    let mut lines = Vec::new();
    for f in files {
        if let Ok(data) = std::fs::read(f) {
            if let Some(b) = luau_core::observe_ballot(&data) {
                lines.push(encode_ballot(&b));
            }
        }
    }
    lines.join("\n")
}

fn resolve_prior(pool_text: &str, data: &[u8]) -> Option<[u8; 256]> {
    let ballot = luau_core::observe_ballot(data)?;
    let book = decode_book(pool_text);
    let cfg = ConsensusConfig::default();
    let resolved = book.resolve_for(&ballot, &cfg);
    if resolved.is_empty() {
        None
    } else {
        Some(resolved.map)
    }
}

/// Parse the `;   0xNN -> DD NAME` map header lines: NAME -> shuffled byte.
fn parse_map_header(disasm: &str) -> BTreeMap<String, u8> {
    let mut m = BTreeMap::new();
    for line in disasm.lines() {
        let Some(rest) = line.strip_prefix(";   0x") else { continue };
        // rest = "6F -> 26 JUMPIFNOT"
        let Some(byte) = rest.get(..2).and_then(|h| u8::from_str_radix(h, 16).ok()) else { continue };
        let Some(name) = rest.split_whitespace().last() else { continue };
        m.insert(name.to_string(), byte);
    }
    m
}

/// The A-register printed for a conditional-jump line, e.g. "R8 -> +42" -> 8.
fn jump_reg(fields_after_name: &str) -> Option<u32> {
    let t = fields_after_name.trim_start();
    let r = t.strip_prefix('R')?;
    let end = r.find(|c: char| !c.is_ascii_digit()).unwrap_or(r.len());
    r[..end].parse().ok()
}

/// Longest run of the same JUMPIF* opcode with strictly incrementing A register.
/// Real branches never come in long consecutive-register runs; a run of loads
/// mis-decoded as conditional jumps does. Returns the number of instructions
/// that belong to such runs (length >= MIN_RUN) in this proto.
fn masquerade_run_insns(lines_by_proto: &[(String, String)]) -> usize {
    const MIN_RUN: usize = 5;
    let is_condjump = |n: &str| n.starts_with("JUMPIF");
    let mut total = 0usize;
    let mut run_op = String::new();
    let mut run_len = 0usize;
    let mut last_reg: Option<u32> = None;
    let flush = |run_len: usize, total: &mut usize| {
        if run_len >= MIN_RUN {
            *total += run_len;
        }
    };
    for (name, rest) in lines_by_proto {
        let reg = jump_reg(rest);
        if is_condjump(name) {
            let continues = name == &run_op
                && matches!((last_reg, reg), (Some(p), Some(c)) if c == p + 1);
            if continues {
                run_len += 1;
            } else {
                flush(run_len, &mut total);
                run_op = name.clone();
                run_len = 1;
            }
            last_reg = reg;
        } else {
            flush(run_len, &mut total);
            run_op.clear();
            run_len = 0;
            last_reg = None;
        }
    }
    flush(run_len, &mut total);
    total
}

/// Extract (opcode_name, operands_after_name) per proto from the disasm body.
fn body_by_proto(disasm: &str) -> BTreeMap<usize, Vec<(String, String)>> {
    let mut out: BTreeMap<usize, Vec<(String, String)>> = BTreeMap::new();
    let mut cur: Option<usize> = None;
    for line in disasm.lines() {
        if let Some(rest) = line.strip_prefix("; === Proto ") {
            if let Some(n) = rest.split_whitespace().next().and_then(|s| s.parse::<usize>().ok()) {
                cur = Some(n);
                out.entry(n).or_default();
            }
            continue;
        }
        let t = line.trim_start();
        let Some(colon) = t.find(':') else { continue };
        if colon == 0 || !t[..colon].chars().all(|c| c.is_ascii_digit()) { continue; }
        let after = t[colon + 1..].trim_start();
        let Some(after) = after.strip_prefix("[0x") else { continue };
        let Some(close) = after.find(']') else { continue };
        let mut fields = after[close + 1..].trim_start();
        if let Some(l) = fields.strip_prefix("[L") {
            if let Some(rb) = l.find(']') { fields = l[rb + 1..].trim_start(); }
        }
        let name = fields.split_whitespace().next().unwrap_or("");
        if !is_opcode_name(name) { continue; }
        let rest = fields[name.len()..].to_string();
        if let Some(p) = cur { out.entry(p).or_default().push((name.to_string(), rest)); }
    }
    out
}

#[test]
fn loadk_jumpifnot_confusion_across_the_corpus() {
    let Some(dir) = corpus_dir() else {
        println!("AUDIT_CORPUS unset — skipping");
        return;
    };
    let files = bin_files(&dir);
    assert!(!files.is_empty());
    let pool = build_pool(&files);

    // Distribution of which shuffled byte each file assigns to LOADK / JUMPIFNOT.
    let mut loadk_byte: BTreeMap<u8, usize> = BTreeMap::new();
    let mut jumpifnot_byte: BTreeMap<u8, usize> = BTreeMap::new();
    // Files whose JUMPIFNOT byte is NOT the corpus-majority 0x0E.
    let mut jumpifnot_deviants: Vec<(String, u8)> = Vec::new();
    // Files exhibiting the load-masquerading-as-branch signature.
    let mut masq_files: Vec<(String, usize)> = Vec::new();
    let mut masq_proto_count = 0usize;
    let mut files_with_map = 0usize;

    for f in &files {
        let Ok(data) = std::fs::read(f) else { continue };
        let name = f.file_stem().and_then(|s| s.to_str()).unwrap_or("?").to_string();
        let prior = resolve_prior(&pool, &data);
        let Ok(disasm) = luau_core::disassemble_with_opmap(&data, prior.as_ref()) else { continue };

        let hdr = parse_map_header(&disasm);
        if !hdr.is_empty() {
            files_with_map += 1;
            if let Some(&b) = hdr.get("LOADK") { *loadk_byte.entry(b).or_default() += 1; }
            if let Some(&b) = hdr.get("JUMPIFNOT") {
                *jumpifnot_byte.entry(b).or_default() += 1;
                if b != 0x0E { jumpifnot_deviants.push((name.clone(), b)); }
            }
        }

        let body = body_by_proto(&disasm);
        let mut file_masq = 0usize;
        for (_p, lines) in &body {
            let m = masquerade_run_insns(lines);
            if m > 0 { masq_proto_count += 1; file_masq += m; }
        }
        if file_masq > 0 { masq_files.push((name, file_masq)); }
    }

    masq_files.sort_by(|a, b| b.1.cmp(&a.1));
    let top = |m: &BTreeMap<u8, usize>| -> Vec<(u8, usize)> {
        let mut v: Vec<(u8, usize)> = m.iter().map(|(&b, &n)| (b, n)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        v
    };

    println!("\n============ LOADK / JUMPIFNOT MAP DISTRIBUTION ============");
    println!("files with a shuffle map: {}", files_with_map);
    println!("shuffled byte assigned to LOADK (byte: #files):");
    for (b, n) in top(&loadk_byte).iter().take(8) { println!("   0x{:02X}: {}", b, n); }
    println!("shuffled byte assigned to JUMPIFNOT (byte: #files):");
    for (b, n) in top(&jumpifnot_byte).iter().take(8) { println!("   0x{:02X}: {}", b, n); }
    println!("JUMPIFNOT deviants (not the 0x0E majority):");
    for (nm, b) in &jumpifnot_deviants { println!("   {:<52} 0x{:02X}", nm, b); }
    println!("-----------------------------------------------------------");
    println!("LOAD-AS-BRANCH signature (run>=5 same cond-jump, incrementing reg):");
    println!("   protos flagged: {}", masq_proto_count);
    println!("   files  flagged: {}", masq_files.len());
    for (nm, n) in masq_files.iter().take(25) {
        println!("     {:<56} {} insns", nm, n);
    }
    println!("===========================================================\n");
}

#[test]
fn constants_versus_constant_loads_across_the_corpus() {
    let Some(dir) = corpus_dir() else {
        println!("AUDIT_CORPUS unset — skipping");
        return;
    };
    let files = bin_files(&dir);
    assert!(!files.is_empty(), "no .bin files in {}", dir.display());

    let pool = build_pool(&files);

    // Aggregate counters.
    let mut files_total = 0usize;
    let mut protos_total = 0usize;

    // Clean single-consumer invariant violations (proto-level).
    let mut v_import_protos = 0usize;
    let mut v_closure_protos = 0usize;
    let mut v_table_protos = 0usize;
    let mut files_with_any_violation: Vec<String> = Vec::new();

    // The doc's framing: constants present but zero LOADK/LOADN/LOADB/GETIMPORT.
    let mut v_docframe_protos = 0usize;
    // Broadest: has value-constants but zero constant-consuming opcode of any kind.
    let mut v_broad_protos = 0usize;

    // For a sense of scale, list the worst offenders.
    let mut offenders: Vec<(String, usize, usize, usize)> = Vec::new(); // file, proto, constants, getimport

    for f in &files {
        let Ok(data) = std::fs::read(f) else { continue };
        let Ok(chunk) = luau_core::parser::parse(&data) else { continue };
        let name = f.file_stem().and_then(|s| s.to_str()).unwrap_or("?").to_string();
        files_total += 1;

        let census = census_by_proto(&chunk);
        let prior = resolve_prior(&pool, &data);
        let Ok(disasm) = luau_core::disassemble_with_opmap(&data, prior.as_ref()) else { continue };
        let tally = tally_by_proto(&disasm);

        let mut file_flagged = false;
        for (i, c) in census.iter().enumerate() {
            protos_total += 1;
            let t = tally.get(&i).cloned().unwrap_or_default();

            if c.import > 0 && t.getimport == 0 {
                v_import_protos += 1;
                file_flagged = true;
                if c.import >= 3 {
                    offenders.push((name.clone(), i, c.total, t.getimport));
                }
            }
            if c.closure > 0 && t.dupclosure == 0 {
                v_closure_protos += 1;
                file_flagged = true;
            }
            if c.table > 0 && t.duptable == 0 {
                v_table_protos += 1;
                file_flagged = true;
            }

            // Doc framing: any constants, zero of the four load opcodes.
            let four = t.loadk + t.loadn + t.loadb + t.getimport;
            if c.total >= 8 && four == 0 {
                v_docframe_protos += 1;
            }
            // Broad: has a loadable value constant, zero constant-consuming ops.
            let has_value_const = c.import + c.string + c.number + c.table + c.closure > 0;
            if has_value_const && c.total >= 8 && t.const_consuming == 0 {
                v_broad_protos += 1;
            }
        }
        if file_flagged {
            files_with_any_violation.push(name.clone());
        }
    }

    offenders.sort_by(|a, b| b.2.cmp(&a.2));

    println!("\n================ OPMAP STRUCTURAL AUDIT ================");
    println!("corpus            {}", dir.display());
    println!("files parsed      {}", files_total);
    println!("protos parsed     {}", protos_total);
    println!("-------------------------------------------------------");
    println!("PROVEN MIS-MAP (single-consumer invariant, map-free premise):");
    println!("  Import const present  &  0 GETIMPORT decoded : {} protos", v_import_protos);
    println!("  Closure const present &  0 DUPCLOSURE decoded: {} protos", v_closure_protos);
    println!("  Table  const present  &  0 DUPTABLE decoded  : {} protos", v_table_protos);
    println!("  FILES with >=1 such violation                : {}", files_with_any_violation.len());
    println!("-------------------------------------------------------");
    println!("DOC FRAMING (>=8 constants, 0 LOADK/LOADN/LOADB/GETIMPORT): {} protos", v_docframe_protos);
    println!("BROAD (>=8 consts w/ loadable value, 0 constant-consuming op): {} protos", v_broad_protos);
    println!("-------------------------------------------------------");
    println!("worst Import-invariant offenders (constants, getimport):");
    for (nm, p, k, gi) in offenders.iter().take(15) {
        println!("  {:<52} proto {:<3} {:>4} consts  {} GETIMPORT", nm, p, k, gi);
    }
    println!("FILES flagged (first 40):");
    for nm in files_with_any_violation.iter().take(40) {
        println!("  {}", nm);
    }
    println!("=======================================================\n");
}

/// Full per-proto detail on the reproducer named in the doc.
#[test]
fn reproducer_sunbear_census_versus_decode() {
    let Some(dir) = corpus_dir() else {
        println!("AUDIT_CORPUS unset — skipping");
        return;
    };
    let files = bin_files(&dir);
    let pool = build_pool(&files);

    let target = files.iter().find(|f| {
        f.file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.contains("SunBear2Init"))
    });
    let Some(f) = target else {
        println!("reproducer SunBear2Init not present in corpus — skipping");
        return;
    };

    let data = std::fs::read(f).unwrap();
    let chunk = luau_core::parser::parse(&data).unwrap();
    let census = census_by_proto(&chunk);
    let prior = resolve_prior(&pool, &data);
    let disasm = luau_core::disassemble_with_opmap(&data, prior.as_ref()).unwrap();
    let tally = tally_by_proto(&disasm);

    println!("\n============ REPRODUCER: {} ============", f.file_stem().unwrap().to_str().unwrap());
    println!("protos: {}", chunk.protos.len());
    for (i, c) in census.iter().enumerate() {
        let t = tally.get(&i).cloned().unwrap_or_default();
        println!("--- proto {} ---", i);
        println!("  CONSTANTS (map-free): total={} import={} closure={} table={} string={} number={}",
            c.total, c.import, c.closure, c.table, c.string, c.number);
        println!("  DECODED   (prod map): insns={} GETIMPORT={} DUPCLOSURE={} DUPTABLE={} LOADK={} LOADN={} LOADB={} LOADKX={} const-consuming={}",
            t.total_insns, t.getimport, t.dupclosure, t.duptable, t.loadk, t.loadn, t.loadb, t.loadkx, t.const_consuming);
        let mut verdict = Vec::new();
        if c.import > 0 && t.getimport == 0 { verdict.push("IMPORT-INVARIANT VIOLATED"); }
        if c.closure > 0 && t.dupclosure == 0 { verdict.push("CLOSURE-INVARIANT VIOLATED"); }
        if c.table > 0 && t.duptable == 0 { verdict.push("TABLE-INVARIANT VIOLATED"); }
        if verdict.is_empty() {
            println!("  verdict: consistent");
        } else {
            println!("  verdict: {}", verdict.join(", "));
        }
    }

    // Full opcode histogram for the file, so the decode can be inspected whole.
    let mut hist: BTreeMap<String, usize> = BTreeMap::new();
    for line in disasm.lines() {
        let t = line.trim_start();
        let Some(colon) = t.find(':') else { continue };
        if colon == 0 || !t[..colon].chars().all(|c| c.is_ascii_digit()) { continue; }
        if let Some(rest) = t[colon + 1..].trim_start().strip_prefix("[0x") {
            if let Some(close) = rest.find(']') {
                let mut fields = rest[close + 1..].trim_start();
                if let Some(l) = fields.strip_prefix("[L") {
                    if let Some(rb) = l.find(']') { fields = l[rb + 1..].trim_start(); }
                }
                let nm = fields.split_whitespace().next().unwrap_or("");
                if is_opcode_name(nm) { *hist.entry(nm.to_string()).or_default() += 1; }
            }
        }
    }
    let mut hv: Vec<(String, usize)> = hist.into_iter().collect();
    hv.sort_by(|a, b| b.1.cmp(&a.1));
    println!("--- whole-file opcode histogram (production map) ---");
    for (nm, n) in hv.iter().take(25) {
        println!("  {:>5}  {}", n, nm);
    }
    println!("=======================================================\n");
}
