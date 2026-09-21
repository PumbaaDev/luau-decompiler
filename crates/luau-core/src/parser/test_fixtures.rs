//! Real bytecode for tests that need a chunk the structural detectors can
//! actually read.
//!
//! Synthetic instruction streams are fine for testing a walk or a conflict
//! rule, but they are useless for testing anything that depends on *detection
//! quality* — a hand-written stream has none of the structure the detectors
//! look for, so they read almost nothing off it and every threshold appears to
//! fail. The fixtures here are two probe-set programs compiled by upstream
//! `luau-compile`, so a test exercising the database lookup sees the same kind
//! of evidence a real file would.
//!
//! The permutation helper is the inverse of `alignment::align_pair`: it walks
//! the canonical stream and relabels opcode bytes, leaving AUX words and every
//! operand bit untouched — exactly the transformation a client's opcode shuffle
//! applies.

use super::alignment::{canonical_to_internal, UNMAPPED};
use super::opcodes::LuauOpcode;
use super::types::{insn_op, Chunk};

/// A mid-size program: closures, loops, calls, fastcalls, globals, varargs.
pub const M04_MIRROR_FLOW: &[u8] = include_bytes!("../../probe/fixtures/m04_mirror_flow.luac");
/// A branch-heavy program: every comparison jump and every constant-compare.
pub const M02_MIRROR_BRANCH: &[u8] = include_bytes!("../../probe/fixtures/m02_mirror_branch.luac");
/// Three one-line functions: `not x`, `-x`, `#x`. The whole point of this one
/// is to exercise the NOT / MINUS / LENGTH passthrough.
pub const P03_UNARY: &[u8] = include_bytes!("../../probe/fixtures/p03_unary.luac");

/// Serialise a chunk back to bytecode by patching the opcode bytes of an
/// existing blob in place.
///
/// Only the code words change under `permute`, and every code word sits at a
/// fixed offset in the file, so re-emitting means overwriting the same u32
/// slots. Avoids writing a full bytecode WRITER just to build a test input.
pub fn permuted_bytes(original: &[u8], perm: fn(u8) -> u8) -> Vec<u8> {
    let chunk = canonical(original);
    let mut out = original.to_vec();
    // Locate each proto's code array by re-scanning for its exact word run.
    // The code arrays are contiguous and unique enough in practice; to stay
    // exact we track a cursor so identical runs cannot be matched twice.
    let mut cursor = 0usize;
    for proto in &chunk.protos {
        let needle: Vec<u8> = proto
            .code
            .iter()
            .flat_map(|w| w.to_le_bytes())
            .collect();
        let at = find_from(&out, &needle, cursor).expect("code array is present in the blob");
        let permuted = permute(&single(&chunk, proto.code.clone()), perm);
        let bytes: Vec<u8> = permuted.protos[0]
            .code
            .iter()
            .flat_map(|w| w.to_le_bytes())
            .collect();
        out[at..at + bytes.len()].copy_from_slice(&bytes);
        cursor = at + bytes.len();
    }
    out
}

fn single(template: &Chunk, code: Vec<u32>) -> Chunk {
    let mut c = template.clone();
    let mut p = c.protos[0].clone();
    p.code = code;
    c.protos = vec![p];
    c.main_proto = 0;
    c
}

fn find_from(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    (from..=haystack.len() - needle.len()).find(|&i| &haystack[i..i + needle.len()] == needle)
}

/// Parse a fixture into a canonical chunk.
pub fn canonical(bytes: &[u8]) -> Chunk {
    super::parse(bytes).expect("fixture is valid v6 bytecode")
}

/// Relabel every opcode byte through `perm`, touching nothing else.
///
/// Panics if the input is not canonical Luau, because a fixture that is not
/// canonical would silently produce a meaningless test.
pub fn permute(chunk: &Chunk, perm: fn(u8) -> u8) -> Chunk {
    let mut out = chunk.clone();
    for proto in out.protos.iter_mut() {
        let code = &mut proto.code;
        let mut i = 0usize;
        while i < code.len() {
            let canonical_op = insn_op(code[i]);
            let internal =
                canonical_to_internal(canonical_op).expect("fixture must be canonical Luau");
            code[i] = (code[i] & 0xFFFF_FF00) | perm(canonical_op) as u32;
            i += if LuauOpcode::from_u8(internal).has_aux() {
                2
            } else {
                1
            };
        }
    }
    out
}

/// The exact map a probe would derive for `perm`: shuffled byte -> internal
/// opcode, over every canonical opcode.
pub fn exact_map(perm: fn(u8) -> u8) -> [u8; 256] {
    let mut m = [UNMAPPED; 256];
    for canonical_op in 0..super::alignment::CANONICAL_OPCODE_COUNT as u8 {
        if let Some(internal) = canonical_to_internal(canonical_op) {
            m[perm(canonical_op) as usize] = internal;
        }
    }
    m
}

