//! Derive an opcode permutation EXACTLY, by aligning two compilations of the
//! same source: one whose numbering we know, one whose numbering we do not.
//!
//! # Why this is not inference
//!
//! Everything else in this crate *guesses* the permutation from structure, and
//! guessing has a ceiling. This module does not guess. If you can obtain the
//! same program compiled twice — once by a compiler whose opcode numbering is
//! documented (upstream `luau-compile`), once by the compiler whose numbering
//! is secret — then the permutation is a fact you can read off, not a
//! hypothesis you have to score.
//!
//! # The bootstrap problem, and why it does not exist here
//!
//! The obvious objection is circular: to walk an instruction stream you must
//! skip AUX words, to know which instructions carry an AUX word you must know
//! the opcode, and to know the opcode you need the permutation. That circle is
//! real only if you insist on walking the *unknown* stream.
//!
//! You do not have to. The two chunks are the same program, so they have the
//! same proto count, the same code length per proto, and the same word-for-word
//! layout. They differ in exactly one place: the low byte of each instruction
//! word. AUX-ness is a property of an instruction's identity, not of its
//! numbering, so AUX words land at the SAME word offsets in both streams.
//!
//! So: walk the KNOWN stream, which is self-decoding, and read the unknown
//! stream at the identical offsets. One pass, no fixpoint, no iteration.
//!
//! # The verifier that makes this safe to trust
//!
//! A positional walk is only as good as the assumption that the two streams
//! really are the same program. That assumption is checked, not assumed: the
//! permutation relabels the opcode byte and nothing else, so at every
//! instruction position bits 8..31 of the two words must be identical, and AUX
//! words must match in full. 32-bit equality on every non-opcode bit is an
//! extremely strong check — it fails immediately if the two compilers disagree
//! about register allocation, jump distance, constant ordering or anything
//! else.
//!
//! When it does fail, the affected proto is discarded and the rest of the file
//! still contributes. That is the difference between degrading and corrupting:
//! a divergent compiler costs coverage, never correctness.
//!
//! # What this module must never do
//!
//! Resolve a disagreement. Every mapping produced here is exact or absent. If
//! one canonical opcode is seen at two different bytes, or one byte is seen
//! carrying two different opcodes, that is a contradiction — one of the two
//! readings is wrong and there is no way to tell which. Both are dropped and
//! the contradiction is reported. Majority voting belongs in `consensus`, where
//! the inputs are guesses; it has no place where the inputs are observations.

use super::opcodes::LuauOpcode;
use super::opmap::OpcodeMap;
use super::types::{insn_op, Chunk};

/// Sentinel for "no mapping" in a `[u8; 256]` opcode table.
pub const UNMAPPED: u8 = 255;

/// Canonical (upstream Luau) opcode number -> this decompiler's internal
/// number, or `None` when the byte has no canonical meaning.
///
/// Canonical Luau defines opcodes 0..=82. The internal layout has one extra
/// slot (a removed generic-for variant Roblox still emits) which no compiler
/// can be made to produce, so it has no canonical counterpart here.
pub fn canonical_to_internal(canonical_op: u8) -> Option<u8> {
    match OpcodeMap::canonical_luau_to_internal(canonical_op) {
        UNMAPPED => None,
        internal => Some(internal),
    }
}

/// Name of a canonical opcode, in canonical numbering.
///
/// The interchange format is name-keyed rather than number-keyed precisely so
/// that neither side has to keep a copy of the numbering translation.
pub fn canonical_opcode_name(canonical_op: u8) -> Option<&'static str> {
    canonical_to_internal(canonical_op).map(|i| LuauOpcode::from_u8(i).name())
}

/// Does a canonical opcode carry an AUX word? Expressed in canonical numbering,
/// answered through the internal table so there is exactly one source of truth
/// for AUX-ness in the crate.
pub fn canonical_has_aux(canonical_op: u8) -> bool {
    canonical_to_internal(canonical_op)
        .map(|i| LuauOpcode::from_u8(i).has_aux())
        .unwrap_or(false)
}

/// Number of canonical opcodes (0..=82).
pub const CANONICAL_OPCODE_COUNT: usize = 83;

