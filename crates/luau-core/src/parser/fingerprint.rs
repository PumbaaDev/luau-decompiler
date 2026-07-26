//! Identify which opcode permutation a chunk belongs to, from the chunk alone.
//!
//! # There is nothing in a chunk to hash
//!
//! The obvious design is "hash the chunk, look the hash up". It cannot work. A
//! `Chunk` carries a bytecode version, a types version, a string table and some
//! prototypes — nothing that names the client that produced it. Anything
//! derived from the bytes identifies the *script*, not the *build*.
//!
//! Nor can the fingerprint be the set of opcode bytes the chunk uses. That is
//! the natural next idea and it fails on the requirement that matters most:
//! stability across scripts. A twelve-line script and a ten-thousand-line
//! script from the same client observe different subsets of the same
//! permutation, so any equality test over "bytes observed" puts them in
//! different buckets. The relationship between two scripts of one build is
//! subset, not equality.
//!
//! # So identification is falsification, not lookup
//!
//! What a fingerprint can do is *propose* candidates and then try hard to
//! disprove each one against the chunk. That is cheap and it is decisive:
//!
//! * **Header** — bytecode and types version must match exactly. A free reject.
//! * **Anchors** — a handful of opcodes that structural detection finds almost
//!   perfectly. Any disagreement rejects the candidate outright.
//! * **Corroboration** — score the chunk's own structural reading against the
//!   candidate's full permutation. This is what actually separates two similar
//!   builds, because it looks at every byte the chunk uses, not just anchors.
//!
//! # Why the bar is higher here than in `consensus`
//!
//! `consensus::same_shuffle` admits a peer on three anchor agreements and
//! tolerates disagreements, which is right for a tally: a wrong admission costs
//! one vote among dozens. Here a wrong match installs an entire foreign
//! permutation as LOCKED ground truth, and the output that follows is clean,
//! plausible and completely wrong — with no unresolved instructions to signal
//! it. A miss just falls back to today's inference.
//!
//! That asymmetry sets every threshold in this module. Anchor conflicts are not
//! tolerated at all, a minimum score is required, and when two candidates are
//! close the answer is "I don't know", never the better-scoring one.

use super::consensus::ANCHOR_OPCODES;
use super::opmap::OpcodeMap;
use super::types::Chunk;

/// Anchors expected to be BOTH ~100% reliably detected AND to move between
/// builds. They are used as a zero-tolerance gate, not as the discriminator.
///
/// The distinction matters: across the shuffle variants this crate already
/// knows about, `RETURN` and `PREPVARARGS` sit on the *same* byte in every one.
/// A design that leaned on anchors varying would quietly fail on a family of
/// related builds. Here they only ever reject, so constancy costs nothing.
pub const TIER_A_ANCHORS: [u8; 2] = [
    22, // Return
    65, // PrepVarargs
];

/// A structured, partial identification of a chunk's permutation.
#[derive(Debug, Clone)]
pub struct ChunkFingerprint {
    pub version: u8,
    pub types_version: u8,
    /// Parallel to [`ANCHOR_OPCODES`]: the shuffled byte each anchor was found
    /// at, or 255 if this chunk gave no reading for it.
    pub anchor_bytes: [u8; ANCHOR_OPCODES.len()],
    /// The chunk's own structural reading, uninfluenced by anything installed.
    pub solo_map: [u8; 256],
    /// Which bytes occur at a true instruction position.
    pub solo_present: [bool; 256],
}

impl ChunkFingerprint {
    /// Read a fingerprint off a chunk.
    ///
    /// Returns `None` for bytecode that carries no Roblox shuffle: canonical
    /// Luau has no permutation to identify, and keying it would be meaningless.
    pub fn from_chunk(chunk: &Chunk) -> Option<Self> {
        if !OpcodeMap::needs_remapping(chunk) {
            return None;
        }
        // Structural only. An installed ground truth must never influence the
        // reading that decides which ground truth to install.
        let solo = OpcodeMap::detect_structural(chunk);
        let solo_map = solo.heuristic_map;
        let solo_present = solo.present_byte_mask(chunk);

        let mut anchor_bytes = [255u8; ANCHOR_OPCODES.len()];
        for (i, &op) in ANCHOR_OPCODES.iter().enumerate() {
            if let Some(b) = anchor_byte(&solo_map, op) {
                anchor_bytes[i] = b;
            }
        }

        Some(ChunkFingerprint {
            version: chunk.version,
            types_version: chunk.types_version,
            anchor_bytes,
            solo_map,
            solo_present,
        })
    }