/// Serialise the tests that install a process-global ground-truth map.
///
/// `GROUND_TRUTH` is process-wide, and the test harness runs tests in parallel,
/// so two tests installing different maps would race and read each other's
/// state. That is not a flaw in the tests: it is the same limitation
/// `GroundTruthGuard` documents for real callers, surfacing exactly where you
/// would expect it to.
pub fn ground_truth_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// Three unrelated permutations, standing in for three client builds.
pub fn perm_a(op: u8) -> u8 {
    op.wrapping_mul(3).wrapping_add(151)
}
pub fn perm_b(op: u8) -> u8 {
    op.wrapping_mul(5).wrapping_add(37)
}
pub fn perm_c(op: u8) -> u8 {
    op.wrapping_mul(11).wrapping_add(83)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::opmap::OpcodeMap;

    #[test]
    fn fixtures_are_canonical_luau() {
        for bytes in [M04_MIRROR_FLOW, M02_MIRROR_BRANCH] {
            let c = canonical(bytes);
            assert!(OpcodeMap::is_canonical_luau(&c));
            assert!(!OpcodeMap::needs_remapping(&c));
            assert!(c.protos.len() > 3);
        }
    }

    #[test]
    fn permuting_produces_bytecode_that_takes_the_shuffle_path() {
        for perm in [perm_a as fn(u8) -> u8, perm_b, perm_c] {
            let c = permute(&canonical(M04_MIRROR_FLOW), perm);
            assert!(
                OpcodeMap::needs_remapping(&c),
                "a permuted fixture must reach the Roblox path"
            );
        }
    }

    #[test]
    fn the_exact_map_decodes_the_permuted_fixture_cleanly() {
        let original = canonical(M04_MIRROR_FLOW);
        let shuffled = permute(&original, perm_a);
        let report = OpcodeMap::walk_verify(&shuffled, &exact_map(perm_a));
        assert!(
            report.verdict.is_clean(),
            "{}",
            report.verdict.describe()
        );
        assert!(report.present_bytes() > 20);
    }

    #[test]
    fn a_foreign_map_does_not_decode_the_permuted_fixture() {
        let shuffled = permute(&canonical(M04_MIRROR_FLOW), perm_a);
        let report = OpcodeMap::walk_verify(&shuffled, &exact_map(perm_b));
        assert!(
            !report.verdict.is_clean(),
            "a different build's map must not walk this chunk cleanly"
        );
    }

    #[test]
    fn permutation_preserves_every_operand_bit() {
        let original = canonical(M02_MIRROR_BRANCH);
        let shuffled = permute(&original, perm_a);
        assert_eq!(original.protos.len(), shuffled.protos.len());
        for (o, s) in original.protos.iter().zip(shuffled.protos.iter()) {
            assert_eq!(o.code.len(), s.code.len());
            for (a, b) in o.code.iter().zip(s.code.iter()) {
                assert_eq!(a >> 8, b >> 8, "only the opcode byte may change");
            }
        }
    }

    #[test]
    fn alignment_recovers_the_permutation_from_a_real_fixture() {
        let original = canonical(M04_MIRROR_FLOW);
        let shuffled = permute(&original, perm_a);
        let a = crate::parser::alignment::align_pair(&original, &shuffled).expect("aligns");
        assert!(a.protos_rejected.is_empty());
        assert!(a.conflicts.is_empty());
        assert!(a.pinned() >= 30, "pinned only {}", a.pinned());
        for b in 0..256usize {
            if a.map[b] != UNMAPPED {
                assert_eq!(a.map[b], exact_map(perm_a)[b], "byte {:#04X}", b);
            }
        }
    }
}

#[cfg(test)]
mod reemit_tests {
    use super::*;

    #[test]
    fn permuted_bytes_round_trips_through_the_parser() {
        for bytes in [P03_UNARY, M04_MIRROR_FLOW, M02_MIRROR_BRANCH] {
            let re = permuted_bytes(bytes, perm_a);
            assert_eq!(re.len(), bytes.len(), "patching must not resize the blob");
            let parsed = super::super::parse(&re).expect("patched blob still parses");
            let expected = permute(&canonical(bytes), perm_a);
            assert_eq!(parsed.protos.len(), expected.protos.len());
            for (a, b) in parsed.protos.iter().zip(expected.protos.iter()) {
                assert_eq!(a.code, b.code, "re-emitted code must match the permutation");
            }
        }
    }

    #[test]
    fn the_unary_fixture_really_contains_all_three_operators() {
        let c = canonical(P03_UNARY);
        let seen = crate::parser::probe::observed_canonical_opcodes(&c).expect("canonical");
        for name in ["NOT", "MINUS", "LENGTH"] {
            let found = (0..crate::parser::alignment::CANONICAL_OPCODE_COUNT as u8).any(|op| {
                seen[op as usize] && crate::parser::alignment::canonical_opcode_name(op) == Some(name)
            });
            assert!(found, "unary fixture is missing {}", name);
        }
    }
}