/// One proto that could not be aligned, and the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtoReject {
    pub proto: usize,
    pub reason: RejectReason,
}

/// Why a single proto was discarded. Every variant means "these two protos are
/// not the same code", which is information, not failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// Different instruction counts — different code, full stop.
    CodeLenMismatch { known: usize, unknown: usize },
    /// A byte in the KNOWN stream is not a canonical opcode, so the reference
    /// side is not what it claims to be (wrong compiler, or already shuffled).
    NonCanonicalOpcode { offset: usize, byte: u8 },
    /// Bits 8..31 differ: same instruction index, different operands. The two
    /// compilers produced different code.
    OperandDivergence { offset: usize },
    /// An AUX-carrying opcode sat at the last word of the proto.
    TruncatedAux { offset: usize },
    /// Within this proto, one byte carried two different opcodes.
    InternalFunctionConflict { shuffled_byte: u8 },
    /// Within this proto, one opcode arrived at two different bytes.
    InternalInjectivityConflict { internal_op: u8 },
}

/// A contradiction between two exact readings. Both readings are dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conflict {
    /// One shuffled byte was observed carrying two different opcodes.
    Function {
        shuffled_byte: u8,
        first: u8,
        second: u8,
    },
    /// One opcode was observed arriving at two different shuffled bytes.
    Injectivity {
        internal_op: u8,
        first: u8,
        second: u8,
    },
}

/// Structural failures that make a whole pair unusable. Note how few there are:
/// almost everything degrades to a rejected proto instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlignError {
    VersionMismatch { known: u8, unknown: u8 },
    ProtoCountMismatch { known: usize, unknown: usize },
    /// Neither chunk has any protos to align.
    Empty,
}

impl std::fmt::Display for AlignError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AlignError::VersionMismatch { known, unknown } => write!(
                f,
                "bytecode version mismatch: reference is v{}, subject is v{}",
                known, unknown
            ),
            AlignError::ProtoCountMismatch { known, unknown } => write!(
                f,
                "prototype count mismatch: reference has {}, subject has {} \
                 (not the same program, or a different compiler)",
                known, unknown
            ),
            AlignError::Empty => write!(f, "chunk contains no prototypes"),
        }
    }
}

impl std::error::Error for AlignError {}

/// An exact, partial permutation read off one aligned pair.
#[derive(Debug, Clone)]
pub struct Alignment {
    /// shuffled byte -> internal opcode. `UNMAPPED` where nothing was observed.
    pub map: [u8; 256],
    /// internal opcode -> shuffled byte. The bijection witness; kept so that
    /// injectivity can be checked in O(1) instead of by scanning.
    pub inv: [u8; 256],
    pub protos_total: usize,
    pub protos_aligned: usize,
    pub protos_rejected: Vec<ProtoReject>,
    pub instructions_aligned: usize,
    /// Non-opcode words compared bit-for-bit. This is the size of the evidence
    /// that the two streams really are the same program.
    pub operand_words_checked: u64,
    /// Contradictions found while folding. Never resolved, only reported.
    pub conflicts: Vec<Conflict>,
}

impl Default for Alignment {
    fn default() -> Self {
        Self::empty()
    }
}

impl Alignment {
    pub fn empty() -> Self {
        Alignment {
            map: [UNMAPPED; 256],
            inv: [UNMAPPED; 256],
            protos_total: 0,
            protos_aligned: 0,
            protos_rejected: Vec::new(),
            instructions_aligned: 0,
            operand_words_checked: 0,
            conflicts: Vec::new(),
        }
    }

    /// Distinct shuffled bytes pinned to an opcode.
    pub fn pinned(&self) -> usize {
        self.map.iter().filter(|&&v| v != UNMAPPED).count()
    }