    /// Parse and fingerprint in one step.
    pub fn from_bytecode(bytes: &[u8]) -> Option<Self> {
        let chunk = super::parse(bytes).ok()?;
        Self::from_chunk(&chunk)
    }

    /// How many anchors this chunk gave a reading for.
    pub fn observed_anchors(&self) -> usize {
        self.anchor_bytes.iter().filter(|&&b| b != 255).count()
    }

    /// Distinct bytes this chunk actually executes.
    pub fn present_bytes(&self) -> usize {
        self.solo_present.iter().filter(|&&p| p).count()
    }

    /// The byte this chunk read a given anchor at, if any.
    pub fn anchor(&self, opcode: u8) -> Option<u8> {
        let i = ANCHOR_OPCODES.iter().position(|&o| o == opcode)?;
        match self.anchor_bytes[i] {
            255 => None,
            b => Some(b),
        }
    }

    /// A coarse narrowing key over the header and the two most reliable
    /// anchors. Purely an index: it narrows the candidate list and never makes
    /// a decision. `None` when either Tier-A anchor was not observed, in which
    /// case the caller must fall back to scanning.
    pub fn bucket_key(&self) -> Option<u64> {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut mix = |b: u8| {
            h ^= b as u64;
            h = h.wrapping_mul(0x1000_0000_01b3);
        };
        mix(self.version);
        mix(self.types_version);
        for &op in TIER_A_ANCHORS.iter() {
            mix(self.anchor(op)?);
        }
        Some(h)
    }

    /// Do this chunk's Tier-A readings contradict a candidate permutation?
    ///
    /// Only where BOTH have an opinion. A chunk that never showed us `RETURN`
    /// is not evidence against anything.
    pub fn tier_a_conflicts(&self, candidate: &[u8; 256]) -> u32 {
        let mut conflicts = 0;
        for &op in TIER_A_ANCHORS.iter() {
            if let (Some(mine), Some(theirs)) = (self.anchor(op), anchor_byte(candidate, op)) {
                if mine != theirs {
                    conflicts += 1;
                }
            }
        }
        conflicts
    }

    /// How many anchors this chunk and a candidate agree on.
    pub fn anchor_agreements(&self, candidate: &[u8; 256]) -> (u32, u32) {
        let mut agree = 0;
        let mut conflict = 0;
        for (i, &op) in ANCHOR_OPCODES.iter().enumerate() {
            let mine = self.anchor_bytes[i];
            if mine == 255 {
                continue;
            }
            match anchor_byte(candidate, op) {
                Some(theirs) if theirs == mine => agree += 1,
                Some(_) => conflict += 1,
                None => {}
            }
        }
        (agree, conflict)
    }

    /// Score this chunk's structural reading against a candidate's full
    /// permutation, over the bytes the chunk actually executes.
    ///
    /// Conflicts are EXPECTED even against the correct candidate: solo
    /// structural detection is only around 60% accurate, so the true build will
    /// still disagree with a good number of the chunk's own guesses. That is
    /// why this returns both halves and the caller weighs them, rather than
    /// capping conflicts outright — an absolute cap would reject the right
    /// answer.
    pub fn corroboration(&self, candidate: &[u8; 256]) -> (u32, u32) {
        corroboration_score(&self.solo_map, &self.solo_present, candidate)
    }
}

/// Which shuffled byte does `map` send to `opcode`?
pub fn anchor_byte(map: &[u8; 256], opcode: u8) -> Option<u8> {
    map.iter().position(|&v| v == opcode).map(|i| i as u8)
}

