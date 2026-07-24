//! Round-trip bytecode validation.
//!
//! Compares an *original* bytecode blob against a *recompiled* bytecode blob
//! (the decompiler's output, re-fed through the Luau compiler via the executor's
//! `lift_closure`) by walking instructions in lock-step.
//!
//! Constant-pool ordering differs run-to-run — when the Luau compiler sees a
//! freshly written source file, it may interleave the literals in a different
//! order than the one baked into the shipping bytecode. To avoid counting
//! these reorderings as real divergences we normalize K-referenced operands
//! through a per-proto map from the recompiled index to the original index.
//!
//! The comparison is intentionally conservative: an exact opcode mismatch is
//! always a divergence, but an AddK that reads a different constant *index*
//! while the underlying constant value matches is normalized to "same".

use crate::parser::opcodes::LuauOpcode;
use crate::parser::types::{insn_a, insn_b, insn_c, insn_d, insn_e, insn_op, Chunk, Constant, Proto};

/// Summary of a roundtrip comparison for one script.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct RoundtripReport {
    pub total_insns: usize,
    pub matching_insns: usize,
    pub match_pct: f64,
    /// pc (within the main proto) of the first differing instruction, if any
    pub first_divergence_pc: Option<usize>,
    /// Short textual description of that divergence
    pub divergence_detail: Option<String>,
    /// Number of child proto pairs that were compared
    pub compared_protos: usize,
    /// Number of protos whose counts differed and which therefore could not be
    /// walked in lock-step beyond the shorter body
    pub size_mismatched_protos: usize,
}

/// Compare two parsed chunks and return a RoundtripReport.
///
/// Walks protos in index order. For each proto pair, constant-pool reordering
/// is normalized by mapping recompiled K indices back to original K indices
/// *before* field-level comparison.
pub fn compare_chunks(original: &Chunk, recompiled: &Chunk) -> RoundtripReport {
    let mut total: usize = 0;
    let mut matching: usize = 0;
    let mut first_div_pc: Option<usize> = None;
    let mut first_div_detail: Option<String> = None;
    let mut size_mismatched: usize = 0;

    let orig_main = original.main_proto as usize;
    let rec_main = recompiled.main_proto as usize;
    if orig_main >= original.protos.len() || rec_main >= recompiled.protos.len() {
        return RoundtripReport {
            total_insns: 0,
            matching_insns: 0,
            match_pct: 0.0,
            first_divergence_pc: Some(0),
            divergence_detail: Some("invalid main_proto index".to_string()),
            compared_protos: 0,
            size_mismatched_protos: 0,
        };
    }

    // Compare protos in index order — simplest stable pairing. When the
    // compiler emits the same number of protos (typical for a faithful
    // decompile), this lines them up cleanly.
    let n_protos = original.protos.len().min(recompiled.protos.len());
    let compared_protos = n_protos;
    if original.protos.len() != recompiled.protos.len() {
        size_mismatched += 1;
    }

    for pi in 0..n_protos {
        let o = &original.protos[pi];
        let r = &recompiled.protos[pi];
        let kmap = build_const_remap(o, r);
        let n_insns = o.code.len().min(r.code.len());
        if o.code.len() != r.code.len() {
            size_mismatched += 1;
        }

        let mut pc = 0usize;
        while pc < n_insns {
            total += 1;
            let o_insn = o.code[pc];
            let r_insn = r.code[pc];

            let (matched, detail) = compare_insn(o, r, o_insn, r_insn, &kmap);
            if matched {
                matching += 1;
            } else if first_div_pc.is_none() && pi == orig_main {
                first_div_pc = Some(pc);
                first_div_detail = Some(detail);
            }

            // Advance past AUX word. If both sides agree on has_aux, count
            // the AUX word as its own instruction (comparing raw u32 equality).
            // If only one side has AUX, record a structural mismatch.
            let o_op = LuauOpcode::from_u8(insn_op(o_insn));
            let r_op = LuauOpcode::from_u8(insn_op(r_insn));
            let o_has_aux = o_op.has_aux() && pc + 1 < o.code.len();
            let r_has_aux = r_op.has_aux() && pc + 1 < r.code.len();

            if o_has_aux && r_has_aux {
                pc += 1;
                if pc < n_insns {
                    total += 1;
                    if o.code[pc] == r.code[pc] {
                        matching += 1;
                    } else if first_div_pc.is_none() && pi == orig_main {
                        first_div_pc = Some(pc);
                        first_div_detail = Some(format!(
                            "AUX word differs at pc={} (0x{:08X} vs 0x{:08X})",
                            pc, o.code[pc], r.code[pc]
                        ));
                    }
                    pc += 1;
                }
            } else if o_has_aux != r_has_aux {
                if first_div_pc.is_none() && pi == orig_main {
                    first_div_pc = Some(pc);
                    first_div_detail = Some(format!(
                        "AUX mismatch at pc={}: orig has_aux={} recompiled has_aux={}",
                        pc, o_has_aux, r_has_aux
                    ));
                }
                pc += 1;
            } else {
                pc += 1;
            }
        }
    }

    let match_pct = if total == 0 {
        0.0
    } else {
        (matching as f64) * 100.0 / (total as f64)
    };

    RoundtripReport {
        total_insns: total,
        matching_insns: matching,
        match_pct,
        first_divergence_pc: first_div_pc,
        divergence_detail: first_div_detail,
        compared_protos,
        size_mismatched_protos: size_mismatched,
    }
}

