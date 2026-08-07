//! Is the instruction stream aligned?
//!
//! ── WHY THIS QUESTION COMES FIRST ───────────────────────────────────────
//! Five theories about the remaining damage were formed today by reading
//! decompiled output and inferring backwards, and all five were wrong
//! (inlining, FASTCALL1 AUX, NAMECALL mis-decode, GET/SET swap, LOADK vs
//! GETUPVAL). The two fixes that HELD came from evidence in the bytecode
//! itself — a proto flag, and opcode-byte assignment counts.
//!
//! So: measure a property of the bytecode, not of the output.
//!
//! ── THE PROPERTY ────────────────────────────────────────────────────────
//! Luau instructions are 32-bit words. Some opcodes consume a following AUX
//! word (GETIMPORT, GETTABLEKS, NAMECALL, the JUMPIFcmp family, ...). Walking
//! a proto from index 0, consuming 1 or 2 words per instruction according to
//! `has_aux()`, must land EXACTLY on `code.len()`.
//!
//! If the walk overshoots or stops short, the decoder is reading words at the
//! wrong offsets from that point on. Every opcode after the desync is then
//! meaningless regardless of how correct the opcode MAP is — which would mean
//! chasing individual byte assignments cannot fix those protos.
//!
//! A misaligned walk has exactly two possible causes, and both are actionable:
//!   * an opcode is mapped to something with the wrong AUX-arity
//!   * an opcode genuinely present is absent from `has_aux()`
//!
//! This is deliberately a DIAGNOSTIC, not a pass/fail gate: it reports, so the
//! next piece of work starts from a measurement instead of a guess.
//!
//! Run with:
//!   BC_CORPUS=<dir> cargo test -p luau-core --release \
//!     --test stream_alignment -- --nocapture

use luau_core::parser::opcodes::LuauOpcode;
use std::collections::BTreeMap;
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

#[test]
fn instruction_streams_land_exactly_on_their_length() {
    let files = corpus();
    if files.is_empty() {
        eprintln!("SKIP: bytecode corpus not present");
        return;
    }

    let mut chunks_scanned = 0usize;
    let mut protos_scanned = 0usize;
    let mut protos_misaligned = 0usize;
    let mut chunks_with_misalignment = 0usize;
    // opcode name -> how many times it was the LAST op before an overshoot
    let mut suspect_ops: BTreeMap<String, usize> = BTreeMap::new();
    let mut worst: Vec<(String, usize, usize)> = Vec::new();

    for path in &files {
        let Ok(bytes) = std::fs::read(path) else { continue };
        // Remap exactly as the decompiler does, so we measure the stream the
        // decompiler actually sees rather than raw shuffled bytes. Measuring
        // the raw stream would answer a different (and useless) question.
        let Ok(mut chunk) = luau_core::parser::parse(&bytes) else { continue };
        if luau_core::parser::opmap::OpcodeMap::needs_remapping(&chunk) {
            let map = luau_core::parser::opmap::OpcodeMap::detect(&chunk);
            let _ = map.remap_chunk(&mut chunk);
        }
        chunks_scanned += 1;

        let mut bad_here = 0usize;
        for proto in &chunk.protos {
            protos_scanned += 1;
            let len = proto.code.len();
            let mut i = 0usize;
            let mut last_op: Option<LuauOpcode> = None;
            while i < len {
                let insn = proto.code[i];
                let op_byte = (insn & 0xFF) as u8;
                let op = LuauOpcode::from_u8(op_byte);
                last_op = Some(op);
                i += if op.has_aux() { 2 } else { 1 };
            }
            // Landing past the end means the final instruction claimed an AUX
            // word that does not exist — the classic desync signature.
            if i != len {
                protos_misaligned += 1;
                bad_here += 1;
                if let Some(op) = last_op {
                    *suspect_ops.entry(format!("{:?}", op)).or_insert(0) += 1;
                }
            }
        }
        if bad_here > 0 {
            chunks_with_misalignment += 1;
            worst.push((
                path.file_stem().unwrap_or_default().to_string_lossy().to_string(),
                bad_here,
                chunk.protos.len(),
            ));
        }
    }

    worst.sort_by(|a, b| b.1.cmp(&a.1));

    eprintln!();
    eprintln!("================================================================");
    eprintln!("  INSTRUCTION STREAM ALIGNMENT");
    eprintln!("================================================================");
    eprintln!("  chunks scanned          {:>6}", chunks_scanned);
    eprintln!("  protos scanned          {:>6}", protos_scanned);
    eprintln!(
        "  protos MISALIGNED       {:>6}  ({:.2}%)",
        protos_misaligned,
        100.0 * protos_misaligned as f64 / protos_scanned.max(1) as f64
    );
    eprintln!("  chunks affected         {:>6}", chunks_with_misalignment);
    eprintln!();
    if suspect_ops.is_empty() {
        eprintln!("  Every proto walks cleanly to its exact length.");
        eprintln!("  The stream is ALIGNED — remaining damage is not a desync,");
        eprintln!("  so opcode-assignment work is the right target after all.");
    } else {
        eprintln!("  LAST OPCODE BEFORE AN OVERSHOOT (the AUX-arity suspects)");
        let mut v: Vec<_> = suspect_ops.iter().collect();
        v.sort_by(|a, b| b.1.cmp(a.1));
        for (name, n) in v.iter().take(12) {
            eprintln!("    {:<24} {:>5}", name, n);
        }
        eprintln!();
        eprintln!("  WORST CHUNKS");
        for (name, bad, total) in worst.iter().take(10) {
            let short: String = name.chars().take(56).collect();
            eprintln!("    {:>3}/{:<3} protos   {}", bad, total, short);
        }
    }
    eprintln!("================================================================");
}