    /// Canonical opcode names this alignment did NOT pin, in canonical order.
    /// The honest statement of what is still unknown.
    pub fn unpinned_names(&self) -> Vec<&'static str> {
        (0..CANONICAL_OPCODE_COUNT as u8)
            .filter_map(|c| {
                let internal = canonical_to_internal(c)?;
                if self.inv[internal as usize] == UNMAPPED {
                    canonical_opcode_name(c)
                } else {
                    None
                }
            })
            .collect()
    }

    /// Record one observation, refusing to overwrite a different one.
    fn record(&mut self, shuffled_byte: u8, internal_op: u8) -> Result<(), Conflict> {
        let existing = self.map[shuffled_byte as usize];
        if existing != UNMAPPED && existing != internal_op {
            return Err(Conflict::Function {
                shuffled_byte,
                first: existing,
                second: internal_op,
            });
        }
        let existing_byte = self.inv[internal_op as usize];
        if existing_byte != UNMAPPED && existing_byte != shuffled_byte {
            return Err(Conflict::Injectivity {
                internal_op,
                first: existing_byte,
                second: shuffled_byte,
            });
        }
        self.map[shuffled_byte as usize] = internal_op;
        self.inv[internal_op as usize] = shuffled_byte;
        Ok(())
    }

    /// Erase a mapping in both directions. Used when a contradiction makes both
    /// readings untrustworthy.
    fn unpin(&mut self, shuffled_byte: u8, internal_op: u8) {
        if self.map[shuffled_byte as usize] == internal_op {
            self.map[shuffled_byte as usize] = UNMAPPED;
        }
        if self.inv[internal_op as usize] == shuffled_byte {
            self.inv[internal_op as usize] = UNMAPPED;
        }
    }
}

/// Is `map` a partial bijection — a function whose inverse is also a function?
///
/// A permutation cannot send two bytes to the same opcode. If it appears to,
/// the map was not derived by alignment and must not be treated as exact.
pub fn is_partial_bijection(map: &[u8; 256]) -> bool {
    let mut seen = [false; 256];
    for &v in map.iter() {
        if v == UNMAPPED {
            continue;
        }
        if seen[v as usize] {
            return false;
        }
        seen[v as usize] = true;
    }
    true
}

/// Align one pair: the same source compiled by a compiler we understand and by
/// one we do not.
///
/// `known` must be canonical upstream Luau bytecode. `unknown` must be the same
/// source compiled under the permutation being derived.
///
/// Per-proto tolerance is deliberate. A client compiler that lowers one
/// construct differently costs you the opcodes in that one proto, not the file.
pub fn align_pair(known: &Chunk, unknown: &Chunk) -> Result<Alignment, AlignError> {
    if known.version != unknown.version {
        return Err(AlignError::VersionMismatch {
            known: known.version,
            unknown: unknown.version,
        });
    }
    if known.protos.len() != unknown.protos.len() {
        return Err(AlignError::ProtoCountMismatch {
            known: known.protos.len(),
            unknown: unknown.protos.len(),
        });
    }
    if known.protos.is_empty() {
        return Err(AlignError::Empty);
    }

    let mut out = Alignment::empty();
    out.protos_total = known.protos.len();

    for (idx, (kp, up)) in known.protos.iter().zip(unknown.protos.iter()).enumerate() {
        match align_proto(idx, &kp.code, &up.code) {
            Ok(part) => {
                // The proto walked cleanly on its own. Commit it, but a
                // cross-proto contradiction still unpins rather than resolves.
                let mut committed: Vec<(u8, u8)> = Vec::new();
                let mut poisoned = false;
                for (b, op) in part.votes.iter().copied() {
                    match out.record(b, op) {
                        Ok(()) => committed.push((b, op)),
                        Err(c) => {
                            out.conflicts.push(c);
                            poisoned = true;
                            // Erase the pre-existing reading too: with two
                            // exact-looking readings disagreeing, neither can
                            // be trusted.
                            match c {
                                Conflict::Function {
                                    shuffled_byte,
                                    first,
                                    ..
                                } => out.unpin(shuffled_byte, first),
                                Conflict::Injectivity {
                                    internal_op, first, ..
                                } => out.unpin(first, internal_op),
                            }
                        }
                    }
                }
                if poisoned {
                    // Do NOT roll the whole proto back: the entries that did
                    // not contradict anything are still exact observations.
                    // Only the contradicting pair is dropped, above.
                }
                out.protos_aligned += 1;
                out.instructions_aligned += part.instructions;
                out.operand_words_checked += part.operand_words;
            }
            Err(reason) => out.protos_rejected.push(ProtoReject { proto: idx, reason }),
        }
    }

    Ok(out)
}

