//! `SETTABLEKS Rn.key Rn` and the `{X = "X"}` tables it folds into.
//!
//! ── THE ORIGINAL DEFECT (fixed; this file is now its regression guard) ───
//! `ReplicatedStorage.Stickers.StickerPlacer`, proto 11:
//!
//! ```text
//! 0: LOADK       R2 K0 "GetCanvasTouchPoint"   -- R2 = a STRING
//! 1: SETTABLEKS  R2."GetCanvasTouchPoint" R2   -- R2[key] = R2
//! 3: MOVE        R3 R1
//! 4: CALL        R2 args=1 results=4           -- calls R2
//! ```
//!
//! `CALL args=1` is a plain call `f(arg)`, so R2 must hold a FUNCTION, and
//! instructions 0-1 do not produce one. The reference decompiler settles what
//! the source was:
//!
//! ```text
//! local v39, v40, v41, v42 = u1.GetCanvasTouchPoint(p38);
//! ```
//!
//! -- a field READ from an upvalue. Instruction 1 was really GETTABLEKS, and
//! the byte was mis-assigned. The lifter was blameless: given
//! `SETTABLEKS Rn.k Rn` it faithfully emitted `{GetCanvasTouchPoint = "..."}`.
//!
//! ── WHY THIS FILE WAS REWRITTEN, 22 Aug 2026 ────────────────────────────
//! This test used to assert a CORPUS-WIDE INVARIANT: "a real SETTABLEKS never
//! has A == B; a candidate byte that produces them is not SETTABLEKS." That
//! invariant is FALSE, and it had been red ever since.
//!
//! It is refuted by ground truth, not by argument. Decoding
//! `ReplicatedStorage.Beequips.BeequipTypes` under `opmap_db/roblox_v9_live`
//! - a permutation read off the client's OWN compiler by `probe align`, so
//! SETTABLEKS's byte there is measured, not inferred - gives:
//!
//! ```text
//! ;   0x30 -> 16 SETTABLEKS
//!      3: [0x10] SETTABLEKS  R0."__index" R0      <- A == B, and CORRECT
//! ```
//!
//! `tbl.__index = tbl` is the ordinary Lua metatable idiom and compiles to
//! exactly A == B. Measured on the 1,324-file union corpus: 69 of the 70
//! `x.field = x` lines in the output are `__index`.
//!
//! The constructor form is no safer. `X = "X"` is an ordinary table entry, and
//! a table whose entries are ALL of that shape is an ordinary ENUM - the three
//! files matching it corpus-wide are a Roblox CoreScript event-name table
//! (`CharacterAdded = "CharacterAdded"`, ...) and two shop enums. Nothing in
//! either shape distinguishes the defect from correct code.
//!
//! Had anyone "fixed" the detector to satisfy the old assertion, they would
//! have taught it that `tbl.__index = tbl` is impossible and broken the decode
//! of every class table in the corpus. A test that can only be satisfied by
//! breaking the decompiler is worse than no test.
//!
//! So this file now asserts the two things that ARE true: the concrete defect
//! stays fixed, and the legitimate shape stays legal. The second is a mutation
//! guard - ban A == B and it goes red immediately.

use std::path::{Path, PathBuf};

/// Corpus directory. `bc/` ships with the repo, so the test does not depend on
/// a path inside a tool's private workspace.
fn corpus_dir() -> PathBuf {
    std::env::var("BC_CORPUS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bc"))
}

/// Decompile the way the SHIPPING path does.
///
/// The CLI silently applies the bundled measured permutation
/// (`opmap_db/roblox_v9_live.json`) via its `bundled_db_path()`; bare
/// `luau_core::decompile` does not, and on that weaker detector-only path
/// StickerPlacer still emits the artifact. Asserting against the bare API
/// would therefore be measuring a path no shipped consumer of this tool takes.
///
/// KNOWN GAP, recorded here because this helper is where it becomes visible:
/// the bundled-DB default lives in the CLI, so `luau-wasm` and `luau-server`,
/// which call `luau_core::decompile`, get the weaker path. That is a real
/// difference in output quality between the binary and the library, and it is
/// not this test's job to paper over it.
fn decompile_shipping(bytes: &[u8]) -> Option<String> {
    let db_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../opmap_db/roblox_v9_live.json");
    if let Ok((db, _warnings)) = luau_core::parser::opmap_db::OpmapDb::load(&db_path) {
        if let Some(entry) = db.get("roblox_v9_live") {
            if let Ok((src, _)) = luau_core::decompile_with_plan(
                bytes,
                &luau_core::DecodePlan { prior: None, exact: Some(entry) },
            ) {
                return Some(src);
            }
        }
    }
    luau_core::decompile(bytes).ok()
}

