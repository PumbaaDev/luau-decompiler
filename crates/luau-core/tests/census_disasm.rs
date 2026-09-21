//! Exact-map disassembly dump — census tooling, not a gate.
//!
//! `disassemble --opmap` consults only the pooled store, so with no store it
//! infers a partial map and the listing lies. This dump applies the SAME
//! measured database entry the batch decode used (`from_exact_map`, verbatim,
//! no detection, no completion), so the listing shows what the lifter saw.
//! Bytes the entry leaves unpinned stay unmapped and are visible as such,
//! rather than being silently guessed.
//!
//! Run with:
//!   CENSUS_BINS=<dir> CENSUS_OUT=<dir> [CENSUS_DB=<json>] \
//!     cargo test -p luau-core --release --test census_disasm -- --nocapture

use luau_core::parser::opmap::OpcodeMap;
use luau_core::parser::opmap_db::OpmapDb;
use std::path::{Path, PathBuf};

#[test]
fn dump_exact_disasm() {
    let Ok(bins) = std::env::var("CENSUS_BINS") else {
        eprintln!("SKIP: set CENSUS_BINS=<dir of .bin>");
        return;
    };
    let Ok(outdir) = std::env::var("CENSUS_OUT") else {
        eprintln!("SKIP: set CENSUS_OUT=<dir>");
        return;
    };
    let db_path = std::env::var("CENSUS_DB")
        .unwrap_or_else(|_| "opmap_db/roblox_v9_live.json".to_string());
    let (db, warnings) = OpmapDb::load(Path::new(&db_path)).expect("load opmap db");
    for w in &warnings {
        eprintln!("DB WARNING: {}", w);
    }
    std::fs::create_dir_all(&outdir).expect("create out dir");

    let mut files: Vec<PathBuf> = std::fs::read_dir(&bins)
        .expect("read bins dir")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "bin"))
        .collect();
    files.sort();

    let (mut hits, mut misses) = (0usize, 0usize);
    for p in &files {
        let stem = p.file_stem().unwrap_or_default().to_string_lossy().to_string();
        let Ok(data) = std::fs::read(p) else { continue };
        let Ok(mut chunk) = luau_core::parser::parse(&data) else {
            eprintln!("PARSEFAIL\t{}", stem);
            continue;
        };
        let lookup = db.lookup(&chunk);
        let Some((id, map, _sem)) = lookup.hit() else {
            eprintln!("NOHIT\t{}\t{}", stem, lookup.describe());
            misses += 1;
            continue;
        };
        let id = id.to_string();
        let map = *map;
        if OpcodeMap::needs_remapping(&chunk) {
            let exact = OpcodeMap::from_exact_map(map);
            let _ = exact.remap_chunk(&mut chunk);
        }
        let body = luau_core::disasm::disassemble(&chunk, true);
        let text = format!("; census exact-map disasm — db entry \"{}\", applied verbatim\n{}", id, body);
        std::fs::write(Path::new(&outdir).join(format!("{}.dis", stem)), text).expect("write");
        hits += 1;
    }
    eprintln!("CENSUS_DISASM done: {} written, {} no-hit", hits, misses);
}
