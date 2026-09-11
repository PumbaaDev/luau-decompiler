//! `opmap-db` — inspect and populate the measured-permutation database.
//!
//! Everything here except `import` is read-only. That is the point: a decompile
//! can consult this database but can never write to it, so no amount of bad
//! decoding can degrade it.

use anyhow::{Context, Result};
use luau_core::parser::opmap_db::{describe_entry, entry_from_probe_report, DbLookup, OpmapDb};
use std::fs;
use std::path::Path;

/// Load a database, printing any per-entry warnings to stderr.
pub fn load(path: &Path) -> Result<OpmapDb> {
    let (db, warnings) = OpmapDb::load_or_empty(path)
        .with_context(|| format!("loading opmap database {}", path.display()))?;
    for w in &warnings {
        eprintln!("opmap-db: {}", w);
    }
    Ok(db)
}

pub fn run_list(path: &Path) -> Result<()> {
    let db = load(path)?;
    if db.is_empty() {
        println!("{} holds no entries", path.display());
        return Ok(());
    }
    println!(
        "{:<24} {:>4} {:>8}  {:<14} {}",
        "id", "bcv", "pinned", "method", "unary semantics"
    );
    println!("{}", "-".repeat(78));
    for e in &db.entries {
        println!(
            "{:<24} {:>4} {:>8}  {:<14} not={} minus={} length={}",
            e.id,
            e.bytecode_version,
            e.pinned(),
            e.provenance.method,
            e.semantics.not.as_str(),
            e.semantics.minus.as_str(),
            e.semantics.length.as_str(),
        );
    }
    Ok(())
}

pub fn run_show(path: &Path, id: &str, json: bool) -> Result<()> {
    let db = load(path)?;
    let entry = db
        .get(id)
        .ok_or_else(|| anyhow::anyhow!("no entry \"{}\" in {}", id, path.display()))?;
    if json {
        let one = OpmapDb {
            entries: vec![entry.clone()],
        };
        print!("{}", one.to_json());
    } else {
        print!("{}", describe_entry(entry));
    }
    Ok(())
}

pub fn run_import(
    path: &Path,
    report: &Path,
    id: Option<&str>,
    build: Option<&str>,
    force: bool,
) -> Result<()> {
    let text = fs::read_to_string(report)
        .with_context(|| format!("reading {}", report.display()))?;
    let entry = entry_from_probe_report(&text, id, build)?;
    let pinned = entry.pinned();
    let entry_id = entry.id.clone();
    let mut db = load(path)?;
    db.insert(entry, force)?;
    db.save(path)?;
    println!(
        "imported \"{}\" ({} opcodes pinned) into {}",
        entry_id,
        pinned,
        path.display()
    );
    Ok(())
}

/// Explain, stage by stage, why a chunk did or did not match.
///
/// This exists because "my file did not get an exact map" is otherwise
/// unanswerable, and a silent fallback to inference looks identical to a
/// database that was never consulted.
pub fn run_match(path: &Path, bytecode: &Path) -> Result<()> {
    let db = load(path)?;
    let data = fs::read(bytecode).with_context(|| format!("reading {}", bytecode.display()))?;
    let chunk = luau_core::parser::parse(&data)?;

    println!("database   {} ({} entries)", path.display(), db.entries.len());
    println!("chunk      {} (bytecode v{}, {} protos)", bytecode.display(), chunk.version, chunk.protos.len());

    if let Some(fp) = luau_core::parser::fingerprint::ChunkFingerprint::from_chunk(&chunk) {
        println!(
            "reading    {} distinct opcode bytes executed, {} anchors read",
            fp.present_bytes(),
            fp.observed_anchors()
        );
        for e in &db.entries {
            let (agree, conflict) = fp.anchor_agreements(&e.map);
            let (corr_agree, corr_conflict) = fp.corroboration(&e.map);
            let walk = luau_core::parser::opmap::OpcodeMap::walk_verify(&chunk, &e.map);
            println!(
                "  {:<20} anchors {}/{}  corroboration {}/{}  {}",
                e.id,
                agree,
                conflict,
                corr_agree,
                corr_conflict,
                walk.verdict.describe()
            );
        }
    } else {
        println!("reading    chunk carries no opcode shuffle");
    }

    let result = db.lookup(&chunk);
    println!();
    match result {
        DbLookup::Hit { .. } => println!("MATCH      {}", result.describe()),
        _ => println!("NO MATCH   {}", result.describe()),
    }
    Ok(())
}
