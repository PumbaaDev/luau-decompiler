//! SETTABLEKS must be DETECTED in every chunk that actually contains it.
//!
//! ── WHAT THIS PROTECTS ──────────────────────────────────────────────────
//! GETTABLEKS, SETTABLEKS and NAMECALL share one instruction shape: A and B
//! are valid registers, C is unused, and the AUX word is a string-constant
//! index. Nothing in a single instruction word distinguishes them. The
//! detectors therefore separate them by context, and `detect_namecall`
//! (crates/luau-core/src/parser/opmap.rs) runs BEFORE `detect_table_ops` and
//! claims whichever byte scores highest on its survivor count.
//!
//! Once a byte is claimed, `detect_table_ops` skips it forever —
//!     if ctx.is_mapped(op) { continue; }
//! — and every later pass carries the same filter. There is NO recovery
//! path. So when NAMECALL wins the byte that is really SETTABLEKS, the chunk
//! is decompiled with no SETTABLEKS mapping at all: every `t.k = v` in that
//! file is decoded as something else, or invented by bijection completion in
//! Tier 9 with no evidence behind it.
//!
//! This test asserts the one thing that must be true regardless of which
//! detector wins the race:
//!
//!   IF the shuffled byte that the corpus agrees is SETTABLEKS occurs at a
//!   true instruction position in a chunk, THEN SETTABLEKS must appear
//!   somewhere in that chunk's evidence-backed map.
//!
//! ── WHY IT CANNOT BE MADE GREEN BY EDITING THE TEST ─────────────────────
//! Three properties, deliberately:
//!
//!   1. The subject byte is NOT hardcoded. It is elected by majority vote
//!      across the corpus, from the detector's own output on the files where
//!      detection succeeded. Nobody can move the goalposts by picking a
//!      friendlier constant, and if the shuffle changes with the client build
//!      the vote follows it. The elected byte is printed, and the vote must
//!      have a clear winner or the test fails on that instead.
//!
//!   2. The denominator is the PRESENCE MASK, not the file list. A file that
//!      never contains the byte is an absence, not a miss, and is excluded —
//!      the same distinction `OpcodeMap::present_byte_mask` exists to make.
//!      Presence is measured by the AUX-skipping instruction walk, so an AUX
//!      data word whose low byte happens to equal the subject byte cannot be
//!      mistaken for an occurrence of it.
//!
//!   3. The assertion is on the PRE-COMPLETION map — real evidence only.
//!      Tier 9 bijection completion will happily fill SETTABLEKS in from the
//!      leftovers; that fill is a guess with nothing behind it, and counting
//!      it would make this test green while the defect is untouched. That is
//!      exactly how earlier checks in this project were silently blunted.
//!
//! Detection is run via `detect_structural`: prior-free and ground-truth-
//! free, so the reading is a property of the bytecode and not of whatever a
//! previous test in the same process happened to install globally.
//!
//! ── WHY THE NUMBER IS WHAT IT IS ────────────────────────────────────────
//! The failure count is NOT a tuning knob and is not asserted against. The
//! test demands zero. The count printed is simply how many chunks in this
//! corpus contain the byte and lose the opcode; at the time this test was
//! written that number was measured, on the 628-file single-build corpus, and
//! is reported in the run output together with a tally of WHICH opcode holds
//! the byte instead. If that tally says NAMECALL, the race described above is
//! what you are looking at.
//!
//! It goes green only when the detectors stop dropping SETTABLEKS on files
//! that contain it. Nothing else moves it.
//!
//! Run with:
//!   BC_CORPUS=<dir> cargo test -p luau-core --release \
//!     --test settableks_detection_gate -- --nocapture

use luau_core::parser::opcodes::LuauOpcode;
use luau_core::parser::opmap::OpcodeMap;
use std::path::PathBuf;

/// The opcode under test.
const SUBJECT: u8 = LuauOpcode::SetTableKS as u8;