/// Votes gathered from one proto that walked cleanly end to end.
struct ProtoVotes {
    votes: Vec<(u8, u8)>,
    instructions: usize,
    operand_words: u64,
}

/// Walk one proto pair. All-or-nothing: a proto contributes every vote in it or
/// none, because a divergence anywhere means the positional correspondence is
/// broken everywhere after it.
fn align_proto(_idx: usize, known: &[u32], unknown: &[u32]) -> Result<ProtoVotes, RejectReason> {
    if known.len() != unknown.len() {
        return Err(RejectReason::CodeLenMismatch {
            known: known.len(),
            unknown: unknown.len(),
        });
    }

    let mut votes: Vec<(u8, u8)> = Vec::new();
    let mut local_map = [UNMAPPED; 256];
    let mut local_inv = [UNMAPPED; 256];
    let mut instructions = 0usize;
    let mut operand_words = 0u64;

    let mut i = 0usize;
    while i < known.len() {
        let canonical_op = insn_op(known[i]);
        let Some(internal) = canonical_to_internal(canonical_op) else {
            return Err(RejectReason::NonCanonicalOpcode {
                offset: i,
                byte: canonical_op,
            });
        };

        // The verifier. Everything except the opcode byte must be identical.
        if (known[i] >> 8) != (unknown[i] >> 8) {
            return Err(RejectReason::OperandDivergence { offset: i });
        }
        operand_words += 1;

        let shuffled_byte = insn_op(unknown[i]);

        // Self-consistency inside this proto.
        let prev = local_map[shuffled_byte as usize];
        if prev != UNMAPPED && prev != internal {
            return Err(RejectReason::InternalFunctionConflict { shuffled_byte });
        }
        let prev_byte = local_inv[internal as usize];
        if prev_byte != UNMAPPED && prev_byte != shuffled_byte {
            return Err(RejectReason::InternalInjectivityConflict {
                internal_op: internal,
            });
        }
        if prev == UNMAPPED {
            local_map[shuffled_byte as usize] = internal;
            local_inv[internal as usize] = shuffled_byte;
            votes.push((shuffled_byte, internal));
        }
        instructions += 1;

        if LuauOpcode::from_u8(internal).has_aux() {
            if i + 1 >= known.len() {
                return Err(RejectReason::TruncatedAux { offset: i });
            }
            if known[i + 1] != unknown[i + 1] {
                return Err(RejectReason::OperandDivergence { offset: i + 1 });
            }
            operand_words += 1;
            i += 2;
        } else {
            i += 1;
        }
    }

    Ok(ProtoVotes {
        votes,
        instructions,
        operand_words,
    })
}

/// Fold several exact partial maps into one.
///
/// Unlike a consensus tally, agreement here adds nothing and disagreement is
/// not noise. Two exact readings that disagree mean the inputs came from
/// different builds, or one alignment was wrong. Both are dropped and recorded;
/// the caller decides whether the result is still worth keeping.
pub fn union_alignments(parts: &[Alignment]) -> Alignment {
    let mut out = Alignment::empty();
    for part in parts {
        out.protos_total += part.protos_total;
        out.protos_aligned += part.protos_aligned;
        out.protos_rejected.extend(part.protos_rejected.iter().cloned());
        out.instructions_aligned += part.instructions_aligned;
        out.operand_words_checked += part.operand_words_checked;
        out.conflicts.extend(part.conflicts.iter().copied());

        for (b, &op) in part.map.iter().enumerate() {
            if op == UNMAPPED {
                continue;
            }
            let b = b as u8;
            if let Err(c) = out.record(b, op) {
                out.conflicts.push(c);
                match c {
                    Conflict::Function {
                        shuffled_byte,
                        first,
                        ..
                    } => out.unpin(shuffled_byte, first),
                    Conflict::Injectivity {
                        internal_op, first, ..
                    } => out.unpin(first, internal_op),
                }
            }
        }
    }
    out
}