/// Compare two instruction words. Returns (matched, detail-if-not-matched).
fn compare_insn(
    orig_proto: &Proto,
    rec_proto: &Proto,
    o_insn: u32,
    r_insn: u32,
    kmap: &[Option<usize>],
) -> (bool, String) {
    let o_op = LuauOpcode::from_u8(insn_op(o_insn));
    let r_op = LuauOpcode::from_u8(insn_op(r_insn));

    if (o_op as u8) != (r_op as u8) {
        return (
            false,
            format!("opcode {} vs {}", o_op.name(), r_op.name()),
        );
    }

    let (oa, ob, oc, od, oe) = (
        insn_a(o_insn),
        insn_b(o_insn),
        insn_c(o_insn),
        insn_d(o_insn),
        insn_e(o_insn),
    );
    let (ra, rb, rc, rd, re) = (
        insn_a(r_insn),
        insn_b(r_insn),
        insn_c(r_insn),
        insn_d(r_insn),
        insn_e(r_insn),
    );

    let mismatch = |detail: String| (false, detail);

    match o_op {
        // ABC arithmetic / bitops with K operand in C
        LuauOpcode::AddK
        | LuauOpcode::SubK
        | LuauOpcode::MulK
        | LuauOpcode::DivK
        | LuauOpcode::ModK
        | LuauOpcode::PowK
        | LuauOpcode::IDivK
        | LuauOpcode::AndK
        | LuauOpcode::OrK
        | LuauOpcode::Bandk
        | LuauOpcode::Bork => {
            if oa != ra || ob != rb {
                return mismatch(format!("{} A/B differ", o_op.name()));
            }
            if !kidx_eq(oc as u32, rc as u32, kmap, orig_proto, rec_proto) {
                return mismatch(format!(
                    "{} constant differs (K{} vs K{})",
                    o_op.name(),
                    oc,
                    rc
                ));
            }
            (true, String::new())
        }

        // ABC arithmetic with K operand in B (SubRK, DivRK: K on left)
        LuauOpcode::SubRK | LuauOpcode::DivRK => {
            if oa != ra || oc != rc {
                return mismatch(format!("{} A/C differ", o_op.name()));
            }
            if !kidx_eq(ob as u32, rb as u32, kmap, orig_proto, rec_proto) {
                return mismatch(format!("{} constant differs", o_op.name()));
            }
            (true, String::new())
        }

        // AD instructions whose D is a K index
        LuauOpcode::LoadK | LuauOpcode::DupClosure | LuauOpcode::DupTable => {
            if oa != ra {
                return mismatch(format!("{} A differs", o_op.name()));
            }
            if !kidx_eq(od as u16 as u32, rd as u16 as u32, kmap, orig_proto, rec_proto) {
                return mismatch(format!(
                    "{} constant differs (K{} vs K{})",
                    o_op.name(),
                    od,
                    rd
                ));
            }
            (true, String::new())
        }

        // GetImport: D is K index into an Import constant
        LuauOpcode::GetImport => {
            if oa != ra {
                return mismatch("GETIMPORT A differs".to_string());
            }
            if !kidx_eq(od as u16 as u32, rd as u16 as u32, kmap, orig_proto, rec_proto) {
                return mismatch("GETIMPORT constant differs".to_string());
            }
            (true, String::new())
        }

        // E-format jump
        LuauOpcode::JumpX => {
            if oe != re {
                return mismatch(format!("JUMPX offset differs ({} vs {})", oe, re));
            }
            (true, String::new())
        }

        // AD-format jumps / for-loops
        LuauOpcode::Jump
        | LuauOpcode::JumpBack
        | LuauOpcode::JumpIf
        | LuauOpcode::JumpIfNot
        | LuauOpcode::JumpIfEq
        | LuauOpcode::JumpIfLE
        | LuauOpcode::JumpIfLT
        | LuauOpcode::JumpIfNotEq
        | LuauOpcode::JumpIfNotLE
        | LuauOpcode::JumpIfNotLT
        | LuauOpcode::ForNPrep
        | LuauOpcode::ForNLoop
        | LuauOpcode::ForGPrep
        | LuauOpcode::ForGLoop
        | LuauOpcode::ForGPrepINext
        | LuauOpcode::ForGPrepNext
        | LuauOpcode::NewClosure
        | LuauOpcode::JumpXEqKNil
        | LuauOpcode::JumpXEqKB
        | LuauOpcode::JumpXEqKN
        | LuauOpcode::JumpXEqKS => {
            if oa != ra || od != rd {
                return mismatch(format!(
                    "{} A/D differ ({},{} vs {},{})",
                    o_op.name(),
                    oa,
                    od,
                    ra,
                    rd
                ));
            }
            (true, String::new())
        }

        // All other opcodes: byte-exact A/B/C comparison
        _ => {
            if oa != ra || ob != rb || oc != rc {
                return mismatch(format!(
                    "{} A/B/C differ ({},{},{} vs {},{},{})",
                    o_op.name(),
                    oa,
                    ob,
                    oc,
                    ra,
                    rb,
                    rc
                ));
            }
            (true, String::new())
        }
    }
}