fn corpus_dir() -> PathBuf {
    PathBuf::from(std::env::var("BC_CORPUS").unwrap_or_else(|_| {
        r"C:\Users\jep\AppData\Local\Potassium\workspace\bc_extract_1786138100".to_string()
    }))
}

/// One chunk's independent reading of the shuffle.
struct Reading {
    name: String,
    /// Evidence-backed map only (detector findings, before Tier 9 completion).
    pre: [u8; 256],
    /// Shuffled bytes occurring at a TRUE instruction position (AUX skipped).
    present: [bool; 256],
}

impl Reading {
    /// Which shuffled byte did this file's detectors call SETTABLEKS, if any?
    fn subject_byte(&self) -> Option<u8> {
        self.pre.iter().position(|&v| v == SUBJECT).map(|i| i as u8)
    }
}

fn opname(v: u8) -> String {
    if v == 255 {
        "UNMAPPED".to_string()
    } else {
        LuauOpcode::from_u8(v).name().to_string()
    }
}

/// Read every shuffled chunk in the corpus and take a prior-free reading.
fn read_corpus(dir: &std::path::Path) -> Vec<Reading> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("bin")))
        .collect();
    paths.sort();

    for path in paths {
        let Ok(bytes) = std::fs::read(&path) else { continue };
        let Ok(chunk) = luau_core::parser::parse(&bytes) else { continue };
        // Canonical (unshuffled) bytecode has no shuffle to detect; it is not
        // evidence about this defect either way.
        if !OpcodeMap::needs_remapping(&chunk) {
            continue;
        }
        let detected = OpcodeMap::detect_structural(&chunk);
        out.push(Reading {
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
            pre: detected.pre_completion_map,
            present: detected.present_byte_mask(&chunk),
        });
    }
    out
}