/// `(agreements, conflicts)` between a solo reading and a candidate map, over
/// the bytes the chunk actually executes and both sides have an opinion about.
pub fn corroboration_score(
    solo_map: &[u8; 256],
    solo_present: &[bool; 256],
    candidate: &[u8; 256],
) -> (u32, u32) {
    let mut agree = 0;
    let mut conflict = 0;
    for b in 0..256usize {
        if !solo_present[b] {
            continue;
        }
        let mine = solo_map[b];
        let theirs = candidate[b];
        if mine == 255 || theirs == 255 {
            continue;
        }
        if mine == theirs {
            agree += 1;
        } else {
            conflict += 1;
        }
    }
    (agree, conflict)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::types::{Constant, Proto};

    fn insn(op: u8, operands: u32) -> u32 {
        (op as u32) | (operands << 8)
    }

    fn proto(code: Vec<u32>) -> Proto {
        Proto {
            max_stack_size: 8,
            num_params: 0,
            num_upvalues: 0,
            is_vararg: true,
            flags: 0,
            typeinfo: None,
            code,
            constants: vec![Constant::Nil],
            child_protos: Vec::new(),
            line_defined: 0,
            debug_name: None,
            line_info: None,
            debug_info: None,
        }
    }

    fn chunk(protos: Vec<Proto>) -> Chunk {
        Chunk {
            version: 6,
            types_version: 3,
            strings: Vec::new(),
            protos,
            main_proto: 0,
        }
    }

    /// Internal opcode numbers used below.
    const RETURN: u8 = 22;
    const PREPVARARGS: u8 = 65;
    const MOVE: u8 = 6;
    const LOADN: u8 = 4;

    /// Two different permutations of the internal opcode space.
    fn perm_a(op: u8) -> u8 {
        op.wrapping_mul(3).wrapping_add(151)
    }
    fn perm_b(op: u8) -> u8 {
        op.wrapping_mul(5).wrapping_add(37)
    }

    fn full_map(perm: fn(u8) -> u8) -> [u8; 256] {
        let mut m = [255u8; 256];
        for op in 0..84u8 {
            m[perm(op) as usize] = op;
        }
        m
    }

    /// A chunk that a real detector can read: PREPVARARGS first, then some
    /// body, then RETURN. Shuffled by `perm`.
    fn shuffled_chunk(perm: fn(u8) -> u8, extra: usize) -> Chunk {
        let mut code = vec![insn(perm(PREPVARARGS), 0)];
        for i in 0..extra {
            code.push(insn(perm(LOADN), ((i as u32) << 8) | 1));
            code.push(insn(perm(MOVE), ((i as u32 + 1) << 8) | (i as u32)));
        }
        code.push(insn(perm(RETURN), 1));
        chunk(vec![proto(code)])
    }

    #[test]
    fn canonical_bytecode_has_no_permutation_to_identify() {
        // Low opcode bytes throughout: needs_remapping declines.
        let c = chunk(vec![proto(vec![
            insn(PREPVARARGS, 0),
            insn(MOVE, 0x0102),
            insn(RETURN, 1),
        ])]);
        assert!(ChunkFingerprint::from_chunk(&c).is_none());
    }

    #[test]
    fn fingerprint_reads_the_header_verbatim() {
        let c = shuffled_chunk(perm_a, 6);
        let fp = ChunkFingerprint::from_chunk(&c).expect("shuffled");
        assert_eq!(fp.version, 6);
        assert_eq!(fp.types_version, 3);
    }

    /// The stability property the whole design turns on: two DIFFERENT scripts
    /// from ONE build must not contradict each other, and must key the same.
    #[test]
    fn two_scripts_of_one_build_agree_and_bucket_together() {
        let small = shuffled_chunk(perm_a, 3);
        let large = shuffled_chunk(perm_a, 60);
        let fs = ChunkFingerprint::from_chunk(&small).expect("small");
        let fl = ChunkFingerprint::from_chunk(&large).expect("large");

        assert_eq!(fs.version, fl.version);
        for &op in TIER_A_ANCHORS.iter() {
            if let (Some(a), Some(b)) = (fs.anchor(op), fl.anchor(op)) {
                assert_eq!(a, b, "anchor {} moved between two scripts of one build", op);
            }
        }
        assert_eq!(
            fs.bucket_key(),
            fl.bucket_key(),
            "same build must produce the same bucket key regardless of script size"
        );

        // And the larger script observes a superset of the smaller one's bytes:
        // subset, not equality, which is exactly why a hash cannot be the key.
        let small_bytes: Vec<usize> = (0..256).filter(|&b| fs.solo_present[b]).collect();
        for b in small_bytes {
            assert!(fl.solo_present[b], "large script lost byte {:#04X}", b);
        }
    }

    #[test]
    fn tier_a_gate_rejects_a_foreign_permutation() {
        let c = shuffled_chunk(perm_a, 20);
        let fp = ChunkFingerprint::from_chunk(&c).expect("shuffled");
        assert_eq!(
            fp.tier_a_conflicts(&full_map(perm_a)),
            0,
            "must not contradict its own build"
        );
        assert!(
            fp.tier_a_conflicts(&full_map(perm_b)) > 0,
            "must contradict a different build"
        );
    }

    #[test]
    fn corroboration_prefers_the_true_build_by_a_wide_margin() {
        let c = shuffled_chunk(perm_a, 40);
        let fp = ChunkFingerprint::from_chunk(&c).expect("shuffled");

        let (right_agree, right_conflict) = fp.corroboration(&full_map(perm_a));
        let (wrong_agree, wrong_conflict) = fp.corroboration(&full_map(perm_b));

        let right = right_agree as i32 - 3 * right_conflict as i32;
        let wrong = wrong_agree as i32 - 3 * wrong_conflict as i32;
        assert!(
            right > wrong,
            "true build scored {} vs foreign {}",
            right,
            wrong
        );
        assert!(right_agree > 0);
    }

    #[test]
    fn anchor_byte_finds_and_misses_correctly() {
        let m = full_map(perm_a);
        assert_eq!(anchor_byte(&m, RETURN), Some(perm_a(RETURN)));
        let empty = [255u8; 256];
        assert_eq!(anchor_byte(&empty, RETURN), None);
    }

    #[test]
    fn corroboration_ignores_bytes_the_chunk_never_executes() {
        let mut solo = [255u8; 256];
        let mut present = [false; 256];
        let mut cand = [255u8; 256];
        // Agreement on a byte the chunk uses.
        solo[0x10] = MOVE;
        cand[0x10] = MOVE;
        present[0x10] = true;
        // Disagreement on a byte the chunk NEVER uses: must not count.
        solo[0x20] = MOVE;
        cand[0x20] = RETURN;
        let (agree, conflict) = corroboration_score(&solo, &present, &cand);
        assert_eq!((agree, conflict), (1, 0));
    }

    #[test]
    fn a_chunk_with_no_anchor_readings_has_no_bucket_key() {
        let fp = ChunkFingerprint {
            version: 6,
            types_version: 3,
            anchor_bytes: [255; ANCHOR_OPCODES.len()],
            solo_map: [255; 256],
            solo_present: [false; 256],
        };
        assert_eq!(fp.bucket_key(), None);
        assert_eq!(fp.observed_anchors(), 0);
    }

    #[test]
    fn fingerprint_ignores_installed_ground_truth() {
        let _lock = crate::parser::test_fixtures::ground_truth_lock();
        // Install a map for a DIFFERENT permutation, then fingerprint. If the
        // reading were influenced by it, the anchors would move.
        let c = shuffled_chunk(perm_a, 20);
        let before = ChunkFingerprint::from_chunk(&c).expect("shuffled");

        crate::set_ground_truth_opmap(Some(full_map(perm_b)));
        let during = ChunkFingerprint::from_chunk(&c).expect("shuffled");
        crate::set_ground_truth_opmap(None);

        assert_eq!(
            before.anchor_bytes, during.anchor_bytes,
            "identification must not be influenced by what is already installed"
        );
        assert_eq!(before.solo_map, during.solo_map);
    }
}