/// Build a remap from recompiled-pool index to original-pool index.
fn build_const_remap(orig: &Proto, rec: &Proto) -> Vec<Option<usize>> {
    let mut map = vec![None; rec.constants.len()];
    for (ri, rc_val) in rec.constants.iter().enumerate() {
        for (oi, oc_val) in orig.constants.iter().enumerate() {
            if const_eq(rc_val, oc_val) {
                map[ri] = Some(oi);
                break;
            }
        }
    }
    map
}

/// Structural equality for two constants.
fn const_eq(a: &Constant, b: &Constant) -> bool {
    match (a, b) {
        (Constant::Nil, Constant::Nil) => true,
        (Constant::Boolean(x), Constant::Boolean(y)) => x == y,
        (Constant::Number(x), Constant::Number(y)) => x.to_bits() == y.to_bits(),
        (Constant::String(x), Constant::String(y)) => x == y,
        (Constant::Import(x), Constant::Import(y)) => x == y,
        (Constant::Closure(x), Constant::Closure(y)) => x == y,
        (Constant::Vector(x1, y1, z1, w1), Constant::Vector(x2, y2, z2, w2)) => {
            x1.to_bits() == x2.to_bits()
                && y1.to_bits() == y2.to_bits()
                && z1.to_bits() == z2.to_bits()
                && w1.to_bits() == w2.to_bits()
        }
        (Constant::Table(x), Constant::Table(y)) => x == y,
        _ => false,
    }
}