#[test]
fn settableks_is_detected_wherever_its_byte_occurs() {
    let dir = corpus_dir();
    if !dir.exists() {
        eprintln!("SKIP: corpus not present at {}", dir.display());
        return;
    }

    let readings = read_corpus(&dir);
    if readings.is_empty() {
        eprintln!(
            "SKIP: no shuffled bytecode chunks found under {}",
            dir.display()
        );
        return;
    }

    // ── ELECT THE SUBJECT BYTE ──────────────────────────────────────────
    // Majority vote over the files whose detectors DID find SETTABLEKS.
    // Files that dropped it cast no vote; they are the population under test,
    // so letting them vote would be circular.
    let mut votes = [0usize; 256];
    for r in &readings {
        if let Some(b) = r.subject_byte() {
            votes[b as usize] += 1;
        }
    }
    let total_votes: usize = votes.iter().sum();
    let mut tally: Vec<(u8, usize)> = (0..256u16)
        .map(|b| (b as u8, votes[b as usize]))
        .filter(|&(_, n)| n > 0)
        .collect();
    tally.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    eprintln!();
    eprintln!("================================================================");
    eprintln!("  SETTABLEKS DETECTION GATE");
    eprintln!("================================================================");
    eprintln!("  corpus            {}", dir.display());
    eprintln!("  shuffled chunks   {}", readings.len());
    eprintln!("  chunks that named a byte SETTABLEKS: {}", total_votes);
    for (b, n) in tally.iter().take(5) {
        eprintln!(
            "    0x{:02X}  {:>4} vote(s)   {:>5.1}%",
            b,
            n,
            100.0 * *n as f64 / total_votes.max(1) as f64
        );
    }

    assert!(
        total_votes > 0,
        "no chunk in the corpus detected SETTABLEKS at all, so there is no \
         consensus byte to test against. That is a harder failure than the \
         one this test was written for, not a pass."
    );

    let (subject_byte, winner_votes) = tally[0];
    let runner_up = tally.get(1).map(|&(_, n)| n).unwrap_or(0);
    assert!(
        winner_votes * 2 > total_votes && winner_votes >= runner_up * 2,
        "the corpus does not agree on which byte is SETTABLEKS \
         (winner 0x{:02X} with {} of {} votes, runner-up {}). A fractured \
         vote means the elected byte is not trustworthy as a subject, so this \
         test refuses to report a number rather than report a meaningless one.",
        subject_byte,
        winner_votes,
        total_votes,
        runner_up
    );

    // ── MEASURE ─────────────────────────────────────────────────────────
    let mut contains_byte = 0usize;
    let mut failures: Vec<&Reading> = Vec::new();
    let mut mapped_elsewhere = 0usize;
    let mut missing_regardless_of_presence = 0usize;

    for r in &readings {
        if r.subject_byte().is_none() {
            missing_regardless_of_presence += 1;
        }
        if !r.present[subject_byte as usize] {
            continue;
        }
        contains_byte += 1;
        match r.subject_byte() {
            None => failures.push(r),
            Some(b) if b != subject_byte => mapped_elsewhere += 1,
            Some(_) => {}
        }
    }

    // Who holds the byte instead, on the files that lost the opcode?
    let mut thief = std::collections::BTreeMap::<String, usize>::new();
    // …and was the byte that really holds NAMECALL left unmapped there?
    let mut namecall_present_but_unmapped = 0usize;
    for r in &failures {
        *thief.entry(opname(r.pre[subject_byte as usize])).or_insert(0) += 1;
        let namecall_byte = r
            .pre
            .iter()
            .position(|&v| v == LuauOpcode::NameCall as u8)
            .map(|i| i as u8);
        if namecall_byte == Some(subject_byte) {
            namecall_present_but_unmapped += 1;
        }
    }

    eprintln!();
    eprintln!("  ELECTED SUBJECT BYTE           0x{:02X}", subject_byte);
    eprintln!(
        "  chunks where 0x{:02X} OCCURS      {}   <- the denominator",
        subject_byte, contains_byte
    );
    eprintln!(
        "  ...of those, SETTABLEKS LOST   {}   <- the failure count",
        failures.len()
    );
    eprintln!(
        "  ...of those, mapped to another byte  {}",
        mapped_elsewhere
    );
    eprintln!(
        "  chunks missing SETTABLEKS entirely (incl. ones that never contain the byte)  {}",
        missing_regardless_of_presence
    );
    if !thief.is_empty() {
        eprintln!();
        eprintln!("  WHAT HOLDS 0x{:02X} ON THE FAILING CHUNKS:", subject_byte);
        let mut rows: Vec<(&String, &usize)> = thief.iter().collect();
        rows.sort_by(|a, b| b.1.cmp(a.1));
        for (name, n) in rows {
            eprintln!("    {:<12} {:>4}", name, n);
        }
        eprintln!(
            "    (of the failures, {} have NAMECALL sitting on 0x{:02X})",
            namecall_present_but_unmapped, subject_byte
        );
    }
    if !failures.is_empty() {
        eprintln!();
        eprintln!("  FIRST 10 FAILING CHUNKS:");
        for r in failures.iter().take(10) {
            eprintln!("    {}  (0x{:02X} -> {})", r.name, subject_byte, opname(r.pre[subject_byte as usize]));
        }
    }
    eprintln!("================================================================");

    assert!(
        contains_byte > 0,
        "byte 0x{:02X} was elected SETTABLEKS by {} chunk(s) but the presence \
         walk says it never occurs at a true instruction position anywhere. \
         The election and the presence mask disagree, which means one of them \
         is broken — this is not a pass.",
        subject_byte,
        winner_votes
    );

    assert!(
        failures.is_empty(),
        "SETTABLEKS was dropped from the evidence-backed map in {} of {} \
         chunk(s) that actually execute byte 0x{:02X}.\n\
         Those files are decompiled with no SETTABLEKS mapping: every field \
         store in them is decoded as some other opcode, or invented outright \
         by Tier 9 bijection completion.\n\
         This is a detector-ordering defect, not a measurement artifact — see \
         this file's doc comment. Fix the detector; do not relax the test.",
        failures.len(),
        contains_byte,
        subject_byte
    );
}