fn decompile_named(stem: &str) -> Option<String> {
    let dir = corpus_dir();
    let path = dir.join(format!("{stem}.bin"));
    let bytes = std::fs::read(path).ok()?;
    decompile_shipping(&bytes)
}

/// THE REGRESSION. `GetCanvasTouchPoint` must be READ from a table and then
/// CALLED - never stored into itself, and never folded into a `{X = "X"}`
/// entry. Both of those are what the bad decode produced.
#[test]
fn stickerplacer_reads_the_field_it_calls() {
    let Some(src) = decompile_named("16_ReplicatedStorage_Stickers_StickerPlacer") else {
        eprintln!("SKIP: 16_ReplicatedStorage_Stickers_StickerPlacer.bin not in corpus");
        return;
    };

    // The artifact shape, stated exactly: the key stored as its own name.
    let artifact = src
        .lines()
        .map(str::trim)
        .any(|l| l.starts_with("GetCanvasTouchPoint = \"GetCanvasTouchPoint\""));
    assert!(
        !artifact,
        "StickerPlacer emitted `GetCanvasTouchPoint = \"GetCanvasTouchPoint\"` - the \
         SETTABLEKS byte is holding GETTABLEKS again. Instruction 1 of proto 11 is a \
         field READ; the source is `u1.GetCanvasTouchPoint(p38)`."
    );

    // And the positive half: the field is read off a table, then called. A test
    // that only checks the artifact is absent would also pass on an empty file.
    let reads_field = src.contains(".GetCanvasTouchPoint");
    assert!(
        reads_field,
        "StickerPlacer no longer reads `.GetCanvasTouchPoint` off a table at all - the \
         call site is gone, which is a worse failure than the artifact this guards."
    );
}

/// THE MUTATION GUARD, and the reason the old corpus-wide assertion was wrong.
///
/// `tbl.__index = tbl` is `SETTABLEKS Rn."__index" Rn` - A == B - and it is
/// correct Lua. If a future change teaches the opmap detectors that A == B
/// disqualifies a byte from being SETTABLEKS, class tables stop decoding and
/// this test reds. It is deliberately an assertion that the shape EXISTS.
#[test]
fn a_table_may_store_itself_in_its_own_field() {
    let Some(src) = decompile_named("16_ReplicatedStorage_Stickers_StickerPlacer") else {
        eprintln!("SKIP: corpus not present");
        return;
    };
    // StickerPlacer itself need not contain one; scan the corpus for the idiom.
    let dir = corpus_dir();
    if !dir.exists() {
        eprintln!("SKIP: corpus dir {} not present", dir.display());
        return;
    }
    let _ = src;

    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "bin"))
        .collect();
    files.sort();
    if files.is_empty() {
        eprintln!("SKIP: no .bin files in {}", dir.display());
        return;
    }

    let mut found = 0usize;
    for path in &files {
        let Ok(bytes) = std::fs::read(path) else { continue };
        let Some(src) = decompile_shipping(&bytes) else { continue };
        for raw in src.lines() {
            let t = raw.trim().trim_end_matches(',');
            if let Some((lhs, rhs)) = t.split_once(" = ") {
                if let Some((base, field)) = lhs.trim().rsplit_once('.') {
                    if field == "__index" && base.trim() == rhs.trim() {
                        found += 1;
                    }
                }
            }
        }
    }

    assert!(
        found > 0,
        "no `x.__index = x` survived the decode of {} chunks. That idiom is \
         `SETTABLEKS Rn.\"__index\" Rn` - A == B - and it is CORRECT. Losing it \
         means something now treats a self-store as impossible, which is the \
         false invariant this file was rewritten to refute.",
        files.len()
    );
}