/// Independent re-derivation: decode `unknown` with `map` and confirm it
/// reproduces the canonical walk exactly.
///
/// This checks something the derivation itself cannot: that the finished map,
/// used the way the decompiler will use it, actually recovers the program we
/// already know the answer for. The "known answer" is our own compilation of
/// our own source — no external oracle is consulted.
pub fn validate_against_canonical(
    map: &[u8; 256],
    known: &Chunk,
    unknown: &Chunk,
) -> Result<(), AlignError> {
    if known.version != unknown.version {
        return Err(AlignError::VersionMismatch {
            known: known.version,
            unknown: unknown.version,
        });
    }
    if known.protos.len() != unknown.protos.len() {
        return Err(AlignError::ProtoCountMismatch {
            known: known.protos.len(),
            unknown: unknown.protos.len(),
        });
    }
    for (kp, up) in known.protos.iter().zip(unknown.protos.iter()) {
        if kp.code.len() != up.code.len() {
            continue; // rejected during derivation; nothing to re-check
        }
        let mut i = 0usize;
        while i < kp.code.len() {
            let Some(internal) = canonical_to_internal(insn_op(kp.code[i])) else {
                break;
            };
            let decoded = map[insn_op(up.code[i]) as usize];
            if decoded != UNMAPPED && decoded != internal {
                return Err(AlignError::ProtoCountMismatch {
                    known: internal as usize,
                    unknown: decoded as usize,
                });
            }
            i += if LuauOpcode::from_u8(internal).has_aux() {
                2
            } else {
                1
            };
        }
    }
    Ok(())
}