/// Compare two K indices modulo the remap.
fn kidx_eq(
    orig_idx: u32,
    rec_idx: u32,
    kmap: &[Option<usize>],
    orig: &Proto,
    rec: &Proto,
) -> bool {
    let oi = orig_idx as usize;
    let ri = rec_idx as usize;
    if let Some(mapped) = kmap.get(ri).and_then(|o| *o) {
        if mapped == oi {
            return true;
        }
    }
    match (orig.constants.get(oi), rec.constants.get(ri)) {
        (Some(a), Some(b)) => const_eq(a, b),
        _ => false,
    }
}

// ───────────────────────────── Tests ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::types::*;

    fn mk_proto(code: Vec<u32>, constants: Vec<Constant>) -> Proto {
        Proto {
            max_stack_size: 2,
            num_params: 0,
            num_upvalues: 0,
            is_vararg: false,
            flags: 0,
            typeinfo: None,
            code,
            constants,
            child_protos: vec![],
            line_defined: 0,
            debug_name: None,
            line_info: None,
            debug_info: None,
        }
    }

    fn mk_chunk(proto: Proto) -> Chunk {
        Chunk {
            version: 6,
            types_version: 0,
            strings: vec![],
            protos: vec![proto],
            main_proto: 0,
        }
    }

    fn enc_abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
        (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
    }

    fn enc_ad(op: u8, a: u8, d: i16) -> u32 {
        (op as u32) | ((a as u32) << 8) | (((d as u16) as u32) << 16)
    }

    /// Identical bytecode must round-trip at 100%.
    #[test]
    fn identical_bytecode_matches_100pct() {
        let code = vec![
            enc_ad(LuauOpcode::LoadN as u8, 0, 1),
            enc_abc(LuauOpcode::Return as u8, 0, 2, 0),
        ];
        let a = mk_chunk(mk_proto(code.clone(), vec![]));
        let b = mk_chunk(mk_proto(code, vec![]));
        let report = compare_chunks(&a, &b);
        assert_eq!(report.total_insns, 2);
        assert_eq!(report.matching_insns, 2);
        assert!((report.match_pct - 100.0).abs() < 1e-9);
        assert!(report.first_divergence_pc.is_none());
    }

    /// Swapping one opcode for another must produce < 100% match with a
    /// first-divergence entry pointing at that pc.
    #[test]
    fn opcode_substituted_drops_match_pct() {
        let orig = vec![
            enc_ad(LuauOpcode::LoadN as u8, 0, 1),
            enc_abc(LuauOpcode::Return as u8, 0, 2, 0),
        ];
        let rec = vec![
            enc_abc(LuauOpcode::LoadB as u8, 0, 1, 0),
            enc_abc(LuauOpcode::Return as u8, 0, 2, 0),
        ];
        let a = mk_chunk(mk_proto(orig, vec![]));
        let b = mk_chunk(mk_proto(rec, vec![]));
        let report = compare_chunks(&a, &b);
        assert_eq!(report.total_insns, 2);
        assert_eq!(report.matching_insns, 1);
        assert_eq!(report.first_divergence_pc, Some(0));
        assert!(report.divergence_detail.as_ref().unwrap().contains("LOADN"));
    }

    /// Permuting the constant pool must STILL round-trip at 100% after
    /// normalization.
    #[test]
    fn constant_reordered_still_100pct() {
        let orig_pool = vec![
            Constant::String("hello".into()),
            Constant::Number(42.0),
        ];
        let rec_pool = vec![
            Constant::Number(42.0),
            Constant::String("hello".into()),
        ];

        // LOADK R0, K0 (which is "hello" in orig, K1 in recompiled)
        let orig_code = vec![enc_ad(LuauOpcode::LoadK as u8, 0, 0)];
        let rec_code = vec![enc_ad(LuauOpcode::LoadK as u8, 0, 1)];

        let a = mk_chunk(mk_proto(orig_code, orig_pool));
        let b = mk_chunk(mk_proto(rec_code, rec_pool));
        let report = compare_chunks(&a, &b);
        assert_eq!(report.total_insns, 1);
        assert_eq!(
            report.matching_insns, 1,
            "constant-pool reorder must not count as divergence"
        );
        assert!((report.match_pct - 100.0).abs() < 1e-9);
    }

    /// Numeric constant swap across pools with ADDK operand.
    #[test]
    fn addk_pool_reorder_normalized() {
        let orig_pool = vec![Constant::Number(1.0), Constant::Number(7.0)];
        let rec_pool = vec![Constant::Number(7.0), Constant::Number(1.0)];

        // ADDK R0 = R0 + K1 (orig: 7, recompiled: K0)
        let orig_code = vec![enc_abc(LuauOpcode::AddK as u8, 0, 0, 1)];
        let rec_code = vec![enc_abc(LuauOpcode::AddK as u8, 0, 0, 0)];

        let a = mk_chunk(mk_proto(orig_code, orig_pool));
        let b = mk_chunk(mk_proto(rec_code, rec_pool));
        let report = compare_chunks(&a, &b);
        assert_eq!(report.matching_insns, 1);
    }

    /// Register mismatch must count as a divergence even when constant pools line up.
    #[test]
    fn register_mismatch_is_divergence() {
        let pool = vec![Constant::Number(1.0)];
        let orig_code = vec![enc_ad(LuauOpcode::LoadN as u8, 0, 1)];
        let rec_code = vec![enc_ad(LuauOpcode::LoadN as u8, 1, 1)];
        let a = mk_chunk(mk_proto(orig_code, pool.clone()));
        let b = mk_chunk(mk_proto(rec_code, pool));
        let report = compare_chunks(&a, &b);
        assert_eq!(report.matching_insns, 0);
        assert_eq!(report.first_divergence_pc, Some(0));
    }

    /// Proto-count mismatch must be surfaced as size_mismatched_protos but
    /// still compute a partial match over the overlapping range.
    #[test]
    fn size_mismatch_tracked() {
        let code1 = vec![enc_abc(LuauOpcode::Return as u8, 0, 1, 0)];
        let code2 = vec![enc_abc(LuauOpcode::Return as u8, 0, 1, 0)];

        let p_orig = mk_proto(code1, vec![]);
        let p_rec1 = mk_proto(code2.clone(), vec![]);
        let p_rec2 = mk_proto(code2, vec![]);

        let orig_chunk = Chunk {
            version: 6,
            types_version: 0,
            strings: vec![],
            protos: vec![p_orig],
            main_proto: 0,
        };
        let rec_chunk = Chunk {
            version: 6,
            types_version: 0,
            strings: vec![],
            protos: vec![p_rec1, p_rec2],
            main_proto: 0,
        };

        let report = compare_chunks(&orig_chunk, &rec_chunk);
        assert!(report.size_mismatched_protos >= 1);
        assert_eq!(report.compared_protos, 1);
    }

    /// When both sides have an AUX word, the walker must step over it and
    /// continue at the correct pc.
    #[test]
    fn aux_words_walked_in_lockstep() {
        let code = vec![
            enc_ad(LuauOpcode::GetGlobal as u8, 0, 0), // GETGLOBAL R0 K0
            0x0000_0000,                               // AUX: k index 0
            enc_abc(LuauOpcode::Return as u8, 0, 2, 0),
        ];
        let pool = vec![Constant::String("print".into())];
        let a = mk_chunk(mk_proto(code.clone(), pool.clone()));
        let b = mk_chunk(mk_proto(code, pool));
        let report = compare_chunks(&a, &b);
        // 3 words counted (GETGLOBAL + AUX + RETURN), all match.
        assert_eq!(report.total_insns, 3);
        assert_eq!(report.matching_insns, 3);
    }
}