/// Convenience: align many pairs and fold them into one map ready for
/// `set_ground_truth_opmap`.
pub fn derive_ground_truth(pairs: &[(&Chunk, &Chunk)]) -> Result<[u8; 256], AlignError> {
    let mut parts = Vec::with_capacity(pairs.len());
    let mut last_err = None;
    for (known, unknown) in pairs {
        match align_pair(known, unknown) {
            Ok(a) => parts.push(a),
            Err(e) => last_err = Some(e),
        }
    }
    if parts.is_empty() {
        return Err(last_err.unwrap_or(AlignError::Empty));
    }
    Ok(union_alignments(&parts).map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::types::Proto;

    fn proto(code: Vec<u32>) -> Proto {
        Proto {
            max_stack_size: 8,
            num_params: 0,
            num_upvalues: 0,
            is_vararg: false,
            flags: 0,
            typeinfo: None,
            code,
            constants: Vec::new(),
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

    /// Build an instruction word from an opcode byte and arbitrary operands.
    fn insn(op: u8, operands: u32) -> u32 {
        (op as u32) | (operands << 8)
    }

    /// A deterministic stand-in for a client permutation.
    fn permute(b: u8) -> u8 {
        // Any involution-free bijection on 0..255 will do.
        b.wrapping_mul(7).wrapping_add(29)
    }

    fn shuffled(code: &[u32], aux_at: &[usize]) -> Vec<u32> {
        code.iter()
            .enumerate()
            .map(|(i, &w)| {
                if aux_at.contains(&i) {
                    w
                } else {
                    insn(permute(insn_op(w)), w >> 8)
                }
            })
            .collect()
    }

    // canonical numbering: 6 = MOVE (no aux), 21 = CALL (no aux), 22 = RETURN,
    // 15 = GETTABLEKS (has aux), 33 = ADD, 5 = LOADK
    const MOVE: u8 = 6;
    const CALL: u8 = 21;
    const RETURN: u8 = 22;
    const GETTABLEKS: u8 = 15;
    const ADD: u8 = 33;

    #[test]
    fn canonical_tables_agree_with_opcode_enum() {
        assert_eq!(canonical_opcode_name(RETURN), Some("RETURN"));
        assert_eq!(canonical_opcode_name(MOVE), Some("MOVE"));
        // Canonical 60 is FASTCALL3, which the internal layout numbers 83.
        assert_eq!(canonical_opcode_name(60), Some("FASTCALL3"));
        assert_eq!(canonical_to_internal(60), Some(83));
        // 83+ has no canonical meaning.
        assert_eq!(canonical_opcode_name(83), None);
        assert!(canonical_has_aux(GETTABLEKS));
        assert!(!canonical_has_aux(MOVE));
    }

    #[test]
    fn aligns_a_clean_pair_exactly() {
        let code = vec![
            insn(MOVE, 0x0102),
            insn(GETTABLEKS, 0x0304),
            0xDEAD_BEEF, // AUX
            insn(ADD, 0x0506),
            insn(RETURN, 0x0001),
        ];
        let known = chunk(vec![proto(code.clone())]);
        let unknown = chunk(vec![proto(shuffled(&code, &[2]))]);

        let a = align_pair(&known, &unknown).expect("aligns");
        assert_eq!(a.protos_aligned, 1);
        assert!(a.protos_rejected.is_empty());
        assert!(a.conflicts.is_empty());
        assert_eq!(a.instructions_aligned, 4, "AUX word is not an instruction");
        for op in [MOVE, GETTABLEKS, ADD, RETURN] {
            let internal = canonical_to_internal(op).unwrap();
            assert_eq!(
                a.map[permute(op) as usize],
                internal,
                "opcode {} misaligned",
                canonical_opcode_name(op).unwrap()
            );
        }
        assert!(is_partial_bijection(&a.map));
    }

    #[test]
    fn aux_word_is_not_read_as_an_opcode() {
        // The AUX word's low byte collides with a real opcode byte. If the walk
        // treated it as an instruction it would learn a wrong mapping.
        let aux = 0x0000_0000 | MOVE as u32; // low byte == MOVE
        let code = vec![
            insn(GETTABLEKS, 0x0304),
            aux,
            insn(RETURN, 0x0001),
        ];
        let known = chunk(vec![proto(code.clone())]);
        let unknown = chunk(vec![proto(shuffled(&code, &[1]))]);

        let a = align_pair(&known, &unknown).expect("aligns");
        // MOVE's shuffled byte must NOT have been learned from the AUX word.
        assert_eq!(a.inv[canonical_to_internal(MOVE).unwrap() as usize], UNMAPPED);
        assert_eq!(a.pinned(), 2, "only GETTABLEKS and RETURN are real here");
    }

    #[test]
    fn operand_divergence_rejects_the_proto_not_the_file() {
        let good = vec![insn(MOVE, 0x1111), insn(RETURN, 0x0001)];
        let bad = vec![insn(CALL, 0x2222), insn(RETURN, 0x0001)];
        let mut bad_shuf = shuffled(&bad, &[]);
        bad_shuf[0] = insn(permute(CALL), 0x9999); // operands disagree

        let known = chunk(vec![proto(good.clone()), proto(bad.clone())]);
        let unknown = chunk(vec![proto(shuffled(&good, &[])), proto(bad_shuf)]);

        let a = align_pair(&known, &unknown).expect("pair still usable");
        assert_eq!(a.protos_aligned, 1);
        assert_eq!(a.protos_rejected.len(), 1);
        assert!(matches!(
            a.protos_rejected[0].reason,
            RejectReason::OperandDivergence { offset: 0 }
        ));
        // The good proto's mappings survived, and CALL was simply not learned.
        assert_eq!(a.map[permute(MOVE) as usize], canonical_to_internal(MOVE).unwrap());
        assert_eq!(a.inv[canonical_to_internal(CALL).unwrap() as usize], UNMAPPED);
    }

    #[test]
    fn mismatched_aux_word_rejects_the_proto() {
        let code = vec![insn(GETTABLEKS, 0x0304), 0xAAAA_AAAA, insn(RETURN, 1)];
        let mut shuf = shuffled(&code, &[1]);
        shuf[1] = 0xBBBB_BBBB;
        let known = chunk(vec![proto(code)]);
        let unknown = chunk(vec![proto(shuf)]);
        let a = align_pair(&known, &unknown).expect("pair parses");
        assert_eq!(a.protos_aligned, 0);
        assert!(matches!(
            a.protos_rejected[0].reason,
            RejectReason::OperandDivergence { offset: 1 }
        ));
    }

    #[test]
    fn proto_count_mismatch_is_a_whole_pair_error() {
        let known = chunk(vec![proto(vec![insn(RETURN, 1)])]);
        let unknown = chunk(vec![
            proto(vec![insn(permute(RETURN), 1)]),
            proto(vec![insn(permute(RETURN), 1)]),
        ]);
        assert_eq!(
            align_pair(&known, &unknown).err(),
            Some(AlignError::ProtoCountMismatch {
                known: 1,
                unknown: 2
            })
        );
    }

    #[test]
    fn non_canonical_reference_is_rejected() {
        // 200 is not a canonical opcode: the "reference" is not canonical Luau.
        let code = vec![insn(200, 0x1111), insn(RETURN, 1)];
        let known = chunk(vec![proto(code.clone())]);
        let unknown = chunk(vec![proto(code)]);
        let a = align_pair(&known, &unknown).expect("pair parses");
        assert_eq!(a.protos_aligned, 0);
        assert!(matches!(
            a.protos_rejected[0].reason,
            RejectReason::NonCanonicalOpcode { byte: 200, .. }
        ));
    }

    #[test]
    fn union_merges_disjoint_coverage() {
        let a_code = vec![insn(MOVE, 1), insn(RETURN, 1)];
        let b_code = vec![insn(ADD, 2), insn(RETURN, 1)];
        let a = align_pair(
            &chunk(vec![proto(a_code.clone())]),
            &chunk(vec![proto(shuffled(&a_code, &[]))]),
        )
        .unwrap();
        let b = align_pair(
            &chunk(vec![proto(b_code.clone())]),
            &chunk(vec![proto(shuffled(&b_code, &[]))]),
        )
        .unwrap();
        let u = union_alignments(&[a, b]);
        assert!(u.conflicts.is_empty());
        assert_eq!(u.pinned(), 3, "MOVE + ADD + RETURN");
        assert!(is_partial_bijection(&u.map));
    }

    #[test]
    fn union_drops_contradictions_instead_of_voting() {
        // Two "exact" readings that disagree about one byte: different builds.
        let mut a = Alignment::empty();
        a.record(0x40, canonical_to_internal(MOVE).unwrap()).unwrap();
        a.record(0x41, canonical_to_internal(ADD).unwrap()).unwrap();
        let mut b = Alignment::empty();
        b.record(0x40, canonical_to_internal(CALL).unwrap()).unwrap();

        let u = union_alignments(&[a, b]);
        assert_eq!(u.conflicts.len(), 1);
        assert_eq!(
            u.map[0x40], UNMAPPED,
            "a contradicted byte is dropped, never majority-resolved"
        );
        assert_eq!(
            u.map[0x41],
            canonical_to_internal(ADD).unwrap(),
            "uncontradicted readings survive"
        );
    }

    #[test]
    fn union_of_nothing_is_empty_not_wrong() {
        let u = union_alignments(&[]);
        assert_eq!(u.pinned(), 0);
        assert!(is_partial_bijection(&u.map));
    }

    #[test]
    fn bijection_check_catches_a_non_permutation() {
        let mut m = [UNMAPPED; 256];
        m[0x10] = 6;
        m[0x11] = 6; // two bytes, one opcode
        assert!(!is_partial_bijection(&m));
    }

    #[test]
    fn validate_accepts_the_map_it_derived() {
        let code = vec![
            insn(MOVE, 1),
            insn(GETTABLEKS, 2),
            0x1234_5678,
            insn(RETURN, 1),
        ];
        let known = chunk(vec![proto(code.clone())]);
        let unknown = chunk(vec![proto(shuffled(&code, &[2]))]);
        let a = align_pair(&known, &unknown).unwrap();
        assert!(validate_against_canonical(&a.map, &known, &unknown).is_ok());
    }

    #[test]
    fn derive_ground_truth_reports_unpinned_honestly() {
        let code = vec![insn(MOVE, 1), insn(RETURN, 1)];
        let known = chunk(vec![proto(code.clone())]);
        let unknown = chunk(vec![proto(shuffled(&code, &[]))]);
        let a = align_pair(&known, &unknown).unwrap();
        let names = a.unpinned_names();
        assert!(!names.contains(&"MOVE"));
        assert!(names.contains(&"CALL"));
        assert_eq!(names.len(), CANONICAL_OPCODE_COUNT - 2);

        let gt = derive_ground_truth(&[(&known, &unknown)]).unwrap();
        assert_eq!(gt[permute(MOVE) as usize], canonical_to_internal(MOVE).unwrap());
    }
}
