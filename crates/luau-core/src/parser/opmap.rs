/// Automatic opcode mapping detection for Roblox-shuffled bytecode.
///
/// Roblox shuffles opcode byte values in each client version. The bytecode format
/// (header, strings, constants, protos) is standard, but the opcode byte in each
/// instruction word is mapped through a secret permutation table.
///
/// This module detects that permutation by analyzing bytecode patterns.

use std::collections::HashMap;
use std::sync::Mutex;
use super::types::*;
use super::opcodes::LuauOpcode;

/// Process-global ground-truth opcode mapping, supplied externally (e.g., by
/// the server's `/api/opmap-probe` endpoint) and applied with top priority at
/// the start of every detection run.
///
/// Ground truth is a direct observation of the Roblox client's shuffle (via
/// `lift_closure(loadstring(src))` round-trip), so it is strictly more
/// authoritative than any heuristic detector or cached consensus. Entries
/// here are LOCKED — validators must never unassign them, and per-script
/// detectors must never reassign their bytes.
static GROUND_TRUTH: Mutex<Option<[u8; 256]>> = Mutex::new(None);

/// Install ground-truth mappings for this process. Later calls fully replace
/// any prior ground truth (so feeding a partially-populated table followed by
/// a more complete one behaves as expected). Pass `None` to clear.
pub fn set_ground_truth(map: Option<[u8; 256]>) {
    let mut lock = GROUND_TRUTH.lock().unwrap_or_else(|p| p.into_inner());
    *lock = map;
}

/// Read the currently-installed ground truth, if any.
pub fn get_ground_truth() -> Option<[u8; 256]> {
    let lock = GROUND_TRUTH.lock().unwrap_or_else(|p| p.into_inner());
    *lock
}

/// Merge a ground-truth map into a prior map. Ground-truth entries override
/// the prior on conflict. Returns the merged map.
///
/// This is exposed for server-side reuse (the server may want to preview the
/// merged prior without touching the global lock).
pub fn merge_ground_truth_into_prior(prior: &[u8; 256], ground_truth: &[u8; 256]) -> [u8; 256] {
    let mut merged = *prior;
    let mut assigned = [false; 256];
    // First pass: drop any prior entries whose *canonical* target is also a
    // target in ground truth — ground truth will re-place them at the right
    // shuffled byte, and leaving the stale prior entry would cause the
    // canonical opcode to be considered "already assigned" when it shouldn't.
    let mut gt_targets = [false; 256];
    for &v in ground_truth.iter() {
        if v != 255 && (v as usize) < LuauOpcode::MAX_OPCODE {
            gt_targets[v as usize] = true;
        }
    }
    for slot in merged.iter_mut() {
        if *slot != 255 && gt_targets[*slot as usize] {
            *slot = 255;
        }
    }
    // Build assigned-set from the cleaned prior.
    for &v in merged.iter() {
        if v != 255 && (v as usize) < LuauOpcode::MAX_OPCODE {
            assigned[v as usize] = true;
        }
    }
    // Apply ground truth. Conflicts on the SAME shuffled byte are overridden
    // (ground truth wins). Canonicals already seen in the prior at a different
    // byte were just cleared above, so the write is safe.
    for (shuffled, &canon) in ground_truth.iter().enumerate() {
        if canon == 255 || (canon as usize) >= LuauOpcode::MAX_OPCODE {
            continue;
        }
        // If some OTHER slot in merged already maps to `canon`, clear that
        // slot so this write doesn't double-assign the canonical.
        for other in 0..256 {
            if other != shuffled && merged[other] == canon {
                merged[other] = 255;
            }
        }
        merged[shuffled] = canon;
        assigned[canon as usize] = true;
    }
    let _ = assigned; // currently unused past construction, retained for clarity.
    merged
}

/// Mutable detection context that holds the mapping state
struct DetectCtx {
    /// shuffled_to_standard[shuffled_byte] = standard Luau opcode byte (255 = unmapped)
    map: [u8; 256],
    /// Which standard opcodes have been assigned
    assigned: [bool; 256],
    /// Frequency of each shuffled opcode byte across all protos
    freq: [u32; 256],
    /// Total instruction count
    total_insns: u32,
    /// Evidence count per shuffled byte — how many detector passes confirmed this mapping.
    /// Higher evidence = higher confidence. Used for conflict resolution when merging
    /// opcode maps from different scripts.
    evidence: [u16; 256],
    /// Cache-seeded entries that must NOT be unassigned by validators.
    /// When true, the entry came from an external prior (e.g., per-build cache
    /// of consensus across many scripts) and is more authoritative than any
    /// single-file heuristic — validators must leave it alone.
    locked: [bool; 256],
}

impl DetectCtx {
    fn new() -> Self {
        Self {
            map: [255u8; 256],
            assigned: [false; 256],
            freq: [0u32; 256],
            total_insns: 0,
            evidence: [0u16; 256],
            locked: [false; 256],
        }
    }

    fn compute_frequencies(&mut self, chunk: &Chunk) {
        self.freq = [0u32; 256];
        self.total_insns = 0;
        for proto in &chunk.protos {
            for &insn in &proto.code {
                let op = insn_op(insn);
                self.freq[op as usize] += 1;
                self.total_insns += 1;
            }
        }
    }

    /// Check if a standard opcode is expected to be rare (should have very low frequency)
    fn is_rare_standard_opcode(standard: u8) -> bool {
        matches!(LuauOpcode::from_u8(standard),
            LuauOpcode::Nop | LuauOpcode::Break | LuauOpcode::Coverage
            | LuauOpcode::NativeCall | LuauOpcode::IDiv | LuauOpcode::IDivK
            | LuauOpcode::SubRK | LuauOpcode::DivRK | LuauOpcode::LoadKX
            | LuauOpcode::FastCall3 | LuauOpcode::Deprecated61
            // Phase B0: JumpX is the long-jump escape hatch, emitted only when
            // 16-bit D overflows. Empirically 0-5 per chunk, so `try_assign`'s
            // 2%-of-total guard is defense-in-depth against stealing
            // high-frequency AD-format bytes (LoadN/LoadK etc.) whose
            // `(insn >> 8)` trivially exceeds the |e|>127 JumpX filter.
            | LuauOpcode::JumpX
        )
    }

    /// Check if a standard opcode is expected to be very common
    fn is_common_standard_opcode(standard: u8) -> bool {
        matches!(LuauOpcode::from_u8(standard),
            LuauOpcode::Call | LuauOpcode::Return | LuauOpcode::Move
            | LuauOpcode::GetTableKS | LuauOpcode::LoadK | LuauOpcode::GetImport
            | LuauOpcode::Jump | LuauOpcode::JumpIfNot
        )
    }

    /// Opcodes that require STRUCTURAL evidence — no amount of frequency,
    /// AUX-shape, or format validation is sufficient; the byte must be proven
    /// to match a dedicated control-flow / table-construction pattern.
    ///
    /// When a structural-required opcode's dedicated detector can't find its
    /// byte, downstream passes (frequency-rank matching, permutation completion,
    /// scored greedy matching, instruction-position inference) MUST NOT guess.
    /// The result is that the opcode stays UNMAPPED for this file — the lifter
    /// will emit `-- unresolved` comments for affected instructions, which is
    /// strictly better than silently emitting `(-tbl).field` garbage because
    /// NEWTABLE was mapped to NOT, or returning nonsense because FORGLOOP was
    /// mapped to some random AD-format byte.
    ///
    /// These opcodes will eventually be detected on some OTHER script where
    /// the structural signal is clear, and cache consensus (cache-as-prior)
    /// will propagate that mapping to this file on the next rejoin.
    fn is_structural_required_standard_opcode(standard: u8) -> bool {
        matches!(LuauOpcode::from_u8(standard),
            LuauOpcode::NewTable
            | LuauOpcode::ForGLoop
            | LuauOpcode::ForGPrep
            | LuauOpcode::ForGPrepINext
            | LuauOpcode::ForGPrepNext
            | LuauOpcode::ForNPrep
            | LuauOpcode::ForNLoop
            // Unary ops: format (C=0, A<ms, B<ms) is shared with Move, GetUpval,
            // SetUpval, DupTable etc., so blind format-based assignment picks
            // wrong bytes. These MUST be assigned by context-validated detectors
            // (detect_unary_not_minus, detect_unary_ops) or left unmapped.
            // On ModuleScript.luac (1 MINUS + 1 LENGTH + 1 NOT across 600+ insns)
            // the greedy scored pass was stealing 0xC1/0xF6/0x1C for them when
            // the real bytes are 0x39/0x1C and 0x__ respectively.
            | LuauOpcode::Not
            | LuauOpcode::Minus
            | LuauOpcode::Length
            // Phase B0.34: JUMPBACK is structurally required for correct for-loop
            // and while-loop handling. Pre-B0.33 cache has variant 0 with
            // JUMPBACK@0x6E (wrong — 0x6E is FORGLOOP). The B0.33 detector fix
            // produces correct 0x48→24 for v0-shaped scripts, but the Tier 8
            // augmenter's bulk-replace can restore the stale 0x6E→24 from the
            // known-shuffle variant. Adding JUMPBACK here ensures the revert
            // path fires when augmenter disagrees with the heuristic.
            // Unanimity across variants is impossible (v0=0x6E, v2/v3=0x48), so
            // revert uniformly leaves JUMPBACK unmapped after augmenter, enabling
            // the post-augmenter detect_jumpback re-run to pick the correct byte.
            | LuauOpcode::JumpBack
        )
    }

    /// Structural possibility gate: given a chunk, return false if the standard
    /// opcode CANNOT appear in this file's bytecode based on hard invariants
    /// (not just frequency heuristics).
    ///
    /// Used by fallback passes (permutation_complete, infer_from_instruction_positions)
    /// to exclude opcodes that would never be emitted by the Luau compiler for the
    /// given bytecode. Without this gate, fallback passes can greedily assign bytes
    /// to opcodes like LoadKX even on small files, stealing the bytes from their
    /// real (rarer) opcodes.
    /// Returns true if `standard` opcode can plausibly appear in this chunk.
    ///
    /// IMPORTANT: This guard is used by permutation_complete to prevent false-positive
    /// assignments. It must be CONSERVATIVE — only exclude opcodes that are truly
    /// structurally impossible in this chunk. A wrong assignment here poisons the
    /// entire opmap by taking a slot that belongs to a different opcode.
    ///
    /// LoadKX: permutation_complete still requires > 32768 constants as a guard
    /// against false assignment (without structural evidence, guessing LoadKX is
    /// dangerous). detect_loadkx uses D=0 purity instead and bypasses this check.
    fn opcode_can_appear_in_chunk(chunk: &Chunk, standard: u8) -> bool {
        match LuauOpcode::from_u8(standard) {
            LuauOpcode::LoadKX => chunk.protos.iter().any(|p| p.constants.len() > 32768),
            _ => true,
        }
    }

    fn try_assign(&mut self, shuffled: u8, standard: u8) -> bool {
        if self.map[shuffled as usize] != 255 {
            // Already mapped — if it matches, count as additional evidence
            if self.map[shuffled as usize] == standard {
                self.evidence[shuffled as usize] = self.evidence[shuffled as usize].saturating_add(1);
                return true;
            }
            return false;
        }
        if self.assigned[standard as usize] {
            return false;
        }
        // Frequency sanity check: prevent rare opcodes from being mapped to
        // high-frequency shuffled bytes (likely a false positive)
        let freq = self.freq[shuffled as usize];
        if self.total_insns > 100 {
            // Rare opcodes should NOT appear more than 2% of all instructions
            if Self::is_rare_standard_opcode(standard) && freq > self.total_insns / 50 {
                return false;
            }
            // Common opcodes should NOT be mapped to extremely low-frequency bytes
            // (less than 0.02% of total) — unless we have very few instructions.
            // Threshold is very low because data-heavy scripts have control flow opcodes
            // appearing very infrequently relative to data opcodes.
            if Self::is_common_standard_opcode(standard) && self.total_insns > 500 && freq < self.total_insns / 5000 {
                return false;
            }
        }
        #[cfg(test)]
        if std::env::var("OPMAP_TRACE").is_ok() {
            eprintln!("[try_assign] 0x{:02X} -> {} ({:?})", shuffled, standard, LuauOpcode::from_u8(standard));
        }
        self.map[shuffled as usize] = standard;
        self.assigned[standard as usize] = true;
        self.evidence[shuffled as usize] = 1;
        true
    }

    /// Force-assign a mapping, bypassing frequency checks.
    /// Use only when the detection method has very high structural confidence
    /// (e.g., RETURN via last-instruction analysis, GETIMPORT via AUX pattern).
    /// Awards 2 evidence points (higher confidence than regular try_assign).
    fn try_assign_force(&mut self, shuffled: u8, standard: u8) -> bool {
        if self.map[shuffled as usize] != 255 {
            // Already mapped — if it matches, count as additional evidence
            if self.map[shuffled as usize] == standard {
                self.evidence[shuffled as usize] = self.evidence[shuffled as usize].saturating_add(2);
                return true;
            }
            return false;
        }
        if self.assigned[standard as usize] {
            return false;
        }
        #[cfg(test)]
        if std::env::var("OPMAP_TRACE").is_ok() {
            eprintln!("[try_assign_force] 0x{:02X} -> {} ({:?})", shuffled, standard, LuauOpcode::from_u8(standard));
        }
        self.map[shuffled as usize] = standard;
        self.assigned[standard as usize] = true;
        self.evidence[shuffled as usize] = 2; // force-assign = higher base confidence
        true
    }

    /// Claim a byte even if a weaker detector already holds it.
    ///
    /// ── WHY THIS EXISTS ─────────────────────────────────────────────────
    /// Detectors run in a fixed order and `try_assign`/`try_assign_force`
    /// both refuse a byte that is already mapped. So the FIRST detector to
    /// guess wins permanently, regardless of how weak its evidence was —
    /// several force-assign on a threshold of 1.
    ///
    /// Measured consequence: in CameraModule, `detect_closure_capture`
    /// (invoked at line 790, threshold `count >= 1`) claimed 0x9F for
    /// CAPTURE. CALL was then never assigned at all, because `detect_call`
    /// runs at line 827 and skips already-mapped bytes. Every call in the
    /// chunk decoded as a no-op, and a 32-proto module produced `return {}`.
    ///
    /// Tightening the CAPTURE detector did NOT fix it — `detect_duptable`
    /// (line 792) simply took the byte instead, and that change regressed a
    /// previously-passing chunk. Ordering is the fault, not any one detector,
    /// so the fix is to let strong evidence displace weak evidence rather
    /// than to reshuffle who guesses first.
    ///
    /// `min_evidence_to_beat` is the evidence level at or below which the
    /// incumbent is considered a guess worth overriding. A detector should
    /// only call this when its own discriminant is genuinely stronger than a
    /// structural coincidence — CALL's C-distribution test qualifies; a
    /// "these bytes look similar" heuristic does not.
    fn try_assign_override(&mut self, shuffled: u8, standard: u8, min_evidence_to_beat: u16) -> bool {
        let current = self.map[shuffled as usize];
        if current == standard {
            self.evidence[shuffled as usize] =
                self.evidence[shuffled as usize].saturating_add(2);
            return true;
        }
        if current != 255 {
            // Never displace an externally-seeded entry: those come from a
            // consensus across many scripts and outrank any single-file test.
            if self.locked[shuffled as usize] {
                return false;
            }
            if self.evidence[shuffled as usize] > min_evidence_to_beat {
                return false;
            }
            // Release the standard opcode the incumbent held, or it stays
            // marked assigned and can never be detected on another byte.
            self.assigned[current as usize] = false;
            self.map[shuffled as usize] = 255;
            self.evidence[shuffled as usize] = 0;
        }
        if self.assigned[standard as usize] {
            return false;
        }
        self.map[shuffled as usize] = standard;
        self.assigned[standard as usize] = true;
        self.evidence[shuffled as usize] = 3;
        true
    }

    fn is_mapped(&self, shuffled: u8) -> bool {
        self.map[shuffled as usize] != 255
    }

    /// How much evidence backs the current mapping of `shuffled`.
    /// 0 = unmapped, 1 = try_assign, 2 = force-assign, 3+ = corroborated.
    fn evidence_for(&self, shuffled: u8) -> u16 {
        self.evidence[shuffled as usize]
    }

    fn find_shuffled(&self, standard: u8) -> Option<u8> {
        self.map.iter().position(|&v| v == standard).map(|i| i as u8)
    }
}

/// Detected opcode mapping result
pub struct OpcodeMap {
    pub shuffled_to_standard: [u8; 256],
    pub mapped_count: usize,
    /// Map snapshot taken BEFORE speculative completion (permutation_complete).
    /// Only contains high-confidence heuristic detections — safe to cache.
    pub heuristic_map: [u8; 256],
    pub heuristic_count: usize,
    /// Evidence counts per shuffled byte from the heuristic detection phase.
    /// Higher values = more detector passes confirmed this mapping.
    /// Used for conflict resolution when merging maps across scripts.
    pub heuristic_evidence: [u16; 256],
    /// The exact map that was handed to `permutation_complete`, i.e. everything
    /// that had real evidence behind it — detector findings plus, on the cached
    /// path, the merged prior. Distinct from `heuristic_map`, which is always
    /// *this file's* detections only. Bytes present here are evidence-backed;
    /// bytes that appear only in `shuffled_to_standard` were invented by the
    /// bijection-completion tier and carry no evidence at all.
    pub pre_completion_map: [u8; 256],
}

/// Per-file confidence summary for a detected opcode map.
///
/// Every count here is over *true instruction positions*. `compute_frequencies`
/// counts every 32-bit word in the code array, including AUX data words, so a
/// raw frequency table is not a usable denominator: an AUX word carrying the
/// value 4 in its low byte is indistinguishable from an occurrence of opcode
/// byte 0x04. `walk_instruction_positions` skips AUX using the map's own
/// knowledge of which opcodes carry one.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpmapCoverage {
    /// Distinct shuffled bytes that actually occur at an instruction position.
    /// This is the true denominator — the only bytes whose mapping can affect
    /// the decode of this file.
    pub present_bytes: usize,
    /// Of `present_bytes`, how many were assigned before speculative completion.
    pub present_confident: usize,
    /// Of `present_bytes`, how many were supplied by bijection completion alone.
    /// These decode as a concrete opcode with nothing behind the choice — the
    /// silent-wrongness bucket.
    pub present_invented: usize,
    /// Of `present_bytes`, how many are still unmapped in the final map and will
    /// therefore surface as unresolved instructions. Honest doubt.
    pub present_unmapped: usize,
    /// Total instruction words in the chunk.
    pub insn_words: u32,
    /// Instruction words whose opcode byte was assigned before completion.
    pub insn_words_confident: u32,
    /// Instruction words decoded through a completion-invented mapping.
    pub insn_words_invented: u32,
    /// Mappings in the final map for bytes that never occur at an instruction
    /// position. These are pure bijection filler: they cannot affect this file's
    /// decode, but their presence is what makes `mapped_count` a misleading
    /// confidence signal.
    pub ghost_mappings: usize,
}

impl OpmapCoverage {
    /// Share of the program (weighted by instruction words, not by distinct
    /// bytes) that was decoded through an evidence-backed mapping.
    pub fn confidence_pct(&self) -> u32 {
        if self.insn_words == 0 { return 100; }
        (self.insn_words_confident as u64 * 100 / self.insn_words as u64) as u32
    }
}

/// Did a chunk decode cleanly under a candidate map, and if not, where did it
/// break? Named failures, because "this map does not fit this chunk" is the
/// single most useful thing a lookup can report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalkVerdict {
    Clean,
    /// The map has no entry for a byte the chunk actually executes.
    UnmappedByte { byte: u8, proto: usize, pc: usize },
    /// AUX skipping stepped past the end of a prototype, so the map disagrees
    /// with this chunk about which instructions carry an AUX word.
    OverranProto { proto: usize },
}

impl WalkVerdict {
    pub fn is_clean(&self) -> bool {
        matches!(self, WalkVerdict::Clean)
    }

    pub fn describe(&self) -> String {
        match self {
            WalkVerdict::Clean => "walks cleanly".to_string(),
            WalkVerdict::UnmappedByte { byte, proto, pc } => format!(
                "no mapping for byte 0x{:02X} (proto {}, pc {})",
                byte, proto, pc
            ),
            WalkVerdict::OverranProto { proto } => {
                format!("AUX skipping overran proto {}", proto)
            }
        }
    }
}

/// The result of [`OpcodeMap::walk_verify`].
#[derive(Debug, Clone)]
pub struct WalkReport {
    pub verdict: WalkVerdict,
    /// Bytes seen at a true instruction position before the walk stopped.
    pub present: [bool; 256],
    pub insn_words: u32,
}

impl WalkReport {
    /// How many distinct bytes the chunk actually executes.
    pub fn present_bytes(&self) -> usize {
        self.present.iter().filter(|&&p| p).count()
    }
}

/// Walk the instruction stream, counting only true instruction positions.
///
/// AUX words are skipped using `map`'s knowledge of which opcodes carry one.
/// An unmapped byte is assumed to carry no AUX and is stepped over by one —
/// the same conservative assumption `infer_from_instruction_positions` makes.
fn walk_instruction_positions(chunk: &Chunk, map: &[u8; 256]) -> ([u32; 256], u32) {
    let mut freq = [0u32; 256];
    let mut total = 0u32;
    for proto in &chunk.protos {
        let code = &proto.code;
        let mut i = 0usize;
        while i < code.len() {
            let op = insn_op(code[i]);
            freq[op as usize] += 1;
            total += 1;
            let mapped = map[op as usize];
            if mapped != 255 && LuauOpcode::from_u8(mapped).has_aux() {
                i += 2;
            } else {
                i += 1;
            }
        }
    }
    (freq, total)
}

impl OpcodeMap {
    /// Run permutation completion on an externally-provided map array.
    /// Used when merging cached heuristic maps with fresh detections —
    /// the caller passes the merged map and this function fills remaining gaps.
    pub fn permutation_complete_map(map: &mut [u8; 256], chunk: &Chunk) {
        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(chunk);
        // Load the existing map into the context
        // NOTE: upper bound is 95 (not 91) to include Roblox-specific extensions 92-95.
        for (shuffled, &standard) in map.iter().enumerate() {
            if standard != 255 && (standard as usize) < LuauOpcode::MAX_OPCODE {
                ctx.map[shuffled] = standard;
                ctx.assigned[standard as usize] = true;
            }
        }
        // Run permutation completion to fill remaining gaps
        permutation_complete(chunk, &mut ctx);
        // Write back to the caller's map
        *map = ctx.map;
    }

    /// Check if bytecode appears to use shuffled opcodes (values > 83)
    pub fn needs_remapping(chunk: &Chunk) -> bool {
        let mut high_count = 0u32;
        let mut total = 0u32;
        for proto in &chunk.protos {
            for &insn in &proto.code {
                total += 1;
                if insn_op(insn) > 83 {
                    high_count += 1;
                }
            }
        }
        total > 0 && high_count * 100 / total > 20
    }

    /// Translate a *canonical* (upstream open-source Luau) opcode number into
    /// this decompiler's internal (Roblox-layout) opcode number, or 255 if the
    /// byte has no canonical Luau meaning in the range this build understands.
    ///
    /// The upstream `luau-compile` toolchain emits canonical Luau opcode
    /// numbers with NO per-client shuffle. That canonical numbering is identical
    /// to the Roblox layout for opcodes 0..=57 and 65..=75, but diverges in the
    /// 58..=64 and 76..=82 ranges (generic-for, native call, varargs, closure
    /// duplication, three-arg fastcall, jump-eq-constant and integer division).
    /// Each mapping below is by opcode *identity* (same instruction, different
    /// number) and was verified against bytecode produced by `luau-compile`.
    pub(crate) const fn canonical_luau_to_internal(op: u8) -> u8 {
        match op {
            // Identical numbering in both layouts.
            0..=57 => op,
            58 => 59,  // FORGLOOP        -> ForGLoop
            59 => 60,  // FORGPREP_INEXT  -> ForGPrepINext
            60 => 83,  // FASTCALL3       -> FastCall3
            61 => 62,  // FORGPREP_NEXT   -> ForGPrepNext
            62 => 63,  // NATIVECALL      -> NativeCall
            63 => 64,  // GETVARARGS      -> GetVarargs
            64 => 82,  // DUPCLOSURE      -> DupClosure
            // Identical numbering in both layouts.
            65..=75 => op,
            76 => 58,  // FORGPREP        -> ForGPrep
            77 => 78,  // JUMPXEQKNIL     -> JumpXEqKNil
            78 => 79,  // JUMPXEQKB       -> JumpXEqKB
            79 => 80,  // JUMPXEQKN       -> JumpXEqKN
            80 => 81,  // JUMPXEQKS       -> JumpXEqKS
            81 => 76,  // IDIV            -> IDiv
            82 => 77,  // IDIVK           -> IDivK
            _ => 255,  // 83+ has no canonical meaning this build decodes.
        }
    }

    /// Build an [`OpcodeMap`] that translates canonical (upstream) Luau opcode
    /// numbers into this decompiler's internal numbering. Used to decode
    /// standard Luau bytecode that carries no Roblox opcode shuffle. Evidence is
    /// set high for every mapped byte so `remap_chunk`'s AUX validation trusts
    /// the (structurally exact) translation rather than reverting it.
    pub fn canonical_luau() -> Self {
        let mut map = [255u8; 256];
        let mut evidence = [0u16; 256];
        let mut i = 0usize;
        while i < 256 {
            let internal = Self::canonical_luau_to_internal(i as u8);
            map[i] = internal;
            if internal != 255 {
                evidence[i] = 3;
            }
            i += 1;
        }
        let mapped = map.iter().filter(|&&v| v != 255).count();
        OpcodeMap {
            shuffled_to_standard: map,
            mapped_count: mapped,
            heuristic_map: map,
            heuristic_count: mapped,
            heuristic_evidence: evidence,
            // A canonical translation is exact by construction: nothing here was
            // completed speculatively, so every mapped byte is fully confident.
            pre_completion_map: map,
        }
    }

    /// Wrap a MEASURED permutation as an [`OpcodeMap`], with nothing invented.
    ///
    /// `pre_completion_map` is set to the same table, so `coverage` reports
    /// every mapping as evidence-backed and none as bijection filler. That is
    /// not flattery: the map came from aligning two compilations of known
    /// source, so each entry really was observed. Any byte the measurement did
    /// not pin stays unmapped and will surface as an unresolved instruction
    /// rather than being guessed.
    pub fn from_exact_map(map: [u8; 256]) -> Self {
        let mapped = map.iter().filter(|&&v| v != 255).count();
        let mut evidence = [0u16; 256];
        for (i, &v) in map.iter().enumerate() {
            if v != 255 {
                evidence[i] = 3;
            }
        }
        OpcodeMap {
            shuffled_to_standard: map,
            mapped_count: mapped,
            heuristic_map: map,
            heuristic_count: mapped,
            heuristic_evidence: evidence,
            pre_completion_map: map,
        }
    }

    /// Summarise how much of this chunk's decode rests on real evidence versus
    /// bijection filling. See `OpmapCoverage`.
    ///
    /// Must be called BEFORE `remap_chunk`, which rewrites the opcode bytes.
    pub fn coverage(&self, chunk: &Chunk) -> OpmapCoverage {
        let (freq, insn_words) = walk_instruction_positions(chunk, &self.shuffled_to_standard);
        let mut cov = OpmapCoverage { insn_words, ..OpmapCoverage::default() };
        for b in 0..256usize {
            if freq[b] == 0 {
                if self.shuffled_to_standard[b] != 255 {
                    cov.ghost_mappings += 1;
                }
                continue;
            }
            cov.present_bytes += 1;
            if self.pre_completion_map[b] != 255 {
                cov.present_confident += 1;
                cov.insn_words_confident += freq[b];
            } else if self.shuffled_to_standard[b] != 255 {
                cov.present_invented += 1;
                cov.insn_words_invented += freq[b];
            } else {
                cov.present_unmapped += 1;
            }
        }
        cov
    }

    /// Which shuffled bytes actually occur at a true instruction position.
    ///
    /// This is the honest denominator for any cross-file consensus: it lets a
    /// tally tell "this file never contained the byte" apart from "this file
    /// contained it and the detectors declined to call it". The first is an
    /// absence and must not count against the byte; the second is a genuine
    /// abstention. Confusing the two would suppress every rare opcode, which is
    /// most of the structurally important ones.
    ///
    /// Uses the same AUX-skipping walk as [`Self::coverage`], so the mask
    /// cannot count an AUX data word as an opcode occurrence. Must be called
    /// BEFORE `remap_chunk`, which rewrites the opcode bytes.
    pub fn present_byte_mask(&self, chunk: &Chunk) -> [bool; 256] {
        let (freq, _) = walk_instruction_positions(chunk, &self.shuffled_to_standard);
        let mut mask = [false; 256];
        for b in 0..256usize {
            mask[b] = freq[b] > 0;
        }
        mask
    }

    /// Detect whether `chunk` is *standard* (canonical) open-source Luau
    /// bytecode — e.g. produced by upstream `luau-compile` — as opposed to
    /// Roblox bytecode (which carries a per-client opcode shuffle this decoder
    /// must detect and undo).
    ///
    /// The check is a strict validity walk under the canonical Luau instruction
    /// format: every instruction word's opcode must be a known canonical opcode,
    /// and its AUX word(s) are skipped per the canonical layout. If every proto
    /// walks cleanly to exactly its code boundary using only known canonical
    /// opcodes, the chunk is canonical. Roblox-shuffled bytecode fails this walk
    /// because shuffled opcode bytes fall outside the canonical set or misalign
    /// the AUX skipping — so this only ever adds handling for bytecode the
    /// shuffle path (`needs_remapping`) already declines.
    pub fn is_canonical_luau(chunk: &Chunk) -> bool {
        if chunk.protos.is_empty() {
            return false;
        }
        for proto in &chunk.protos {
            let code = &proto.code;
            let mut i = 0;
            while i < code.len() {
                let internal = Self::canonical_luau_to_internal(insn_op(code[i]));
                if internal == 255 {
                    return false; // not a canonical Luau opcode
                }
                if LuauOpcode::from_u8(internal).has_aux() {
                    if i + 2 > code.len() {
                        return false; // AUX word would run past the code stream
                    }
                    i += 2;
                } else {
                    i += 1;
                }
            }
        }
        true
    }

    /// Detect the opcode shuffle table from bytecode patterns
    pub fn detect(chunk: &Chunk) -> Self {
        Self::detect_with_prior(chunk, &[255u8; 256])
    }

    /// Try to decode `chunk` under `map` and report whether it holds together.
    ///
    /// This is the same strict validity walk [`Self::is_canonical_luau`]
    /// performs against the canonical table, generalised to any map. It is a
    /// surprisingly sharp instrument: a wrong permutation will usually either
    /// hit a byte it has no mapping for, or mis-skip an AUX word and step past
    /// the end of a prototype. Both are caught here.
    ///
    /// Also returns which bytes occur at a TRUE instruction position under this
    /// map, which is the honest denominator for "how much of this chunk does
    /// the map actually have to explain".
    pub fn walk_verify(chunk: &Chunk, map: &[u8; 256]) -> WalkReport {
        let mut present = [false; 256];
        let mut insn_words = 0u32;
        if chunk.protos.is_empty() {
            return WalkReport {
                verdict: WalkVerdict::Clean,
                present,
                insn_words,
            };
        }
        for (pi, proto) in chunk.protos.iter().enumerate() {
            let code = &proto.code;
            let mut i = 0usize;
            while i < code.len() {
                let byte = insn_op(code[i]);
                let mapped = map[byte as usize];
                if mapped == 255 || (mapped as usize) >= LuauOpcode::MAX_OPCODE {
                    return WalkReport {
                        verdict: WalkVerdict::UnmappedByte {
                            byte,
                            proto: pi,
                            pc: i,
                        },
                        present,
                        insn_words,
                    };
                }
                present[byte as usize] = true;
                insn_words += 1;
                if LuauOpcode::from_u8(mapped).has_aux() {
                    if i + 2 > code.len() {
                        return WalkReport {
                            verdict: WalkVerdict::OverranProto { proto: pi },
                            present,
                            insn_words,
                        };
                    }
                    i += 2;
                } else {
                    i += 1;
                }
            }
        }
        WalkReport {
            verdict: WalkVerdict::Clean,
            present,
            insn_words,
        }
    }

    /// Detect the opcode shuffle table from bytecode patterns, pre-seeded with
    /// a prior mapping (e.g., a cached opmap from other scripts of the same build).
    ///
    /// The prior provides high-confidence consensus from prior scripts. Per-script
    /// detection can then build on top of those assignments without re-deriving them,
    /// and — crucially — without being fooled into stealing pre-mapped bytes for
    /// other opcodes. This is important for small scripts where per-file detection
    /// lacks enough signal (e.g., a script with no NEWTABLE uses).
    pub fn detect_with_prior(chunk: &Chunk, prior: &[u8; 256]) -> Self {
        Self::detect_with_prior_and_truth(chunk, prior, get_ground_truth())
    }

    /// Detect using ONLY the chunk's own structure — no installed ground truth,
    /// no prior.
    ///
    /// Needed because a reading used to *identify* which permutation a chunk
    /// belongs to must be independent of any permutation already installed.
    /// Feeding an installed map back into the reading that selects it would
    /// make the selection self-confirming: whatever was installed first would
    /// look like the right answer for every subsequent chunk.
    pub fn detect_structural(chunk: &Chunk) -> Self {
        Self::detect_with_prior_and_truth(chunk, &[255u8; 256], None)
    }

    fn detect_with_prior_and_truth(
        chunk: &Chunk,
        prior: &[u8; 256],
        ground_truth: Option<[u8; 256]>,
    ) -> Self {
        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(chunk);

        // Apply any installed ground truth on top of the caller's prior.
        // Ground truth comes from a direct observation of the client's own
        // compiler (see `parser::alignment`), so it strictly overrides both the
        // heuristic detectors and cached consensus. See `set_ground_truth`
        // above for the install path.
        let effective_prior: [u8; 256] = if let Some(gt) = ground_truth {
            merge_ground_truth_into_prior(prior, &gt)
        } else {
            *prior
        };

        // Seed the context with the prior map. Prior entries are treated as
        // already-assigned AND LOCKED: validators must not unassign them, and
        // subsequent detectors must not reassign them. Cache consensus is more
        // authoritative than any single-file heuristic.
        // NOTE: upper bound is MAX_OPCODE (not 91) to include Roblox-specific extensions 92-95.
        for (shuffled, &standard) in effective_prior.iter().enumerate() {
            if standard != 255 && (standard as usize) < LuauOpcode::MAX_OPCODE {
                ctx.map[shuffled] = standard;
                ctx.assigned[standard as usize] = true;
                ctx.evidence[shuffled] = 3;
                ctx.locked[shuffled] = true;
            }
        }

        // TIER 1: Structural constraints (near 100% reliable)
        detect_return(chunk, &mut ctx);
        detect_prepvarargs(chunk, &mut ctx);

        // TIER 2: AUX/pattern matching
        detect_getimport(chunk, &mut ctx);
        detect_closure_capture(chunk, &mut ctx);
        detect_dupclosure(chunk, &mut ctx);
        detect_duptable(chunk, &mut ctx);
        // NEWTABLE + SETLIST as a joint pair. Runs AFTER detect_duptable so that
        // DUPTABLE's byte is already claimed and cannot be mistaken for the
        // creator (a DUPTABLE with a small template-constant index has the same
        // B<=15, C==0 shape). Runs this early because the true NEWTABLE and
        // SETLIST bytes are otherwise consumed by detect_table_ops,
        // detect_comparison_jumps_aux, detect_global_ops and detect_loadkx long
        // before detect_newtable/detect_setlist are reached.
        detect_newtable_setlist_pair(chunk, &mut ctx);
        // Generic-for BEFORE numeric-for: FORGLOOP has a distinctive AUX word
        // (count | (is_ipairs << 31)) that FORNLOOP lacks. By running generic-for
        // first, we detect FORGLOOP's shuffled byte so numeric-for can then use
        // a direct "target != FORGLOOP" exclusion (stronger than AUX heuristics).
        detect_generic_for(chunk, &mut ctx);
        // FORGPREP variants need FORGLOOP to be detected first.
        // Run them BEFORE numeric-for so FORGPREP claims its byte before any other
        // AD-format detector (numeric_for, conditional_jumps, jump, etc.) can steal it.
        // FORGPREP has very strong structural evidence (forward jump to a known FORGLOOP
        // with matching A register).
        detect_forgprep_variants(chunk, &mut ctx);
        // FORGLOOP_INEXT (canonical 61) and FORGPREP_INEXT (60) are detected jointly:
        // detect_forgprep_inext_pair finds both via structural pair matching (no prior
        // knowledge of either required). detect_forgloopinext then confirms/extends if
        // ForGPrepINext is already assigned (e.g. from the augmenter).
        detect_forgprep_inext_pair(chunk, &mut ctx);
        detect_forgloopinext(chunk, &mut ctx);
        detect_numeric_for(chunk, &mut ctx);

        // TIER 3: Frequency + operand analysis
        // GETTABLEN's sequential-read run first: in table-heavy chunks the real
        // GETTABLEN out-counts the real CALL and matches CALL's operand shape, so
        // detect_call force-assigns it and burns two slots. The run signature is
        // exact (0 false positives measured across 5 permutations of the corpus),
        // so claiming it up front also unpoisons detect_call's candidate list.
        detect_gettablen_read_run(chunk, &mut ctx);
        detect_call(chunk, &mut ctx);
        detect_namecall(chunk, &mut ctx);
        detect_loadk(chunk, &mut ctx);
        // MOVE early: its "all C=0" invariant is very strong, and mapping it early
        // prevents GETUPVAL/SETUPVAL/unary detectors from stealing its shuffled byte.
        // It also lets detect_upvalue_ops and detect_unary_* exclude MOVE's byte.
        detect_move(chunk, &mut ctx);
        detect_jump(chunk, &mut ctx);
        detect_table_ops(chunk, &mut ctx);
        detect_conditional_jumps(chunk, &mut ctx);
        detect_upvalue_ops(chunk, &mut ctx);

        // TIER 4: Pattern-based detection for remaining opcodes
        // CRITICAL ORDERING: comparison jumps (JUMPIFEQ etc.) must run BEFORE
        // detect_newtable. JUMPIFEQ is an AD-format instruction with AUX word;
        // when interpreted as ABC, its short forward-jump D creates a fake
        // "NEWTABLE candidate" pattern (c=0, small a, plausible b). By mapping
        // JUMPIFEQ first, its shuffled byte becomes ineligible for NEWTABLE,
        // letting the real NEWTABLE byte rise to the top of the candidate list.
        //
        // The MODK/JUMPXEQKN parity pair goes ahead of both: its evidence is a
        // value-range invariant rather than a shape, and in the parity-test files
        // detect_comparison_jumps_aux otherwise claims the JUMPXEQKN byte as a
        // plain conditional jump, after which neither half is recoverable.
        detect_modk_parity_pair(chunk, &mut ctx);
        detect_comparison_jumps_aux(chunk, &mut ctx);
        detect_jumpxeq(chunk, &mut ctx);
        detect_jumpback(chunk, &mut ctx);
        // CLOSEUPVALS needs its scope terminators (RETURN / JUMPBACK / FORNLOOP /
        // FORGLOOP) mapped, which is true from here on, and must run before the
        // C==0 pipeline (LOADB/LOADN/LOADNIL/MOVE/unary/GETVARARGS) starts taking
        // bytes out of the shared degenerate pool.
        detect_closeupvals_ref_scope(chunk, &mut ctx);
        // Purity-gated JUMP fallback for chunks where detect_jump's count-based
        // path stayed silent. Must run before detect_gettable_settable and
        // detect_unary_not_minus, which are the measured thieves of JUMP's byte.
        detect_jump_unconditional_forward(chunk, &mut ctx);

        // NEWTABLE BEFORE GETGLOBAL: NEWTABLE's AUX (array-size hint, usually small)
        // can incidentally point to a valid K[0] String ("game"), fooling the
        // GETGLOBAL detector. Run NEWTABLE first (it has a stronger cross-check:
        // "followed by SETTABLEKS/SETLIST that uses R(A) as target table").
        detect_newtable(chunk, &mut ctx);
        detect_global_ops(chunk, &mut ctx);
        // FASTCALL family: detect base FASTCALL first (B=0 is a hard constraint),
        // then FASTCALL1 (B=register), then FASTCALL2 (has AUX with register).
        // This ordering prevents FASTCALL1's looser pattern from stealing FASTCALL's byte.
        detect_fastcall(chunk, &mut ctx);
        detect_fastcall1(chunk, &mut ctx);
        detect_fastcall2(chunk, &mut ctx);
        detect_fastcall2k(chunk, &mut ctx);
        detect_setlist(chunk, &mut ctx);
        detect_gettablen_settablen(chunk, &mut ctx);
        detect_gettable_settable(chunk, &mut ctx);
        // Re-run FORGPREP variants in case new FORGLOOPs were detected later
        detect_forgprep_variants(chunk, &mut ctx);
        // Re-run pair detector + FORGLOOPINEXT now that other opcodes are firmly assigned
        detect_forgprep_inext_pair(chunk, &mut ctx); // Phase B0.29: pair detector
        detect_forgloopinext(chunk, &mut ctx);

        // TIER 5: Format-based detection (needs most others mapped first)
        detect_loadb(chunk, &mut ctx);
        detect_loadn(chunk, &mut ctx);
        detect_loadnil(chunk, &mut ctx);
        detect_move(chunk, &mut ctx);
        // Sequence-based arith detection (finds low-frequency arith ops via
        // monotonic-A ladder pattern). Must run BEFORE detect_arithmetic which
        // uses frequency-based detection that fails for single-occurrence ops.
        // The same-register ladder goes first: it covers the compound-assignment
        // shape (A == B throughout) that the monotonic-A test structurally cannot
        // see, and it must beat the frequency detectors for the same reason.
        detect_same_register_arith_k_ladder(chunk, &mut ctx);
        detect_arith_sequence(chunk, &mut ctx);
        detect_arithmetic(chunk, &mut ctx);
        detect_arithmetic_k(chunk, &mut ctx);
        detect_register_arithmetic(chunk, &mut ctx);
        detect_unary_not_minus(chunk, &mut ctx);
        detect_unary_ops(chunk, &mut ctx);
        // Phase B0.44B: single-hit JumpXEqKB/JumpXEqKNil detector via
        // return-target structural validation (handles ModuleScript.luac
        // M.xeq_constants where `if v == true/nil then return ... end`
        // appears exactly once per constant type).
        detect_xeq_single_hit_return_target(chunk, &mut ctx);
        detect_concat(chunk, &mut ctx);
        detect_getvarargs(chunk, &mut ctx);
        detect_closeupvals(chunk, &mut ctx);
        detect_and_or(chunk, &mut ctx);

        // TIER 6: Speculative detectors (only run if prerequisites are met)
        // These are more aggressive and could cause false positives, so they
        // run last and have higher thresholds.
        // Re-run FASTCALL family in case earlier tiers freed up candidates
        detect_fastcall(chunk, &mut ctx);
        detect_fastcall1(chunk, &mut ctx);
        detect_fastcall2(chunk, &mut ctx);
        detect_fastcall2k(chunk, &mut ctx);
        detect_fastcall3(chunk, &mut ctx);
        detect_bitwise_ops(chunk, &mut ctx);
        detect_idiv(chunk, &mut ctx);
        detect_idivk(chunk, &mut ctx);
        detect_subrk_divrk(chunk, &mut ctx);
        detect_loadkx(chunk, &mut ctx);
        detect_elimination_pass(chunk, &mut ctx);

        // POST-DETECTION VALIDATION: unassign implausible frequency mismatches
        validate_frequency_plausibility(chunk, &mut ctx);

        // AUX ALIGNMENT VALIDATION: verify that AUX-using opcodes have valid AUX data.
        // If an opcode is mapped to an AUX-using standard opcode, but its "AUX" word
        // frequently contains what looks like a valid instruction (mapped opcode byte),
        // the mapping is probably wrong.
        validate_aux_alignment(chunk, &mut ctx);

        // SECOND PASS: re-run critical detectors for any core opcodes that weren't found.
        // After validation removed false positives, previously-stolen shuffled bytes
        // are now available for correct mapping.
        if ctx.find_shuffled(LuauOpcode::Return as u8).is_none() {
            detect_return(chunk, &mut ctx);
        }
        if ctx.find_shuffled(LuauOpcode::Call as u8).is_none() {
            detect_call(chunk, &mut ctx);
        }
        if ctx.find_shuffled(LuauOpcode::NameCall as u8).is_none() {
            detect_namecall(chunk, &mut ctx);
        }
        if ctx.find_shuffled(LuauOpcode::Move as u8).is_none() {
            detect_move(chunk, &mut ctx);
        }
        if ctx.find_shuffled(LuauOpcode::GetTableKS as u8).is_none() || ctx.find_shuffled(LuauOpcode::SetTableKS as u8).is_none() {
            detect_table_ops(chunk, &mut ctx);
        }
        if ctx.find_shuffled(LuauOpcode::GetUpval as u8).is_none() {
            detect_upvalue_ops(chunk, &mut ctx);
        }
        if ctx.find_shuffled(LuauOpcode::Jump as u8).is_none() {
            detect_jump(chunk, &mut ctx);
        }
        if ctx.find_shuffled(LuauOpcode::JumpIfNot as u8).is_none() {
            detect_conditional_jumps(chunk, &mut ctx);
        }
        if ctx.find_shuffled(LuauOpcode::GetImport as u8).is_none() {
            detect_getimport(chunk, &mut ctx);
        }
        if ctx.find_shuffled(LuauOpcode::NewClosure as u8).is_none() {
            detect_closure_capture(chunk, &mut ctx);
        }
        if ctx.find_shuffled(LuauOpcode::LoadK as u8).is_none() {
            detect_loadk(chunk, &mut ctx);
        }
        // Re-run dependent detectors that need CALL
        if ctx.find_shuffled(LuauOpcode::Call as u8).is_some() {
            if ctx.find_shuffled(LuauOpcode::NameCall as u8).is_none() {
                detect_namecall(chunk, &mut ctx);
            }
            // Re-run all FASTCALL variants in proper order
            if ctx.find_shuffled(LuauOpcode::FastCall as u8).is_none() {
                detect_fastcall(chunk, &mut ctx);
            }
            if ctx.find_shuffled(LuauOpcode::FastCall1 as u8).is_none() {
                detect_fastcall1(chunk, &mut ctx);
            }
            if ctx.find_shuffled(LuauOpcode::FastCall2 as u8).is_none() {
                detect_fastcall2(chunk, &mut ctx);
            }
            if ctx.find_shuffled(LuauOpcode::FastCall2K as u8).is_none() {
                detect_fastcall2k(chunk, &mut ctx);
            }
            if ctx.find_shuffled(LuauOpcode::FastCall3 as u8).is_none() {
                detect_fastcall3(chunk, &mut ctx);
            }
        }

        // THIRD PASS: re-run ALL detectors one more time for maximum coverage
        // This catches cases where removing false positives freed up shuffled bytes
        // that can now be correctly detected
        detect_return(chunk, &mut ctx);
        detect_prepvarargs(chunk, &mut ctx);
        detect_getimport(chunk, &mut ctx);
        detect_closure_capture(chunk, &mut ctx);
        detect_dupclosure(chunk, &mut ctx);
        detect_duptable(chunk, &mut ctx);
        // Generic-for before numeric-for (see Tier 2 comment)
        detect_generic_for(chunk, &mut ctx);
        detect_forgprep_variants(chunk, &mut ctx);
        detect_forgprep_inext_pair(chunk, &mut ctx); // Phase B0.29: pair detector
        detect_forgloopinext(chunk, &mut ctx); // Phase B0.14: added to 3rd pass
        detect_numeric_for(chunk, &mut ctx);
        // Same ordering rationale as Tier 3: claim the run signature before CALL.
        detect_gettablen_read_run(chunk, &mut ctx);
        detect_call(chunk, &mut ctx);
        detect_namecall(chunk, &mut ctx);
        detect_loadk(chunk, &mut ctx);
        detect_jump(chunk, &mut ctx);
        detect_table_ops(chunk, &mut ctx);
        detect_conditional_jumps(chunk, &mut ctx);
        detect_upvalue_ops(chunk, &mut ctx);
        // NEWTABLE before GETGLOBAL (same rationale as Tier 4)
        detect_newtable(chunk, &mut ctx);
        detect_global_ops(chunk, &mut ctx);
        detect_fastcall(chunk, &mut ctx);
        detect_fastcall1(chunk, &mut ctx);
        detect_fastcall2(chunk, &mut ctx);
        detect_fastcall2k(chunk, &mut ctx);
        detect_setlist(chunk, &mut ctx);
        detect_gettablen_settablen(chunk, &mut ctx);
        detect_gettable_settable(chunk, &mut ctx);
        detect_comparison_jumps_aux(chunk, &mut ctx);
        detect_jumpxeq(chunk, &mut ctx);
        detect_jumpback(chunk, &mut ctx);
        detect_forgprep_variants(chunk, &mut ctx);
        detect_loadb(chunk, &mut ctx);
        detect_loadn(chunk, &mut ctx);
        detect_loadnil(chunk, &mut ctx);
        detect_move(chunk, &mut ctx);
        // Same ordering rationale as Tier 5.
        detect_same_register_arith_k_ladder(chunk, &mut ctx);
        detect_arith_sequence(chunk, &mut ctx);
        detect_arithmetic(chunk, &mut ctx);
        detect_arithmetic_k(chunk, &mut ctx);
        detect_register_arithmetic(chunk, &mut ctx);
        detect_unary_not_minus(chunk, &mut ctx);
        detect_unary_ops(chunk, &mut ctx);
        detect_xeq_single_hit_return_target(chunk, &mut ctx);
        detect_concat(chunk, &mut ctx);
        detect_getvarargs(chunk, &mut ctx);
        detect_closeupvals(chunk, &mut ctx);
        detect_and_or(chunk, &mut ctx);
        detect_fastcall(chunk, &mut ctx);
        detect_fastcall1(chunk, &mut ctx);
        detect_fastcall2(chunk, &mut ctx);
        detect_fastcall2k(chunk, &mut ctx);
        detect_fastcall3(chunk, &mut ctx);
        detect_bitwise_ops(chunk, &mut ctx);
        detect_idiv(chunk, &mut ctx);
        detect_idivk(chunk, &mut ctx);
        detect_subrk_divrk(chunk, &mut ctx);
        detect_loadkx(chunk, &mut ctx);

        // TIER 7: Frequency-rank matching for remaining unknowns
        // This is the final pass — uses statistical frequency matching
        detect_frequency_rank_matching(chunk, &mut ctx);

        // TIER 8: Known shuffle augmentation — fill gaps from hardcoded maps
        // extracted from real Roblox bytecode. This catches opcodes that the
        // heuristic detectors missed by matching against known shuffle variants.
        //
        // Guard: `find_best_known_shuffle` accepts up to 5 conflicts between the
        // current heuristic and the known variant ("close enough"), but for
        // STRUCTURAL-REQUIRED opcodes a close-enough match is not good enough —
        // the wrong NewTable / ForGLoop byte poisons the cache. Revert any
        // structural-required assignments that the augmenter introduced.
        if let Some(augmented) = super::known_shuffles::find_best_known_shuffle(&ctx.map) {
            let old_map = ctx.map;
            ctx.map = augmented;
            for s in 0..256usize {
                let std_op = ctx.map[s];
                if std_op != 255
                    && old_map[s] != std_op
                    && (std_op as usize) < LuauOpcode::MAX_OPCODE
                    && (DetectCtx::is_structural_required_standard_opcode(std_op)
                        || !DetectCtx::opcode_can_appear_in_chunk(chunk, std_op))
                {
                    // Phase A Patch 2: unanimity override.
                    // The revert default protects against close-enough variant
                    // matches poisoning structural-required opcodes. But if
                    // EVERY known variant that contains `std_op` maps it to the
                    // same shuffled byte AND the augmenter picked exactly that
                    // byte, the multi-variant consensus is stronger evidence
                    // than any single-script heuristic could produce. In that
                    // case the augmenter's assignment is as trustworthy as a
                    // detector hit — keep it.
                    //
                    // Scope: `is_structural_required_standard_opcode` only.
                    // For the `opcode_can_appear_in_chunk == false` branch,
                    // unanimity can't rescue us — if the opcode can't appear
                    // in THIS chunk, any byte it gets assigned is speculative.
                    if DetectCtx::is_structural_required_standard_opcode(std_op)
                        && DetectCtx::opcode_can_appear_in_chunk(chunk, std_op)
                    {
                        if let Some(unanimous_byte) =
                            super::known_shuffles::all_variants_that_map(std_op)
                        {
                            if unanimous_byte as usize == s {
                                // Keep the assignment — unanimous multi-variant evidence.
                                continue;
                            }
                        }

                        // Phase B0.19: Format-consistency override for unary ops.
                        //
                        // Not (50), Minus (51), and Length (52) share the unary ABC format:
                        // C=0, A!=B, A<stack, B<stack. No other standard opcode uses
                        // exclusively this exact pattern (the for-loop opcodes use AD format;
                        // Move/GetUpval/SetUpval use similar format but have distinct
                        // structural markers that their dedicated detectors already map).
                        //
                        // When the augmenter proposes one of these for a shuffled byte,
                        // unanimity is impossible (variants disagree because each shuffle
                        // variant picks different shuffled bytes for the same opcode).
                        // But if EVERY occurrence of the proposed shuffled byte in the
                        // entire chunk has C=0, A!=B, A<stack, B<stack, the format
                        // evidence is strong enough to accept the augmenter's proposal
                        // without a unanimous cross-variant agreement.
                        //
                        // Safety gate: total occurrence count must be within the unary
                        // frequency cap (total_insns/20 at most — unary ops are rare).
                        // If the byte appears too often it is more likely a structural
                        // opcode (Move, GetUpval, etc.) that passed format-screening
                        // coincidentally.
                        let is_unary_op = matches!(
                            LuauOpcode::from_u8(std_op),
                            LuauOpcode::Not | LuauOpcode::Minus | LuauOpcode::Length
                        );
                        if is_unary_op {
                            let max_unary_freq = if ctx.total_insns > 100 {
                                ctx.total_insns / 20
                            } else {
                                50u32
                            };
                            let shuffled_byte = s as u8;
                            let mut total_count = 0u32;
                            let mut format_ok_count = 0u32;
                            'format_scan: for proto in &chunk.protos {
                                for &insn in &proto.code {
                                    if insn_op(insn) != shuffled_byte { continue; }
                                    total_count += 1;
                                    if total_count > max_unary_freq {
                                        // Too frequent — abort scan, do not keep.
                                        break 'format_scan;
                                    }
                                    let a = insn_a(insn);
                                    let b = insn_b(insn);
                                    let c = insn_c(insn);
                                    if c == 0 && a != b
                                        && a < proto.max_stack_size
                                        && b < proto.max_stack_size
                                    {
                                        format_ok_count += 1;
                                    }
                                }
                            }
                            // Keep if: at least 1 occurrence, ALL occurrences are format-ok,
                            // and not over-frequent.
                            if total_count >= 1
                                && total_count <= max_unary_freq
                                && format_ok_count == total_count
                            {
                                // Keep the augmenter's assignment — every instance of
                                // this byte in the chunk has the unary ABC format.
                                continue;
                            }
                        }
                    }
                    // Revert: leave unmapped rather than trust a close-enough variant
                    // for (a) structural-required opcodes (catastrophic cache poison
                    // risk), or (b) opcodes that can't structurally appear in this
                    // chunk (e.g. LoadKX with no >32768-constant proto).
                    ctx.map[s] = 255;
                    ctx.assigned[std_op as usize] = false;
                }
            }
            // Phase B0.14 fix: resync ctx.assigned from ctx.map after the augmenter
            // replaces the full map. The augmenter does ctx.map = augmented (bulk replace),
            // which silently erases heuristic entries where augmented[s]=255. Example:
            //   - detect_jump correctly assigns 0xC1→67 (JumpX) → ctx.assigned[67]=true
            //   - augmenter: ctx.map = augmented → augmented[0xC1]=255 → ctx.map[0xC1]=255
            //   - ctx.assigned[67] stays true (stale), but ctx.map has no JumpX entry
            //   - Post-augmenter detect_jump calls try_assign_force(0xC1, 67):
            //     ctx.assigned[67]==true → "already assigned" → fails silently
            // Fix: rebuild ctx.assigned from ctx.map so it accurately reflects the
            // current state. Post-augmenter detect calls can then re-assign erased entries.
            ctx.assigned = [false; 256];
            for &v in ctx.map.iter() {
                if v != 255 && (v as usize) < 256 {
                    ctx.assigned[v as usize] = true;
                }
            }
        }

        // Post-augmenter re-run of detectors that depend on augmenter-provided mappings.
        //
        // detect_forgloopinext: requires ForGPrepINext (canonical 60) to be in ctx before
        // it can find ForGLoopINext (canonical 61). In the S2 shuffle, ForGPrepINext is
        // only discovered by the Tier 8 augmenter (0x64→60 comes from the known variant),
        // because detect_forgprep_variants requires targets to be generic ForGLoop (59) —
        // which ForGPrepINext does NOT use (its target is ForGLoopINext, not ForGLoop).
        // Running detect_forgloopinext here gives it access to the augmenter's ForGPrepINext.
        //
        // detect_jump (JumpX): if detect_jump correctly assigned JumpX before the augmenter,
        // the augmenter's bulk ctx.map replacement erased it (augmented[JumpX_byte]=255).
        // The ctx.assigned resync above cleared ctx.assigned[67], so detect_jump can
        // now re-detect and re-assign JumpX correctly.
        detect_forgprep_inext_pair(chunk, &mut ctx); // Phase B0.29: pair detector
        detect_forgloopinext(chunk, &mut ctx);
        detect_jump(chunk, &mut ctx);
        // Phase B0.34: re-run detect_jumpback post-augmenter.
        //
        // Scenario: augmenter's known-shuffle variant 0 maps 0x6E→JUMPBACK (stale,
        // from pre-B0.33 cache). Since JUMPBACK was added to structural-required,
        // the revert loop above resets ctx.map[0x6E]=255 and ctx.assigned[24]=false.
        // This re-run then lets detect_jumpback assign the correct byte (0x48 in
        // v0-shaped scripts) using the B0.33 FORGLOOP-aware shape filter.
        detect_jumpback(chunk, &mut ctx);
        // Phase B0.78: Post-augmenter LENGTH rescue. If LENGTH is still unmapped
        // after all tiers + augmenter, try a rescue with broader consumer window.
        // LENGTH is a cascade blocker: RBX_EXT requires it as prerequisite, so a
        // missing LENGTH blocks ~14 RBX_EXT opcodes.
        if ctx.find_shuffled(LuauOpcode::Length as u8).is_none() {
            detect_length_rescue(chunk, &mut ctx);
        }
        // Post-augmenter: detect Roblox-specific extensions beyond canonical 91.
        // Must run after the augmenter so the RbxExt assignments don't corrupt
        // the variant fingerprint used by find_best_known_shuffle.
        detect_rbx_ext_ops(chunk, &mut ctx);

        // TIER 8.5: Infer remaining opcodes from instruction-position analysis.
        // Walk the instruction stream using known AUX info to find shuffled bytes
        // that appear at true instruction positions but aren't yet mapped.
        // This is much more reliable than frequency-based guessing because it
        // filters out AUX data words that pollute the frequency counts.
        infer_from_instruction_positions(chunk, &mut ctx);

        // Snapshot the map BEFORE speculative completion — this is safe to cache.
        let heuristic_map = ctx.map;
        let heuristic_count = ctx.map.iter().filter(|&&v| v != 255).count();
        let heuristic_evidence = ctx.evidence;

        // TIER 9: Permutation completion — use the bijection constraint to fill
        // ALL remaining gaps. Since the shuffle is a permutation of opcodes 0-83,
        // each real opcode maps to exactly one shuffled byte and vice versa.
        permutation_complete(chunk, &mut ctx);

        let mapped_count = ctx.map.iter().filter(|&&v| v != 255).count();
        OpcodeMap {
            shuffled_to_standard: ctx.map,
            mapped_count,
            heuristic_map,
            heuristic_count,
            heuristic_evidence,
            pre_completion_map: heuristic_map,
        }
    }

    // NOTE: permutation_complete_map is defined at the top of the impl block (line 132)

    /// Apply the detected mapping to remap all instructions in-place.
    /// CRITICAL: Only remap instruction words, NOT AUX data words.
    /// AUX words contain constant indices, import IDs, register refs, etc.
    /// Remapping their low byte corrupts the data.
    /// Returns the count of unknown (unmapped) instruction words encountered.
    pub fn remap_chunk(&self, chunk: &mut Chunk) -> (usize, [u32; 256], [Option<u32>; 256]) {
        let mut unknown_insn_count = 0usize;
        let mut unknown_byte_freq = [0u32; 256];
        // Sample instructions for unresolved bytes: raw instruction word (low byte = orig opcode)
        let mut unknown_byte_sample: [Option<u32>; 256] = [None; 256];
        for proto in &mut chunk.protos {
            let mut i = 0;
            while i < proto.code.len() {
                let insn = proto.code[i];
                let op = insn_op(insn);
                let new_op = self.shuffled_to_standard[op as usize];
                if new_op != 255 {
                    // Remap this instruction's opcode byte
                    proto.code[i] = (insn & 0xFFFFFF00) | (new_op as u32);
                    // Skip AUX word if the standard opcode has one
                    let standard_op = super::opcodes::LuauOpcode::from_u8(new_op);
                    if standard_op.has_aux() && i + 1 < proto.code.len() {
                        let aux = proto.code[i + 1];
                        // Validate AUX for string-keyed opcodes: the AUX should be
                        // a valid index into proto.constants pointing to a String.
                        let needs_string_aux = matches!(
                            standard_op,
                            super::opcodes::LuauOpcode::GetGlobal
                            | super::opcodes::LuauOpcode::SetGlobal
                            | super::opcodes::LuauOpcode::GetTableKS
                            | super::opcodes::LuauOpcode::SetTableKS
                            | super::opcodes::LuauOpcode::NameCall
                        );
                        if needs_string_aux {
                            // In the Luau VM, GETGLOBAL/SETGLOBAL/GETTABLEKS/
                            // SETTABLEKS/NAMECALL all use AUX as a 0-based index
                            // into proto.constants, and that constant should be a
                            // String.
                            //
                            // IMPORTANT: We no longer revert to Unknown on AUX
                            // validation failure. Reverting + stepping by 1 caused
                            // catastrophic misalignment cascades: the AUX word was
                            // re-examined as an instruction, shifting ALL subsequent
                            // instructions by one word. The lifter handles unresolved
                            // AUX gracefully with field_N/global_N fallbacks, which
                            // is far better than losing the instruction entirely.
                            //
                            // We still validate to detect clearly wrong mappings
                            // (using heuristic evidence), but always step by 2.
                            let valid = (aux as usize) < proto.constants.len()
                                && matches!(
                                    proto.constants.get(aux as usize),
                                    Some(crate::parser::types::Constant::String(_))
                                );
                            if !valid {
                                // Check chunk-level strings as fallback before
                                // considering this a bad mapping.
                                let chunk_valid = (aux as usize) < chunk.strings.len()
                                    || (aux > 0 && ((aux - 1) as usize) < chunk.strings.len());

                                // Extra check: if the AUX is WAY out of bounds
                                // (much larger than the constant table) AND the AUX
                                // word's low byte maps to a known shuffled opcode,
                                // the "AUX" is likely a real instruction that got
                                // consumed due to alignment error. This happens when
                                // a preceding unknown opcode's AUX data was treated
                                // as an instruction, shifting the stream by 1 word.
                                // Revert regardless of heuristic evidence.
                                let aux_wildly_out_of_range =
                                    (aux as usize) > proto.constants.len().saturating_mul(2).max(100);
                                let aux_low = (aux & 0xFF) as u8;
                                let aux_looks_like_instruction =
                                    self.shuffled_to_standard[aux_low as usize] != 255;

                                if !chunk_valid && (self.heuristic_evidence[new_op as usize] < 2
                                    || (aux_wildly_out_of_range && aux_looks_like_instruction)) {
                                    // AUX doesn't resolve and either:
                                    // (a) Low evidence — mapping is likely wrong, OR
                                    // (b) AUX looks like an instruction word — the
                                    //     alignment is shifted and this instruction
                                    //     was created from AUX data of a previous
                                    //     unknown opcode.
                                    // Revert but STILL step by 2 to avoid cascade.
                                    proto.code[i] = (insn & 0xFFFFFF00) | 255;
                                    unknown_insn_count += 1;
                                }
                                // else: keep the mapping, lifter will use fallback names
                            }
                            i += 2;
                        } else {
                            i += 2; // Non-string AUX (GETIMPORT, NEWTABLE, etc.) — trust it
                        }
                    } else if standard_op.has_aux() {
                        i += 2; // At end of code, just skip
                    } else {
                        i += 1;
                    }
                } else {
                    // Unknown opcode — CRITICAL: set opcode byte to 255 so the lifter
                    // sees it as Unknown rather than misinterpreting the original shuffled
                    // byte as a standard opcode (e.g., shuffled 0x36 = decimal 54 = DUPTABLE).
                    unknown_byte_freq[op as usize] += 1;
                    if unknown_byte_sample[op as usize].is_none() {
                        unknown_byte_sample[op as usize] = Some(insn);
                    }

                    proto.code[i] = (insn & 0xFFFFFF00) | 255;
                    unknown_insn_count += 1;

                    // Can't determine if this unknown opcode has AUX.
                    // Use heuristic: check if the next word looks like AUX data
                    // vs a real instruction to avoid misalignment cascades.
                    if i + 1 < proto.code.len() {
                        let next_word = proto.code[i + 1];
                        let next_low = (next_word & 0xFF) as u8;
                        let next_std = self.shuffled_to_standard[next_low as usize];
                        let next_maps_known = next_std != 255;

                        // Check if next word looks like it could be AUX data:
                        // - Small value (constant index, register count, loop var count)
                        // - Import ID pattern (bits 30-31 set, used by GETIMPORT AUX)
                        // - Low byte fits within register range of this proto
                        let looks_like_aux = (next_word < 10000)
                            || (next_word >> 30) >= 1
                            || ((next_word & 0xFF) as u8) < proto.max_stack_size;

                        if next_maps_known && i + 2 < proto.code.len() {
                            let next_op = super::opcodes::LuauOpcode::from_u8(next_std);
                            let word_after = proto.code[i + 2];
                            let after_low = (word_after & 0xFF) as u8;
                            let after_maps_known = self.shuffled_to_standard[after_low as usize] != 255;

                            // Extra validation: if the next word maps to a string-keyed
                            // opcode (GETTABLEKS, SETTABLEKS, NAMECALL, GETGLOBAL, SETGLOBAL),
                            // the word after should be a valid string constant index (AUX).
                            // If it's clearly out of bounds, the "next instruction" is
                            // actually AUX data whose low byte coincidentally matches
                            // a known opcode. Step by 2 in that case.
                            let next_needs_string_aux = matches!(
                                next_op,
                                super::opcodes::LuauOpcode::GetTableKS
                                | super::opcodes::LuauOpcode::SetTableKS
                                | super::opcodes::LuauOpcode::NameCall
                                | super::opcodes::LuauOpcode::GetGlobal
                                | super::opcodes::LuauOpcode::SetGlobal
                            );
                            if next_needs_string_aux {
                                // The word_after would be the AUX of this fake instruction.
                                // Check if it's a valid string constant index.
                                let aux_valid = (word_after as usize) < proto.constants.len()
                                    && matches!(
                                        proto.constants.get(word_after as usize),
                                        Some(crate::parser::types::Constant::String(_))
                                    );
                                let aux_chunk_valid = (word_after as usize) < chunk.strings.len()
                                    || (word_after > 0 && ((word_after - 1) as usize) < chunk.strings.len());
                                if !aux_valid && !aux_chunk_valid {
                                    // AUX doesn't resolve — this "instruction" is likely
                                    // AUX data from the unknown opcode, not a real instruction.
                                    // Step by 2 to skip the AUX data.
                                    i += 2;
                                } else if after_maps_known {
                                    i += 1;
                                } else {
                                    i += 1;
                                }
                            } else if after_maps_known {
                                // Both next and the one after look like instructions → step 1
                                i += 1;
                            } else {
                                // Next looks like instruction but one after doesn't —
                                // the "next instruction" might itself have AUX, so step 1
                                i += 1;
                            }
                        } else if !next_maps_known && looks_like_aux {
                            // Next word doesn't map to any known opcode AND looks like
                            // AUX data → skip it as AUX, step by 2
                            i += 2;
                        } else {
                            // Ambiguous — default to step 1 (conservative)
                            i += 1;
                        }
                    } else {
                        i += 1;
                    }
                }
            }
        }
        (unknown_insn_count, unknown_byte_freq, unknown_byte_sample)
    }
}

// ═══════════════════════════════════════════════════════════════
// Detection heuristics
// ═══════════════════════════════════════════════════════════════

fn detect_return(chunk: &Chunk, ctx: &mut DetectCtx) {
    // The final word of a proto is the RETURN instruction itself: the compiler
    // terminates every proto with RETURN and RETURN carries no AUX word, so the
    // last position can never hold trailing AUX data.
    //
    // The second-to-last position is a much weaker signal — the instruction
    // immediately before the terminating RETURN is very often CALL. Pooling the
    // two positions into one candidate table made `CALL; RETURN` tails a
    // coin flip in chunks with few protos: both bytes score the same count and
    // the tie-break simply hands RETURN to whichever byte is numerically
    // smaller. Score the true end position first and consult the second-to-last
    // position only as a fallback.
    let mut last_candidates: HashMap<u8, usize> = HashMap::new();
    let mut penultimate_candidates: HashMap<u8, usize> = HashMap::new();
    for proto in &chunk.protos {
        if proto.code.is_empty() { continue; }
        let last_op = insn_op(*proto.code.last().unwrap());
        *last_candidates.entry(last_op).or_insert(0) += 1;

        if proto.code.len() >= 2 {
            let second_last_op = insn_op(proto.code[proto.code.len() - 2]);
            if second_last_op != last_op {
                *penultimate_candidates.entry(second_last_op).or_insert(0) += 1;
            }
        }
    }
    // RETURN appears at end of nearly every function.
    // Use lower threshold for small proto counts — even 2 protos is enough
    let num_protos = chunk.protos.len().max(1);
    for candidates in [&last_candidates, &penultimate_candidates] {
        if ctx.find_shuffled(LuauOpcode::Return as u8).is_some() {
            return;
        }
        if let Some((&op, &count)) = candidates.iter()
            .filter(|(&op, _)| !ctx.is_mapped(op))
            .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
        {
            let pct = count * 100 / num_protos;
            if pct >= 50 || (count >= 2 && pct >= 30) || (num_protos <= 3 && count >= 1) {
                // Use force assignment — last-instruction detection is structurally reliable
                // and RETURN can be very rare in data-heavy scripts (failing frequency guards)
                ctx.try_assign_force(op, LuauOpcode::Return as u8);
            }
        }
    }
}

fn detect_prepvarargs(chunk: &Chunk, ctx: &mut DetectCtx) {
    let mut candidates: HashMap<u8, usize> = HashMap::new();
    for proto in &chunk.protos {
        if proto.is_vararg && !proto.code.is_empty() {
            let insn = proto.code[0];
            if insn_a(insn) == proto.num_params {
                *candidates.entry(insn_op(insn)).or_insert(0) += 1;
            }
        }
    }
    if let Some((&op, &count)) = candidates.iter()
        .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
    {
        // PrepVarargs is always the first instruction of vararg protos.
        // Even a single vararg proto is reliable if A == num_params.
        if count >= 1 {
            ctx.try_assign(op, LuauOpcode::PrepVarargs as u8);
        }
    }
}

fn detect_getimport(chunk: &Chunk, ctx: &mut DetectCtx) {
    let mut candidates: HashMap<u8, usize> = HashMap::new();
    for proto in &chunk.protos {
        for i in 0..proto.code.len().saturating_sub(1) {
            let insn = proto.code[i];
            let op = insn_op(insn);
            // Skip if this opcode byte is already mapped to something else
            if ctx.is_mapped(op) && ctx.map[op as usize] != LuauOpcode::GetImport as u8 {
                continue;
            }
            let aux = proto.code[i + 1];
            let count = aux >> 30;
            if count >= 1 && count <= 3 {
                let id0 = (aux >> 20) & 0x3FF;
                let id1 = (aux >> 10) & 0x3FF;
                let id2 = aux & 0x3FF;
                // Import IDs are indices into chunk.strings (global string table)
                let valid = match count {
                    1 => (id0 as usize) < chunk.strings.len(),
                    2 => (id0 as usize) < chunk.strings.len()
                        && (id1 as usize) < chunk.strings.len(),
                    3 => (id0 as usize) < chunk.strings.len()
                        && (id1 as usize) < chunk.strings.len()
                        && (id2 as usize) < chunk.strings.len(),
                    _ => false,
                };
                let d = insn_d(insn);
                if valid && d >= 0 {
                    if let Some(Constant::Import(_)) = proto.constants.get(d as usize) {
                        *candidates.entry(op).or_insert(0) += 1;
                    }
                }
            }
        }
    }
    if let Some((&op, &count)) = candidates.iter()
        .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
    {
        // GETIMPORT has an extremely specific structural signature:
        //   - AUX word with count bits (1-3) + valid string IDs
        //   - D field pointing to a Constant::Import entry
        // A single hit with all those constraints satisfied is 99%+ reliable,
        // and small protos (IsServer-style one-liners) only ever have one
        // GETIMPORT. Previously required `count >= 2` which dropped small
        // protos and let the shuffled byte be claimed by a later fallback.
        if count >= 1 {
            // Force assignment — AUX pattern with Import constant is highly specific
            ctx.try_assign_force(op, LuauOpcode::GetImport as u8);
        }
    }
}

fn detect_closure_capture(chunk: &Chunk, ctx: &mut DetectCtx) {
    let mut closure_candidates: HashMap<u8, usize> = HashMap::new();
    let mut capture_candidates: HashMap<u8, usize> = HashMap::new();

    for proto in &chunk.protos {
        if proto.child_protos.is_empty() { continue; }
        for i in 0..proto.code.len() {
            let insn = proto.code[i];
            let op = insn_op(insn);
            let d = insn_d(insn) as i32;
            if d >= 0 && (d as usize) < proto.child_protos.len() {
                let expected_captures = chunk.protos
                    .get(proto.child_protos[d as usize] as usize)
                    .map(|p| p.num_upvalues as usize)
                    .unwrap_or(0);
                if expected_captures > 0 && i + expected_captures < proto.code.len() {
                    let mut ok = true;
                    let mut cap_op = None;
                    for j in 1..=expected_captures {
                        let ci = proto.code[i + j];
                        let ca = insn_a(ci);
                        let co = insn_op(ci);
                        if ca > 2 { ok = false; break; }
                        match cap_op {
                            Some(prev) if prev != co => { ok = false; break; }
                            None => cap_op = Some(co),
                            _ => {}
                        }
                    }
                    if ok {
                        if let Some(cap) = cap_op {
                            *closure_candidates.entry(op).or_insert(0) += 1;
                            *capture_candidates.entry(cap).or_insert(0) += 1;
                        }
                    }
                }
            }
        }
    }
    if let Some((&op, &count)) = closure_candidates.iter()
        .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
    {
        if count >= 2 { ctx.try_assign(op, LuauOpcode::NewClosure as u8); }
    }
    // CAPTURE has very strong structural evidence (A ≤ 2, always follows NEWCLOSURE,
    // same opcode byte for all captures). Use force-assign with threshold of 1.
    if let Some((&op, &count)) = capture_candidates.iter()
        .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
    {
        if count >= 1 { ctx.try_assign_force(op, LuauOpcode::Capture as u8); }
    }
}

fn detect_dupclosure(chunk: &Chunk, ctx: &mut DetectCtx) {
    let mut candidates: HashMap<u8, usize> = HashMap::new();
    for proto in &chunk.protos {
        for &insn in &proto.code {
            let d = insn_d(insn);
            if d >= 0 {
                if let Some(Constant::Closure(_)) = proto.constants.get(d as usize) {
                    *candidates.entry(insn_op(insn)).or_insert(0) += 1;
                }
            }
        }
    }
    if let Some((&op, &count)) = candidates.iter()
        .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
    {
        // The discriminant above is STRONG: D must index an entry that really is
        // a Constant::Closure in this proto's own constant table. That is a hard
        // structural fact about the chunk, not a shape heuristic — unlike
        // detect_closure_capture's "A <= 2 and every capture shares one byte",
        // which ordinary instructions satisfy by coincidence.
        //
        // But detect_closure_capture runs FIRST (opmap.rs:852 vs 853) and
        // force-assigns on a threshold of 1, and a plain try_assign silently
        // fails on an already-mapped byte. So DUPCLOSURE routinely lost its byte
        // and was never assigned at all.
        //
        // Measured on a 628-script corpus extracted from a live client:
        //   * 63 chunks assigned the SAME unbound register to 3+ table fields
        //     (Badges: 25 fields all = v12; DigitalBeeQuests: 21 all = v4)
        //   * RefCounter's 7 methods were all `= v1`, with nothing ever
        //     assigning v1 — the module exported nothing callable
        // The SETTABLEKS instructions were present and correct throughout; only
        // the closure load was missing, because its opcode had been taken.
        //
        // Two occurrences is the bar for displacing an incumbent: one constant
        // slot could coincidentally hold a closure, two independent instructions
        // pointing at closure constants could not.
        if count >= 2 {
            ctx.try_assign_override(op, LuauOpcode::DupClosure as u8, 2);
        } else if count >= 1 {
            ctx.try_assign(op, LuauOpcode::DupClosure as u8);
        }
    }
}

/// DUPTABLE: AD format where D is a constant index pointing to a Table constant.
/// Used for table constructors with known string keys like {x=1, y=2}.
fn detect_duptable(chunk: &Chunk, ctx: &mut DetectCtx) {
    let mut candidates: HashMap<u8, usize> = HashMap::new();
    for proto in &chunk.protos {
        for &insn in &proto.code {
            let op = insn_op(insn);
            if ctx.is_mapped(op) { continue; }
            let a = insn_a(insn);
            let d = insn_d(insn);
            // DUPTABLE: A = target register, D = constant index pointing to Table template
            if a < proto.max_stack_size && d >= 0 {
                if let Some(Constant::Table(_)) = proto.constants.get(d as usize) {
                    *candidates.entry(op).or_insert(0) += 1;
                }
            }
        }
    }
    // Filter out DupClosure which also points to constants (but Closure, not Table)
    // DupTable is rare in many scripts (only used for table templates like {x=1, y=2}).
    // Accept even a single instance since pointing to a Table constant is very specific.
    if let Some((&op, &count)) = candidates.iter()
        .filter(|(&op, _)| !ctx.is_mapped(op))
        .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
    {
        if count >= 1 { ctx.try_assign(op, LuauOpcode::DupTable as u8); }
    }
}

/// Detect NEWTABLE (canonical 53) and SETLIST (55) as a structural PAIR, without
/// requiring either byte — or any fill opcode — to be known in advance.
///
/// `detect_newtable` only credits a table-constructor "fill" when the filling
/// opcode is ALREADY mapped to SetList / SetTableKS / SetTableN, and
/// `detect_setlist` only accepts a candidate preceded by an ALREADY-mapped
/// NewTable / DupTable. On a chunk that builds arrays but never uses
/// `SETTABLEKS`/`SETTABLEN`, the only possible filler is SETLIST, so the two
/// detectors wait on each other and neither ever fires. This closes that cycle
/// the same way `detect_forgprep_inext_pair` closes the FORGPREP/FORGLOOP one:
/// find both bytes jointly from the allocate-then-fill shape.
///
///   [i]  NEWTABLE A, B          ; B = log2 hash-size hint, C = 0
///        <AUX>                  ; array-size hint
///   ...
///   [j]  SETLIST  A, B, C       ; A = the same table register, B = first value
///        <AUX>                  ; 1-based start index
///
/// The discriminating operand facts, all of which hold for upstream Luau's
/// table-constructor lowering (and are confirmed on real Roblox bytecode):
///   * the fill targets the register the creator wrote: `A(fill) == A(create)`
///   * values are laid out immediately above the table: `B - A` is 1 or 2
///   * the FIRST batch of an array constructor always starts at index 1, so the
///     fill's AUX word is exactly 1
///   * the fill's value count (`C - 1`, or 0 for multret) equals the creator's
///     array-size hint
///
/// The AUX==1 constraint is what makes this separable: without it the shape is
/// shared with half the ABC-format opcodes.
fn detect_newtable_setlist_pair(chunk: &Chunk, ctx: &mut DetectCtx) {
    if ctx.assigned[LuauOpcode::NewTable as usize] || ctx.assigned[LuauOpcode::SetList as usize] {
        return;
    }

    // (creator_byte, filler_byte) → number of distinct creator sites that matched.
    let mut pair_cand: HashMap<(u8, u8), usize> = HashMap::new();

    for proto in &chunk.protos {
        let code = &proto.code;
        let ms = proto.max_stack_size;
        let mut i = 0usize;
        while i < code.len() {
            let insn = code[i];
            let op = insn_op(insn);
            if ctx.is_mapped(op) {
                // AUX-aware walk over the part of the map we already trust.
                let std_op = LuauOpcode::from_u8(ctx.map[op as usize]);
                if std_op.has_aux() && i + 1 < code.len() { i += 2; } else { i += 1; }
                continue;
            }

            let a = insn_a(insn);
            // NEWTABLE shape: C is always 0, B is a log2 hash-size hint (0..15),
            // A is a valid register, and the AUX word is a plausible array size.
            if insn_c(insn) != 0 || insn_b(insn) > 15 || a >= ms || i + 1 >= code.len() {
                i += 1;
                continue;
            }
            let aux_create = code[i + 1];
            if aux_create > 65535 {
                i += 1;
                continue;
            }

            // Forward-scan this proto for a SETLIST-shaped fill of R(a).
            // At most one site is counted per (creator_byte, filler_byte) pair
            // per creator position, so one long proto cannot outvote the rest.
            let mut seen: Vec<u8> = Vec::new();
            let mut j = i + 2;
            while j + 1 < code.len() {
                let fill = code[j];
                let fop = insn_op(fill);
                if fop == op || ctx.is_mapped(fop) {
                    j += 1;
                    continue;
                }
                let fb = insn_b(fill);
                let fc = insn_c(fill);
                if insn_a(fill) == a
                    && fb > a
                    && fb <= a.saturating_add(2)
                    && fb < ms
                    && code[j + 1] == 1
                    && (fc == 0 || fc.saturating_sub(1) as u32 == aux_create)
                    && !seen.contains(&fop)
                {
                    seen.push(fop);
                    *pair_cand.entry((op, fop)).or_insert(0) += 1;
                }
                j += 1;
            }

            i += 2; // step past our own AUX word
        }
    }

    // Accept only a clearly dominant pair: either nothing else matched at all,
    // or the winner has at least twice the runner-up's evidence. A wrong early
    // assignment costs two slots and NewTable is structural-required, so
    // permutation_complete would not clean up after it.
    let mut ranked: Vec<((u8, u8), usize)> = pair_cand.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let ((create_byte, fill_byte), best) = match ranked.first() {
        Some(&(pair, n)) => (pair, n),
        None => return,
    };
    let runner_up = ranked.get(1).map(|e| e.1).unwrap_or(0);
    if best == 0 || (runner_up > 0 && best < runner_up * 2) {
        return;
    }

    // try_assign_force: the evidence is structural rather than frequency-based,
    // and NEWTABLE in a table-heavy module can otherwise trip try_assign's
    // rare-opcode frequency cap.
    ctx.try_assign_force(create_byte, LuauOpcode::NewTable as u8);
    ctx.try_assign_force(fill_byte, LuauOpcode::SetList as u8);
}

fn detect_numeric_for(chunk: &Chunk, ctx: &mut DetectCtx) {
    // Detect FORNPREP/FORNLOOP pairs.
    // FORNLOOP does NOT have AUX, while FORGLOOP does.
    // Strategy: look for AD-format prep/loop pairs where:
    //   1. prep jumps forward (d > 0), loop jumps backward (td < 0)
    //   2. prep A == loop A (same loop variable register)
    //   3. The loop target is NOT an already-mapped FORGLOOP
    //   4. If FORGLOOP isn't mapped yet, use AUX heuristic to distinguish
    let forgloop_shuffled = ctx.find_shuffled(LuauOpcode::ForGLoop as u8);
    let forgprep_shuffled = ctx.find_shuffled(LuauOpcode::ForGPrep as u8);

    // Joint (prep_byte, loop_byte) counter — atomic pair assignment.
    // We no longer track prep/loop independently: past detectors assigned
    // ForNPrep without its matching ForNLoop, polluting the cache. The pair
    // constraint is a hard structural invariant of Luau bytecode.
    let mut pair_cand: HashMap<(u8, u8), usize> = HashMap::new();
    for proto in &chunk.protos {
        for i in 0..proto.code.len() {
            let insn = proto.code[i];
            let op = insn_op(insn);
            // Skip if already mapped (could be FORGPREP or other AD instruction)
            if ctx.is_mapped(op) { continue; }
            let d = insn_d(insn) as i32;
            let a = insn_a(insn);
            if d <= 0 || a >= proto.max_stack_size { continue; }

            // Jump target: FORNPREP's D offset skips PAST the FORNLOOP on the
            // "skip the loop entirely" branch. VM: `pc++; if skip { pc += D; }`.
            // So the skip target = prep_pc + 1 + D, and FORNLOOP sits ONE BEFORE
            // that, at loop_pc = prep_pc + D (no +1). This differs from FORGPREP,
            // which unconditionally `pc += D` AFTER the `pc++`, landing AT the
            // FORGLOOP (loop_pc = prep_pc + 1 + D for generic-for).
            //
            // Verified against real ModuleScript.luac Proto 9 `numeric_for_simple`:
            //   pc=4: FORNPREP A=2 D=+2  →  loop at 4+2=6 (FORNLOOP)
            //   pc=5: ADD (body)
            //   pc=6: FORNLOOP A=2 D=-2  →  back to 6+1-2=5 (body start) ✓
            //   pc=7: RETURN              →  FORNPREP skip lands here (4+1+2=7)
            let target = (i as i32 + d) as usize;
            if target >= proto.code.len() { continue; }

            let ti = proto.code[target];
            let target_op = insn_op(ti);
            let td = insn_d(ti) as i32;

            // Loop instruction must jump backward and share same A register
            if insn_a(ti) != a || td >= 0 { continue; }
            // FORNLOOP jumps back to body start = prep_pc + 1 (first insn after FORNPREP).
            // back_target = loop_pc + 1 + td (VM `pc++; if continue { pc += D' }`).
            let back = (target as i32) + td + 1;
            if (back - (i as i32 + 1)).abs() > 1 { continue; }

            // Exclude if target is already mapped as FORGLOOP
            if Some(target_op) == forgloop_shuffled { continue; }
            // Exclude if this prep is already mapped as FORGPREP
            if Some(op) == forgprep_shuffled { continue; }

            // If FORGLOOP not yet mapped, use AUX heuristic:
            // FORGLOOP has AUX word after it in format (count | (is_ipairs << 31))
            // where count is 1-255 (typically 1-10) and bit 31 is set for ipairs.
            // FORNLOOP does not — word after it is the next instruction opcode.
            //
            // A real FORGLOOP AUX has the form:
            //   0x00000001..0x000000FF (pairs, count in low byte)
            //   0x80000001..0x800000FF (ipairs, count in low byte)
            // I.e. high 23 bits (bits 8-30) are all zero.
            // A real follow-on instruction almost always has non-zero bits in that range.
            if forgloop_shuffled.is_none() {
                let has_aux_hint = if target + 1 < proto.code.len() {
                    let maybe_aux = proto.code[target + 1];
                    let count = maybe_aux & 0xFF;
                    let mid = maybe_aux & 0x7FFFFF00;
                    count >= 1 && count <= 15 && mid == 0
                } else {
                    false
                };
                if has_aux_hint { continue; }
            }

            // Atomic pair counter — target_op must also be unmapped to be a
            // viable FORNLOOP candidate.
            if !ctx.is_mapped(target_op) {
                let pair_key = (op, target_op);
                *pair_cand.entry(pair_key).or_insert(0) += 1;
            }
        }
    }
    // PAIR CONSTRAINT: FORNPREP and FORNLOOP ALWAYS come in pairs. Never assign
    // one without the other — a half-pair pollutes the cache and then downstream
    // detectors/lifter produce garbage for scripts where the "other half" should
    // have been detected via the same structural evidence.
    //
    // Strategy (Phase A Patch 1 — relaxed threshold):
    //   1. Find the best (prep_byte, loop_byte) pair by joint count.
    //   2. Require the pair to pass the unambiguous-candidate test:
    //      either pair_count >= 2 (same pair matched multiple times — strong),
    //      or pair_count == 1 AND this is the ONLY candidate pair in the proto
    //      set (pair_sorted.len() == 1). A lone surviving candidate after the
    //      structural filters (D-sign, A-reg equality, back-edge target, AUX
    //      shape rule-out of FORGLOOP) is safe — those filters are strong
    //      enough that there is no credible competing interpretation.
    //   3. Require prep_byte != loop_byte (different opcodes).
    //   4. Assign BOTH or NEITHER.
    //
    // Why NOT accept any single-count pair: if multiple distinct
    // (prep_byte, loop_byte) combinations each have count 1, then the proto
    // contains multiple numeric-for patterns with different bytes. Committing
    // any one of them would be a coin flip. The rare genuine case (a single
    // script simultaneously introducing two new prep/loop bytes) will be
    // picked up by detectors on other scripts, accumulating evidence over
    // the whole cache pool — no need to guess from one proto.
    //
    // Why lower the threshold: many Roblox scripts contain only one numeric
    // for loop. With pair_count >= 2 required, single-loop protos never
    // contribute FORNPREP/FORNLOOP evidence, the cache never fills these
    // bytes, and downstream structured-control-flow collapses to raw jumps.
    let mut pair_sorted: Vec<((u8, u8), usize)> = pair_cand.into_iter().collect();
    pair_sorted.sort_by(|a, b| b.1.cmp(&a.1)
        .then_with(|| a.0.0.cmp(&b.0.0))
        .then_with(|| a.0.1.cmp(&b.0.1)));
    if let Some(&((prep_op, loop_op), pair_count)) = pair_sorted.first() {
        let multi_hit = pair_count >= 2;
        // Lone surviving candidate after structural filters — safe to commit.
        let single_sole_candidate = pair_count >= 1 && pair_sorted.len() == 1;
        let accept = multi_hit || single_sole_candidate;

        if accept
            && prep_op != loop_op
            && !ctx.is_mapped(prep_op)
            && !ctx.is_mapped(loop_op)
        {
            // Atomic assign: both succeed or both are reverted.
            let prep_ok = ctx.try_assign(prep_op, LuauOpcode::ForNPrep as u8);
            if prep_ok {
                let loop_ok = ctx.try_assign(loop_op, LuauOpcode::ForNLoop as u8);
                if !loop_ok {
                    // Revert prep — keep the pair constraint.
                    ctx.map[prep_op as usize] = 255;
                    ctx.assigned[LuauOpcode::ForNPrep as usize] = false;
                }
            }
        }
    }
}

fn detect_generic_for(chunk: &Chunk, ctx: &mut DetectCtx) {
    // Detect FORGPREP → FORGLOOP pairs.
    // FORGLOOP is distinguished from FORNLOOP by the AUX word that follows it:
    //   AUX = (count & 0xFF) | (is_ipairs << 31)
    // count is typically 1-10 (number of loop variables), bit 31 is set for ipairs.
    // So AUX has the structural shape: bits 0-7 are a small count (1-15),
    // bits 8-30 are ALL ZERO, bit 31 may or may not be set.
    // A real follow-on instruction (numeric-for case) almost always has non-zero
    // bits in positions 8-30 (register operands).
    //
    // Luau VM jump semantics: when an AD instruction at position P with D=d runs,
    // PC advances to P+1 (next instruction) THEN adds d. So the jump target is P+d+1.
    // The lifter and CFG both use this convention.
    let mut prep_cand: HashMap<u8, usize> = HashMap::new();
    let mut loop_cand: HashMap<u8, usize> = HashMap::new();
    // Split prep candidates by whether the FORGLOOP they reach carries the ipairs
    // fast-path flag in AUX bit 31. The AUX word is already in hand below; only
    // its low bits were being consulted. See the assignment site for why.
    let mut prep_inext: HashMap<u8, usize> = HashMap::new();
    let mut prep_plain: HashMap<u8, usize> = HashMap::new();
    for proto in &chunk.protos {
        for i in 0..proto.code.len() {
            let insn = proto.code[i];
            let d = insn_d(insn) as i32;
            let a = insn_a(insn);
            if d > 0 {
                let target = (i as i32 + d + 1) as usize;
                if target + 1 < proto.code.len() {
                    let ti = proto.code[target];
                    let td = insn_d(ti) as i32;
                    let aux = proto.code[target + 1];
                    // Tight AUX check: low byte is count 1-15, mid bits 8-30 are zero.
                    // This excludes most real instructions (which have register operands
                    // in bits 8-23 or longer immediate values in bits 16-31).
                    let count = aux & 0xFF;
                    let mid = aux & 0x7FFFFF00;
                    let looks_like_forgloop_aux = count >= 1 && count <= 15 && mid == 0;
                    if insn_a(ti) == a && td < 0 && looks_like_forgloop_aux {
                        // FORGLOOP jumps back to body start = i + 1 (first insn after FORGPREP)
                        let back = (target as i32) + td + 1;
                        if (back - (i as i32 + 1)).abs() <= 1 {
                            *prep_cand.entry(insn_op(insn)).or_insert(0) += 1;
                            *loop_cand.entry(insn_op(ti)).or_insert(0) += 1;
                            if aux & 0x8000_0000 != 0 {
                                *prep_inext.entry(insn_op(insn)).or_insert(0) += 1;
                            } else {
                                *prep_plain.entry(insn_op(insn)).or_insert(0) += 1;
                            }
                        }
                    }
                }
            }
        }
    }
    if let Some((&op, &count)) = prep_cand.iter()
        .filter(|(&op, _)| !ctx.is_mapped(op))
        .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
    {
        if count >= 1 {
            // Which prep variant is this? The compiler sets AUX bit 31 exactly
            // when it selected the ipairs specialisation, and the instruction
            // that prepares such a loop is FORGPREP_INEXT, not FORGPREP.
            //
            // Labelling it FORGPREP — which is what picking the single
            // highest-count candidate does — is wrong twice over: the byte gets
            // the wrong opcode AND the real FORGPREP byte is left with nowhere
            // to go, because `assigned[ForGPrep]` is now taken. A chunk whose
            // generic-for loops are all `ipairs` loops is the common case, and
            // this misorder alone accounted for 13 of 47 corpus files under
            // every shuffle seed measured.
            //
            // Only act on unanimous evidence; a byte seen preparing both loop
            // kinds is not a variant-specific opcode and falls through to the
            // historical behaviour.
            let ipairs_hits = prep_inext.get(&op).copied().unwrap_or(0);
            let plain_hits = prep_plain.get(&op).copied().unwrap_or(0);
            let variant = if ipairs_hits > 0 && plain_hits == 0 {
                LuauOpcode::ForGPrepINext
            } else {
                LuauOpcode::ForGPrep
            };
            ctx.try_assign(op, variant as u8);
        }
    }
    if let Some((&op, &count)) = loop_cand.iter()
        .filter(|(&op, _)| !ctx.is_mapped(op))
        .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
    {
        if count >= 1 { ctx.try_assign(op, LuauOpcode::ForGLoop as u8); }
    }
}

/// Detect FORGPREP, FORGPREPINEXT, FORGPREPNEXT by finding AD-format instructions
/// that jump forward to a known FORGLOOP instruction.
fn detect_forgprep_variants(chunk: &Chunk, ctx: &mut DetectCtx) {
    let forgloop_shuffled = match ctx.find_shuffled(LuauOpcode::ForGLoop as u8) {
        Some(op) => op,
        None => return,
    };

    // Collect all prep candidates: AD-format, forward jump D>0, target is FORGLOOP
    let mut prep_cand: HashMap<u8, usize> = HashMap::new();
    // Split those candidates by the kind of FORGLOOP they jump to. FORGLOOP's AUX
    // is `nresults | (is_ipairs << 31)`; bit 31 is the ipairs fast-path flag the
    // disassembler already renders as `[inext]` (disasm/mod.rs). A prep byte whose
    // targets all carry that flag is FORGPREP_INEXT; one whose targets never do is
    // plain FORGPREP.
    let mut inext_cand: HashMap<u8, usize> = HashMap::new();
    let mut plain_cand: HashMap<u8, usize> = HashMap::new();

    for proto in &chunk.protos {
        for i in 0..proto.code.len() {
            let insn = proto.code[i];
            let op = insn_op(insn);
            if ctx.is_mapped(op) { continue; }
            let a = insn_a(insn);
            let d = insn_d(insn) as i32;
            if d <= 0 || a >= proto.max_stack_size { continue; }

            // Jump target: VM semantics = i + d + 1 (PC advances past current insn then adds d)
            let target = (i as i32 + d + 1) as usize;
            if target >= proto.code.len() { continue; }

            // Check if target instruction is FORGLOOP
            if insn_op(proto.code[target]) == forgloop_shuffled
                && insn_a(proto.code[target]) == a
            {
                *prep_cand.entry(op).or_insert(0) += 1;

                // Classify by the FORGLOOP's AUX ipairs flag (bit 31), NOT by the
                // loop-variable count in the low bits. The count cannot separate
                // the variants — `for i, v in ipairs(t)` and `for k, v in pairs(t)`
                // both yield 2 — whereas the flag is set by the compiler precisely
                // when it selected the ipairs specialisation, which is exactly when
                // the prep instruction is FORGPREP_INEXT.
                if target + 1 < proto.code.len() {
                    let aux = proto.code[target + 1];
                    if aux & 0x8000_0000 != 0 {
                        *inext_cand.entry(op).or_insert(0) += 1;
                    } else {
                        *plain_cand.entry(op).or_insert(0) += 1;
                    }
                }
            }
        }
    }

    // Sort candidates by frequency — the most common is likely the general FORGPREP.
    // Secondary key: byte value (ascending) for deterministic tiebreak.
    let mut sorted: Vec<_> = prep_cand.iter()
        .filter(|(&op, _)| !ctx.is_mapped(op))
        .map(|(&op, &count)| (op, count))
        .collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    // Assign prep variants: general FORGPREP first, then FORGPREP_NEXT.
    // NOTE: FORGPREP_INEXT is NOT assigned here. It jumps to FORGLOOP_INEXT
    // (Deprecated61/canonical 61), NOT to the standard FORGLOOP. Assigning it
    // here based on ForGLoop-jump evidence is a false positive.
    // Use detect_forgprep_inext_pair() instead, which finds the
    // ForGPrepINext ↔ ForGLoopINext structural pair without knowing either byte.
    for &(op, count) in &sorted {
        if count < 1 { continue; }

        // Unambiguous AUX evidence wins over frequency order.
        //
        // Without this, a chunk whose only generic-for loops are `ipairs` loops —
        // extremely common — hands its FORGPREP_INEXT byte to plain FORGPREP purely
        // because it is the single highest-count candidate, and the true FORGPREP
        // byte is then homeless. That single misorder accounted for 13 of 47 files
        // in every shuffle seed measured. The classification below is unanimous on
        // that corpus: every prep byte reaching an ipairs-flagged FORGLOOP is
        // FORGPREP_INEXT, and every prep byte reaching an unflagged one is FORGPREP.
        let ipairs_hits = inext_cand.get(&op).copied().unwrap_or(0);
        let plain_hits = plain_cand.get(&op).copied().unwrap_or(0);
        if ipairs_hits > 0 && plain_hits == 0 {
            if !ctx.assigned[LuauOpcode::ForGPrepINext as usize] {
                let _ = ctx.try_assign(op, LuauOpcode::ForGPrepINext as u8);
                continue;
            }
        } else if plain_hits > 0 && ipairs_hits == 0 {
            if !ctx.assigned[LuauOpcode::ForGPrep as usize] {
                let _ = ctx.try_assign(op, LuauOpcode::ForGPrep as u8);
                continue;
            }
        }

        // Mixed or absent AUX evidence: fall back to frequency order.
        if !ctx.assigned[LuauOpcode::ForGPrep as usize] {
            let _ = ctx.try_assign(op, LuauOpcode::ForGPrep as u8);
        } else if !ctx.assigned[LuauOpcode::ForGPrepNext as usize] {
            let _ = ctx.try_assign(op, LuauOpcode::ForGPrepNext as u8);
        } else if !ctx.assigned[LuauOpcode::ForGPrepINext as usize] {
            // Phase B0.35: modern Luau deprecated ForGLoopINext (canonical 61).
            // FORGPREP_INEXT now jumps to regular FORGLOOP (with ipairs AUX hint).
            // detect_forgprep_inext_pair only succeeds when FORGLOOPINEXT exists,
            // so for scripts compiled without deprecated FORGLOOPINEXT, the 3rd
            // FORGPREP-like byte should be assigned to FORGPREP_INEXT.
            let _ = ctx.try_assign(op, LuauOpcode::ForGPrepINext as u8);
        }
    }
}

/// Detect FORGLOOP_INEXT (canonical 61 / Deprecated61) by finding the instruction
/// that FORGPREP_INEXT (canonical 60) jumps forward to.
///
/// FORGPREP_INEXT at PC=i with D=d has VM jump target = i + d + 1.
/// That target instruction is FORGLOOP_INEXT (the loop-back for ipairs-style loops).
/// It must satisfy:
///   - A == FORGPREP_INEXT.A (same iterator base register)
///   - D < 0 (backward jump to loop body start)
///   - Back target ≈ i + 1 (body start = instruction after FORGPREP_INEXT)
///
/// This is the paired detector for detect_forgprep_variants: where
/// FORGPREP_INEXT can jump either to FORGLOOP (general) or to FORGLOOP_INEXT
/// (specialized ipairs back-edge), this function captures the latter.
///
/// Phase B0.15 model correction:
/// In Roblox's Luau compiler, ForGLoopINext sits at the TOP of the loop body
/// (the jump target of ForGPrepINext), NOT at the end. The D field encodes a
/// FORWARD EXIT JUMP offset (unsigned 16-bit) to after the loop:
///
///   ForGPrepINext pc=X, D=d  →  jumps to ForGLoopINext at pc = X+d+1
///   ForGLoopINext pc=T, A=a, D=D_u16 (unsigned forward exit):
///     • if iterator valid:  fall through to T+1 (first body instruction)
///     • if iterator done:   jump FORWARD to pc = T + D_u16 + 1 (after loop)
///   Body: T+1 … T+D_u16-1
///   At T+D_u16: JUMPBACK to T (the ForGLoopINext)
///   At T+D_u16+1: after-loop code
///
/// OPCODE_TRACE data confirmed this model: 0x35 at pc=2 with D_signed=-4352
/// means D_unsigned=61184, exit_target=61187 — a valid position in a 65K-insn
/// Animate.lua proto. The old "back_target ≈ i+1" check was based on the
/// wrong model (bottom-of-loop backward D) and rejected ALL real occurrences.
fn detect_forgloopinext(chunk: &Chunk, ctx: &mut DetectCtx) {
    let forgprep_inext_shuffled = match ctx.find_shuffled(LuauOpcode::ForGPrepINext as u8) {
        Some(op) => op,
        None => return, // ForGPrepINext not yet detected — try again later
    };

    let mut candidates: HashMap<u8, usize> = HashMap::new();

    for proto in &chunk.protos {
        for i in 0..proto.code.len() {
            let insn = proto.code[i];
            let op = insn_op(insn);
            if op != forgprep_inext_shuffled { continue; }

            let a = insn_a(insn);
            let d = insn_d(insn) as i32;
            if d <= 0 || a >= proto.max_stack_size { continue; }

            // FORGPREP_INEXT jump target: i + d + 1
            let target = (i as i32 + d + 1) as usize;
            if target >= proto.code.len() { continue; }

            let target_insn = proto.code[target];
            let target_op = insn_op(target_insn);
            if ctx.is_mapped(target_op) { continue; }

            let target_a = insn_a(target_insn);

            // ForGLoopINext must share A with its ForGPrepINext
            if target_a != a { continue; }

            // Phase B0.15: ForGLoopINext uses UNSIGNED D as a forward exit jump.
            // D_signed (i16) is negative when D_unsigned > 32767 (large loop exit).
            // Validate: exit_target = target + D_u16 + 1 must be within proto bounds
            // AND at least 1 instruction ahead (body must exist).
            let target_d_u = insn_d(target_insn) as u16 as usize;
            if target_d_u == 0 { continue; } // degenerate: no body
            let exit_target = target + target_d_u + 1;
            if exit_target > proto.code.len() { continue; } // exit must be in-range

            *candidates.entry(target_op).or_insert(0) += 1;
        }
    }

    if let Some((&op, &count)) = candidates.iter()
        .filter(|(&op, _)| !ctx.is_mapped(op))
        .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
    {
        if count >= 1 && loop_byte_frequency_is_plausible(ctx, op, count) {
            // Phase B0.14+B0.15: use try_assign_force to bypass the 2%-frequency
            // rare-opcode cap. Animate.lua has 69 ForGLoopINext instructions (many
            // ipairs loops), and structural evidence (ForGPrepINext → ForGLoopINext
            // pair with matching A + valid forward exit target) is very strong.
            ctx.try_assign_force(op, LuauOpcode::Deprecated61 as u8);
        }
    }
}

/// Is `loop_byte` frequent enough to be ForGLoopINext, given that only `pair_count`
/// of its occurrences were confirmed as the target of a ForGPrepINext jump?
///
/// Every ForGLoopINext instruction is by construction the jump target of a
/// ForGPrepINext — the compiler emits the two together, one pair per ipairs loop.
/// So for the true byte, essentially every occurrence is accounted for by the pair
/// scan. Both pair detectors, however, assign on `count >= 1` and do it through
/// `try_assign_force`, which deliberately bypasses the rare-opcode frequency cap in
/// `try_assign`. On a large module a single coincidental match can therefore claim a
/// byte that occurs hundreds of times.
///
/// That failure is unusually damaging because the lifter emits nothing at all for
/// Deprecated61 (`opcode_handlers.rs`: `LuauOpcode::Deprecated61 => {}`). Every
/// instruction carrying the stolen byte is dropped from the output without a
/// diagnostic — clean, plausible, and missing 8% of the program.
///
/// The bound is deliberately loose. `ctx.freq` counts every word including AUX data,
/// so a genuine ForGLoopINext byte can be diluted by unrelated AUX words that happen
/// to share its value — but not by an order of magnitude. Requiring the pair evidence
/// to explain a tenth of the raw occurrences rejects the coincidences while leaving
/// real ipairs-heavy scripts (Animate.lua: 69 ForGLoopINext instructions, ~69 pairs)
/// far inside the limit.
fn loop_byte_frequency_is_plausible(ctx: &DetectCtx, loop_byte: u8, pair_count: usize) -> bool {
    let freq = ctx.freq[loop_byte as usize] as usize;
    freq == 0 || pair_count.saturating_mul(10) >= freq
}

/// Detect ForGPrepINext (canonical 60) and Deprecated61/ForGLoopINext (canonical 61) as a
/// structural pair, WITHOUT needing either byte to be known in advance.
///
/// In Roblox Luau's ipairs fast-path (LOOP AT TOP model), the structure is:
///   [prep_pc] FORGPREP_INEXT A, D_prep  ; D_prep small (0–500), jumps to loop_pc
///   [loop_pc] FORGLOOP_INEXT A, D_large  ; D_unsigned large (> 20), forward exit
///   [body...]
///   JUMPBACK to loop_pc
///   [after_loop_pc = loop_pc + D_large + 1]
///
/// ForGLoopINext signature: A < max_stack, D_unsigned (treated as forward exit) > 20,
///   exit target in range, AND is jumped to by a ForGPrepINext (A-match).
/// ForGPrepINext signature: small positive D_signed (0–500), A < max_stack, target
///   matches ForGLoopINext candidate (same A, valid ForGLoopINext shape).
///
/// This replaces the old detect_forgloopinext approach that required ForGPrepINext to
/// be pre-detected. The pair detector finds both simultaneously.
///
/// NOTE: detect_forgprep_variants does NOT assign ForGPrepINext (it was removed from
/// that function because ForGPrepINext jumps to ForGLoopINext, not ForGLoop). This
/// function is the sole source of ForGPrepINext assignment.
fn detect_forgprep_inext_pair(chunk: &Chunk, ctx: &mut DetectCtx) {
    // Skip if both already assigned.
    if ctx.assigned[LuauOpcode::ForGPrepINext as usize]
        && ctx.assigned[LuauOpcode::Deprecated61 as usize]
    {
        return;
    }

    // Joint pair candidates: (prep_shuffled_byte, loop_shuffled_byte) → occurrence count.
    let mut pair_cand: HashMap<(u8, u8), usize> = HashMap::new();

    for proto in &chunk.protos {
        // Walk true instruction positions, skipping AUX words of known instructions.
        let mut i = 0usize;
        while i < proto.code.len() {
            let insn = proto.code[i];
            let op = insn_op(insn);

            if ctx.is_mapped(op) {
                // Special case: if this op is already assigned to ForGPrepINext but
                // Deprecated61 is not yet found, fall through so we can identify the
                // loop byte from structural pair evidence. Without this, the scan skips
                // all ForGPrepINext instructions and pair_cand stays empty.
                let is_known_prep = !ctx.assigned[LuauOpcode::Deprecated61 as usize]
                    && ctx.map[op as usize] == LuauOpcode::ForGPrepINext as u8;
                if !is_known_prep {
                    let canon = ctx.map[op as usize];
                    let luau_op = LuauOpcode::from_u8(canon);
                    if luau_op.has_aux() && i + 1 < proto.code.len() {
                        i += 2;
                    } else {
                        i += 1;
                    }
                    continue;
                }
                // Fall through: treat this known ForGPrepINext instruction as a
                // pair candidate to identify the Deprecated61 (ForGLoopINext) byte.
            }

            let a = insn_a(insn);
            let d_s = insn_d(insn) as i32;

            // ForGPrepINext candidate: non-negative D (forward or zero jump), small enough
            // that we can plausibly be near the loop header, A < max_stack.
            // Allow D_signed in [0, 500] — covers LOOP AT TOP (D=0..5) and moderate-gap
            // layouts. Large-gap layouts (D > 500) are uncommon for the INEXT setup.
            if d_s >= 0
                && d_s < 500
                && (a as usize) < proto.max_stack_size as usize
            {
                let target_pc = (i as i32 + d_s + 1) as usize;
                if target_pc >= proto.code.len() {
                    i += 1;
                    continue;
                }

                let target_insn = proto.code[target_pc];
                let target_op = insn_op(target_insn);

                // Inline helper: is a canonical opcode in the JumpXEq family (78-81)?
                // ForGLoopINext (Deprecated61, canonical 61) is an AD-format instruction
                // with NO AUX. JumpXEqKNil/KB/KN/KS (canonical 78-81) ARE AD-format with
                // AUX. Because both use the same instruction word shape and JumpXEqKB in
                // particular has a very permissive AUX check (aux_low31 <= 1), an early
                // JumpXEq assignment for the ForGLoopINext byte can get locked into the
                // cache before Animate.lua (with 69 ForGLoopINext instructions) is processed.
                // To recover: allow the scan to COUNT pairs even when target_op is already
                // mapped to a JumpXEq opcode, so the structural pair evidence can override it.
                let target_mapped_to_jumpxeq = ctx.is_mapped(target_op) && {
                    let c = ctx.map[target_op as usize];
                    c >= LuauOpcode::JumpXEqKNil as u8 && c <= LuauOpcode::JumpXEqKS as u8
                };
                let target_unmapped = !ctx.is_mapped(target_op);
                // Skip target if: same byte as prep, OR mapped to a non-JumpXEq opcode.
                if target_op == op || (!target_unmapped && !target_mapped_to_jumpxeq) {
                    i += 1;
                    continue;
                }

                let target_a = insn_a(target_insn);
                // ForGLoopINext candidate: same A register as prep,
                // D_unsigned (forward exit offset) is large (body must fit between
                // loop header and exit), exit target is in-range.
                // Threshold D_unsigned > 20 excludes tiny synthetic values.
                let target_d_u = insn_d(target_insn) as u16 as usize;
                if target_a == a && target_d_u > 20 {
                    let exit_target = target_pc + target_d_u + 1;
                    if exit_target <= proto.code.len() {
                        *pair_cand.entry((op, target_op)).or_insert(0) += 1;
                    }
                }
            }

            i += 1;
        }
    }

    // Compute prep_total as the sum of ALL pair-cand entries for each prep_byte.
    // This counts only occurrences that actually formed structurally valid pairs
    // (correct D range, in-bounds target, matching A register, large D_unsigned),
    // excluding AUX-ghost appearances where raw bytes accidentally match the prep byte.
    // A JUMP-like opcode that lands on many different target bytes will have
    // prep_total[JUMP] = sum of many small counts, so no single target exceeds 80%.
    // ForGPrepINext (which ALWAYS jumps to ForGLoopINext) concentrates 100% on one target.
    let mut prep_total: HashMap<u8, usize> = HashMap::new();
    for (&(pb, _lb), &cnt) in &pair_cand {
        *prep_total.entry(pb).or_insert(0) += cnt;
    }

    // Inline helper closure: is a canonical opcode in the JumpXEq family (78-81)?
    let is_jumpxeq_canon = |canon: u8| -> bool {
        canon >= LuauOpcode::JumpXEqKNil as u8 && canon <= LuauOpcode::JumpXEqKS as u8
    };

    // Select the highest-frequency CONSISTENT pair.
    // Consistency requirement: ≥80% of prep_byte's structurally-valid occurrences
    // must land on the same loop_byte. This eliminates JUMP-like false positives that
    // scatter across many target opcodes.
    // loop_byte can be unmapped OR mapped to JumpXEq (confusable false positive).
    // Secondary tiebreak: lower loop_byte wins (prefer rare bytes).
    if let Some((&(prep_byte, loop_byte), &count)) = pair_cand.iter()
        .filter(|(&(pb, lb), &cnt)| {
            if ctx.is_mapped(pb) {
                // Allow if pb is already correctly assigned to ForGPrepINext.
                // In that case we're only looking for the loop byte (Deprecated61).
                if ctx.map[pb as usize] != LuauOpcode::ForGPrepINext as u8 {
                    return false;
                }
            }
            // lb must be unmapped OR mapped to a JumpXEq opcode (which we can override)
            if ctx.is_mapped(lb) && !is_jumpxeq_canon(ctx.map[lb as usize]) {
                return false;
            }
            // Consistency check: pair_count / prep_total >= 80%
            let total = *prep_total.get(&pb).unwrap_or(&0);
            total == 0 || cnt * 10 >= total * 8
        })
        .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.1.cmp(&a.0.1)))
    {
        // Reject a pair whose loop byte is far too frequent to be explained by the
        // handful of matches found. See `loop_byte_frequency_is_plausible`: a single
        // coincidental pair must not be allowed to claim a byte that occurs hundreds
        // of times, because Deprecated61 lifts to nothing and those instructions then
        // vanish from the output with no diagnostic.
        if count >= 1 && loop_byte_frequency_is_plausible(ctx, loop_byte, count) {
            if !ctx.assigned[LuauOpcode::ForGPrepINext as usize] {
                // try_assign_force: bypasses the 2%-frequency rare-opcode cap.
                // ForGPrepINext may appear many times in scripts with many ipairs loops
                // (e.g. Animate.lua: 68 ForGLoopINext occurrences exceed the cap).
                ctx.try_assign_force(prep_byte, LuauOpcode::ForGPrepINext as u8);
            }
            if !ctx.assigned[LuauOpcode::Deprecated61 as usize] {
                // If loop_byte was wrongly assigned to a JumpXEq opcode (common false
                // positive: ForGLoopINext has no AUX but JumpXEqKB has a very permissive
                // AUX check), override it with the structural pair evidence.
                if ctx.is_mapped(loop_byte) && is_jumpxeq_canon(ctx.map[loop_byte as usize]) {
                    let prev_canon = ctx.map[loop_byte as usize] as usize;
                    ctx.assigned[prev_canon] = false;
                    ctx.map[loop_byte as usize] = LuauOpcode::Deprecated61 as u8;
                    ctx.assigned[LuauOpcode::Deprecated61 as usize] = true;
                    ctx.evidence[loop_byte as usize] = 4; // structural pair > AUX heuristic
                    ctx.locked[loop_byte as usize] = false; // un-lock the wrong prior entry
                } else {
                    ctx.try_assign_force(loop_byte, LuauOpcode::Deprecated61 as u8);
                }
            }
        }
    }
}

fn detect_call(chunk: &Chunk, ctx: &mut DetectCtx) {
    // CALL: A = func register, B = nargs+1 (0=vararg), C = nresults+1 (0=multi-return)
    // Key constraints:
    // 1. A < max_stack_size (function register)
    // 2. B is typically 0-6 (most functions have 0-5 args)
    // 3. C is typically 0-3 (most calls return 0-2 values)
    // 4. CALL is one of the most frequent instructions in any bytecode
    //
    // CRITICAL discriminant vs GETUPVAL (AB format, C always=0):
    // GETUPVAL has C=0 for 100% of instances (it's a two-field format).
    // CALL has C>0 for ~80% of instances (most calls return something).
    // We require at least 15% of instances to have C>0 — this eliminates
    // GETUPVAL, MOVE, unary ops (all C=0 format) from being mis-selected.
    //
    // [0] = small_c (c <= 2), [1] = total, [2] = c_positive (c > 0)
    let mut candidates: HashMap<u8, usize> = HashMap::new();
    let mut call_bc_pattern: HashMap<u8, [u32; 3]> = HashMap::new();
    for proto in &chunk.protos {
        for &insn in &proto.code {
            let op = insn_op(insn);
            // Consider bytes another detector already claimed, PROVIDED that
            // claim is weak (evidence <= 2, i.e. a plain try_assign or a
            // single-match force-assign).
            //
            // Skipping every mapped byte is what let CALL be lost entirely:
            // detect_closure_capture (line 790) and detect_duptable (line 792)
            // both run before this and both force-claim on thin evidence, so
            // whichever guessed first held CALL's byte permanently and this
            // detector — the only one with a real statistical discriminant —
            // never even evaluated it. Measured on CameraModule: CALL never
            // assigned, every call decoded as a no-op, 32 protos -> `return {}`.
            //
            // Strongly-held bytes are still skipped, so this cannot disturb a
            // mapping that several passes agree on.
            if ctx.is_mapped(op) && ctx.evidence_for(op) > 2 { continue; }
            let a = insn_a(insn);
            let b = insn_b(insn);
            let c = insn_c(insn);
            // CALL: A is function register, B = nargs+1 (usually 0-6), C = nresults+1 (usually 0-3)
            if a < proto.max_stack_size && b <= 8 && c <= 5 {
                *candidates.entry(op).or_insert(0) += 1;
                let entry = call_bc_pattern.entry(op).or_insert([0, 0, 0]);
                entry[1] += 1;
                // CALL typically has C in {0, 1, 2} (multi-return, 0 results, or 1 result)
                if c <= 2 {
                    entry[0] += 1;
                }
                // Track C > 0: CALL returns values in ~80% of real calls.
                // GETUPVAL/MOVE/unary formats have C=0 always — they score 0%.
                if c > 0 {
                    entry[2] += 1;
                }
            }
        }
    }
    // CALL should be very frequent AND have C concentrated in 0-2 AND have
    // a meaningful fraction of C>0 instances (to distinguish from C-always-0 formats).
    // Deterministic selection: collect viable candidates, sort by count desc then byte asc.
    let mut strict: Vec<(u8, usize)> = candidates.iter()
        .filter(|(&op, &count)| {
            if count < 10 { return false; }
            match call_bc_pattern.get(&op) {
                Some(pattern) => {
                    let total = pattern[1];
                    if total == 0 { return false; }
                    // c <= 2 ratio >= 60%
                    if pattern[0] * 100 / total < 60 { return false; }
                    // c > 0 ratio >= 15% — rejects GETUPVAL (always C=0)
                    if pattern[2] * 100 / total < 15 { return false; }
                    true
                }
                None => false,
            }
        })
        .map(|(&op, &count)| (op, count))
        .collect();
    strict.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let pick = strict.first().copied().or_else(|| {
        let mut loose: Vec<(u8, usize)> = candidates.iter()
            .filter(|(&op, &count)| {
                if count < 3 { return false; }
                match call_bc_pattern.get(&op) {
                    Some(pattern) => {
                        let total = pattern[1];
                        if total == 0 { return false; }
                        // c <= 2 ratio >= 50%
                        if pattern[0] * 100 / total < 50 { return false; }
                        // c > 0 ratio >= 15% — rejects GETUPVAL (always C=0)
                        if pattern[2] * 100 / total < 15 { return false; }
                        true
                    }
                    None => false,
                }
            })
            .map(|(&op, &count)| (op, count))
            .collect();
        loose.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        loose.first().copied()
    });

    if let Some((op, _)) = pick {
        // A byte reaching the STRICT filter has >= 10 instances, >= 60% with
        // C <= 2, and >= 15% with C > 0 — a statistical signature far stronger
        // than the structural coincidences that produce weak claims. That
        // earns the right to displace an incumbent holding the byte on
        // evidence <= 2. The loose fallback below does NOT get that right.
        let from_strict = strict.first().map(|&(o, _)| o) == Some(op);
        if from_strict {
            ctx.try_assign_override(op, LuauOpcode::Call as u8, 2);
        } else {
            ctx.try_assign_force(op, LuauOpcode::Call as u8);
        }
    }
}

fn detect_namecall(chunk: &Chunk, ctx: &mut DetectCtx) {
    let call_op = ctx.find_shuffled(LuauOpcode::Call as u8);

    let mut candidates: HashMap<u8, usize> = HashMap::new();
    for proto in &chunk.protos {
        for i in 0..proto.code.len().saturating_sub(2) {
            let insn = proto.code[i];
            let op = insn_op(insn);
            if ctx.is_mapped(op) { continue; }
            let a = insn_a(insn);
            let b = insn_b(insn);
            let aux = proto.code[i + 1];

            // NAMECALL A B AUX: A+1=B(self), A=method. AUX=string constant index.
            if a < proto.max_stack_size && b < proto.max_stack_size
                && ((aux as usize) < proto.constants.len() || (aux as usize) < chunk.strings.len())
            {
                // Check AUX points to a string constant (method name)
                let aux_is_string = if (aux as usize) < proto.constants.len() {
                    matches!(proto.constants.get(aux as usize), Some(Constant::String(_)))
                } else {
                    (aux as usize) < chunk.strings.len()
                };

                if !aux_is_string { continue; }

                // If we have CALL mapped, verify pc+2 is CALL with same A
                if let Some(call) = call_op {
                    let next_insn = proto.code[i + 2];
                    if insn_op(next_insn) == call && insn_a(next_insn) == a {
                        *candidates.entry(op).or_insert(0) += 1;
                    }
                } else {
                    // Without CALL, just check the AUX pattern:
                    // NAMECALL always uses AUX, so pc+1 is AUX data, pc+2 should be
                    // a different instruction. The key identifier is that AUX is a valid
                    // string constant index and A,B are valid registers.
                    *candidates.entry(op).or_insert(0) += 1;
                }
            }
        }
    }

    // NAMECALL should be one of the most frequent AUX-using opcodes
    // Filter: at least 50% of AUX values must point to string constants.
    // Previously required `count >= 3` which dropped single-use cases in small
    // scripts (e.g., a module that only has one `game:GetService(...)` call).
    // With CALL mapped, the `pc+2 == CALL` check makes single hits highly
    // reliable; without CALL, require count >= 2 to reduce noise.
    let require_count = if call_op.is_some() { 1 } else { 2 };
    // Deterministic: collect viable, sort by count desc then byte asc.
    let mut viable: Vec<(u8, usize)> = Vec::new();
    for (&op, &count) in &candidates {
        if count < require_count { continue; }

        // Verify string AUX ratio
        let mut string_aux = 0u32;
        let mut total = 0u32;
        for proto in &chunk.protos {
            for i in 0..proto.code.len().saturating_sub(1) {
                if insn_op(proto.code[i]) == op {
                    total += 1;
                    let aux = proto.code[i + 1];
                    if (aux as usize) < proto.constants.len() {
                        if matches!(proto.constants.get(aux as usize), Some(Constant::String(_))) {
                            string_aux += 1;
                        }
                    }
                }
            }
        }
        // With CALL linkage, single hits are trustworthy (100% if total=1).
        // Without CALL linkage, require the same 50% string-AUX ratio as before.
        let ratio_ok = total > 0 && string_aux * 100 / total >= 50;
        if ratio_ok {
            viable.push((op, count));
        }
    }
    viable.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    if let Some(&(op, _)) = viable.first() {
        ctx.try_assign(op, LuauOpcode::NameCall as u8);
    }
}

fn detect_loadk(chunk: &Chunk, ctx: &mut DetectCtx) {
    let mut candidates: HashMap<u8, (usize, usize)> = HashMap::new(); // (valid_count, total_count)
    // Purity bookkeeping, kept separate so the selection logic below is untouched.
    //   seen     — EVERY occurrence of the byte, including ones the window test
    //              rejects. The pair above only counts in-window occurrences, so
    //              on its own it can never expose impurity: the disqualifying
    //              evidence is discarded before it is counted.
    //   loadable — in-range constants whose type LOADK can actually load.
    let mut seen: HashMap<u8, usize> = HashMap::new();
    let mut loadable: HashMap<u8, usize> = HashMap::new();
    for proto in &chunk.protos {
        for &insn in &proto.code {
            let op = insn_op(insn);
            if ctx.is_mapped(op) { continue; }
            let a = insn_a(insn);
            let d = insn_d(insn);
            *seen.entry(op).or_insert(0) += 1;
            // LOADK: AD format, A = target register, D = constant index (signed i16).
            // Must use `d as u16 as usize` for constant lookup (see CLAUDE.md).
            if d >= 0 && a < proto.max_stack_size {
                let d_idx = d as u16 as usize;
                let entry = candidates.entry(op).or_insert((0, 0));
                entry.1 += 1; // total instances of this opcode
                if d_idx < proto.constants.len() {
                    entry.0 += 1; // valid constant index
                    // Which constant types does LOADK actually load? Not "any":
                    // the compiler emits LOADNIL for nil, LOADB for booleans,
                    // GETIMPORT for imports, DUPTABLE for table templates and
                    // DUPCLOSURE/NEWCLOSURE for closures. That leaves numbers,
                    // strings and vectors. Measured over a 47-program corpus and
                    // over a 314-proto Roblox module, the true LOADK byte scored
                    // 427/427 and 934/934 on this test — exactly 100% both times.
                    if matches!(
                        proto.constants.get(d_idx),
                        Some(Constant::Number(_)) | Some(Constant::String(_)) | Some(Constant::Vector(..))
                    ) {
                        *loadable.entry(op).or_insert(0) += 1;
                    }
                }
            }
        }
    }

    // Purity veto.
    //
    // Both selection paths below rank on the ABSOLUTE count of conforming
    // instances. That loses to LOADN on frequency alone: LOADN is AD-format with
    // D = a small integer literal, so a literal below the constant-table size is
    // indistinguishable from a constant index, and LOADN is simply more common.
    // Measured on the corpus: the true LOADK byte is 427/427 = 100% in-range,
    // the true LOADN byte 433/587 = 74%, yet 433 > 427 so LOADN wins the count.
    // A ratio separates them unanimously; nothing here computed one.
    //
    // Tolerance is 5% rather than 0 because on real bytecode AUX words collide
    // with every byte value, diluting an otherwise pure candidate. On both
    // corpora the true byte had the full 5 points of headroom.
    candidates.retain(|op, &mut (valid, _total)| {
        let all = seen.get(op).copied().unwrap_or(0);
        let typed = loadable.get(op).copied().unwrap_or(0);
        valid * 20 >= all * 19 && typed * 20 >= valid * 19
    });

    // Sort by valid count descending.
    // Primary path: `valid >= 5` (robust on medium/large protos).
    // Fallback path: `valid >= 2 && valid == total` (100% consistency) — catches
    // small protos with only a couple of LOADK uses, where every single instance
    // of the shuffled byte resolves to a valid constant index. This is strict
    // enough to avoid stealing from other AD-format opcodes (LOADN, JUMP, ADD)
    // which typically have some d-fields out of const range.
    let mut primary: Vec<_> = candidates.iter()
        .filter(|(_, &(valid, _total))| valid >= 5)
        .map(|(&op, &(valid, total))| (op, valid, total))
        .collect();
    // Deterministic: byte ascending when valid-counts tie.
    primary.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let mut assigned_something = false;
    if primary.len() == 1 {
        if ctx.try_assign_force(primary[0].0, LuauOpcode::LoadK as u8) {
            assigned_something = true;
        }
    } else if primary.len() >= 2 {
        // Multiple candidates — require the best to have a clear margin (>20% more valid hits)
        // to avoid confusion with other AD-format opcodes (LOADN, JUMP, etc.)
        let best_count = primary[0].1;
        let second_count = primary[1].1;
        if best_count > second_count + second_count / 5 {
            if ctx.try_assign_force(primary[0].0, LuauOpcode::LoadK as u8) {
                assigned_something = true;
            }
        }
    }

    // Fallback for small protos: only run if primary path didn't assign.
    if !assigned_something {
        // Tier A: 2+ hits with 100% consistency (all d in const range)
        let strict: Vec<_> = candidates.iter()
            .filter(|(_, &(valid, total))| valid >= 2 && valid == total)
            .map(|(&op, &(valid, total))| (op, valid, total))
            .collect();
        if strict.len() == 1 {
            ctx.try_assign_force(strict[0].0, LuauOpcode::LoadK as u8);
        }
    }
}

fn detect_jump(chunk: &Chunk, ctx: &mut DetectCtx) {
    // JUMP: A=0, D=forward offset. A is always 0 for unconditional JUMP.
    let mut strict_candidates: HashMap<u8, usize> = HashMap::new(); // A==0
    let mut loose_candidates: HashMap<u8, usize> = HashMap::new();  // A can be non-zero
    for proto in &chunk.protos {
        for (i, &insn) in proto.code.iter().enumerate() {
            let op = insn_op(insn);
            if ctx.is_mapped(op) { continue; }
            let a = insn_a(insn);
            let d = insn_d(insn) as i32;
            let target = (i as i32 + d) as usize;
            if d > 0 && target < proto.code.len() {
                if a == 0 {
                    *strict_candidates.entry(op).or_insert(0) += 1;
                }
                *loose_candidates.entry(op).or_insert(0) += 1;
            }
        }
    }
    // Prefer strict (A==0) candidates — the REAL Jump always has A=0
    if let Some((&op, &count)) = strict_candidates.iter()
        .filter(|(&op, _)| !ctx.is_mapped(op))
        .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
    {
        if count >= 2 {
            // Force — JUMP with A==0 and valid forward target is very reliable,
            // but frequency guard kills it in data-heavy scripts
            ctx.try_assign_force(op, LuauOpcode::Jump as u8);
        }
    }
    // Also try to detect JUMPX (E format, 24-bit signed offset)
    if ctx.find_shuffled(LuauOpcode::JumpX as u8).is_none() {
        // JUMPX: the E field is 24-bit signed. Rare but important.
        // It's distinguished from JUMP by potentially having non-zero A field
        // (since the entire 24 bits form the offset)
        //
        // CRITICAL: We must exclude candidates that look like GETTABLEKS/SETTABLEKS.
        // In data-heavy scripts, SETTABLEKS instructions (op A B [AUX=string_const_idx])
        // can accidentally satisfy the JUMPX heuristic because the AUX word (which is a
        // small constant index) looks like a valid jump target. We exclude any opcode byte
        // where the majority of its hits have a valid string constant at i+1 (= AUX pattern).
        let mut jumpx_cand: HashMap<u8, usize> = HashMap::new();
        let mut tableks_like: HashMap<u8, usize> = HashMap::new();
        for proto in &chunk.protos {
            for (i, &insn) in proto.code.iter().enumerate() {
                let op = insn_op(insn);
                if ctx.is_mapped(op) { continue; }
                // E = (insn >> 8) as i24 (signed)
                let e = (insn >> 8) as i32;
                let e_signed = if e >= (1 << 23) { e - (1 << 24) } else { e };
                let target = i as i32 + e_signed;
                if target >= 0 && (target as usize) < proto.code.len()
                    && e_signed.abs() > 127 // JUMPX is for long jumps
                {
                    *jumpx_cand.entry(op).or_insert(0) += 1;
                    // Check if i+1 has a valid string constant (AUX pattern = table op)
                    if i + 1 < proto.code.len() {
                        let aux = proto.code[i + 1];
                        if (aux as usize) < proto.constants.len() {
                            if let Some(super::types::Constant::String(_)) = proto.constants.get(aux as usize) {
                                *tableks_like.entry(op).or_insert(0) += 1;
                            }
                        }
                    }
                }
            }
        }
        // Filter out candidates where >50% of hits look like table key ops
        jumpx_cand.retain(|op, count| {
            let tableks_count = tableks_like.get(op).copied().unwrap_or(0);
            tableks_count * 2 < *count // keep only if less than half look like tableks
        });
        // Phase B0 fix: JumpX is extremely rare (long-jump escape hatch emitted
        // only when 16-bit D overflows — typically 0-5 per chunk, dozens at the
        // extreme). LOADN/LOADK and similar AD-format ops have
        //     (insn >> 8) = a | (d << 8)
        // so any op with D>=1 trivially clears the `|e| > 127` filter. Without
        // a raw-frequency cap, `count >= 2` is satisfied by a handful of
        // accidentally-in-range LOADNs out of hundreds, stealing the byte from
        // the real LoadN. Reject any candidate whose RAW chunk frequency
        // exceeds a plausible JumpX budget:
        //   - absolute cap: 20
        //   - scaled cap:   total_insns / 200 + 1 (so very large chunks still have headroom)
        // The validator revert that would catch this IS fired by
        // `validate_frequency_plausibility`, but the unconditional 3rd-pass
        // re-run of detect_jump at opmap.rs:474 undoes it, so the cap MUST
        // live inside the sublogic itself.
        let jx_max: u32 = std::cmp::max(20u32, ctx.total_insns / 200 + 1);
        // Phase B0.14 fix: cap on NOISE (raw_freq - structural_count), not raw freq alone.
        //
        // Problem with raw-freq cap (original code):
        //   raw ctx.freq includes AUX words and instruction encodings that coincidentally
        //   have the JumpX byte as their low byte. For a ~3000-word script, ~12 such
        //   words exist by chance. With 9 real JumpX instructions, freq[JumpX_byte]≈21
        //   > jx_max=20 → JumpX detection silently fails.
        //
        // Problem with structural-count-only cap (naive B0.14):
        //   LOADN-shaped filler bytes accumulate several (≈6) structural hits in small
        //   protos (they satisfy |e|>127 and valid target by accident), which is below
        //   jx_max=20. So filler bytes pass the filter and beat real JumpX (count=2) in
        //   the max_by selection.
        //
        // Correct fix — noise = raw_freq − structural_count:
        //   For real JumpX: all raw occurrences are genuine instructions, so
        //     noise = (9 genuine + 12 AUX-noise) - 9 structural = 12 ≤ jx_max ✓
        //   For LOADN-shaped filler (freq=66, structural≈6):
        //     noise = 66 - 6 = 60 > jx_max=20 → filtered ✓
        //   Also keep the structural-count cap as defense-in-depth.
        jumpx_cand.retain(|op, count| {
            let noise = ctx.freq[*op as usize].saturating_sub(*count as u32);
            (*count as u32) <= jx_max && noise <= jx_max
        });
        if let Some((&op, &count)) = jumpx_cand.iter()
            .filter(|(&op, _)| !ctx.is_mapped(op))
            .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
        {
            // count >= 1: a single structural match (valid JumpX target + |e|>127)
            // is sufficient given the low-count filter above. try_assign_force
            // bypasses the 2% rare-opcode cap which would block JumpX in small scripts.
            if count >= 1 { ctx.try_assign_force(op, LuauOpcode::JumpX as u8); }
        }
    }
}

fn detect_table_ops(chunk: &Chunk, ctx: &mut DetectCtx) {
    // GETTABLEKS/SETTABLEKS: AUX word (next instruction) is valid string constant index
    // Format: op A B C [AUX] where AUX = constant string index for the field name
    //
    // To distinguish GET vs SET, we use CONTEXT-based scoring:
    //   - SETTABLEKS often appears after a VALUE-PRODUCING instruction (DUPCLOSURE,
    //     NEWCLOSURE, MOVE, LOADK, LOADN, LOADB, GETUPVAL) targeting the SAME register
    //     as the table op's A field — because the produced value is what gets stored.
    //   - GETTABLEKS often appears with NO such precursor, or the precursor targets a
    //     different register. Also, after GETTABLEKS the A register is typically consumed.
    //
    // We score each candidate by (dup_closure_precursor_count, total_hits). The candidate
    // with the highest dup-closure-precursor ratio is SETTABLEKS; the other is GETTABLEKS.
    //
    // Pre-scan: find shuffled bytes for DUPCLOSURE, NEWCLOSURE, MOVE, LOADK, LOADN, LOADB,
    // GETUPVAL. These are the "value producers" we watch for.
    let value_producers: Vec<u8> = [
        LuauOpcode::DupClosure, LuauOpcode::NewClosure, LuauOpcode::Move,
        LuauOpcode::LoadK, LuauOpcode::LoadN, LuauOpcode::LoadB, LuauOpcode::GetUpval,
    ].iter()
        .filter_map(|o| ctx.find_shuffled(*o as u8))
        .collect();

    // Bytes we know are AUX-using (so we can detect whether a prev instruction was AUX).
    // For now we just look at direct pc-1 precursors; if that position is an AUX word from
    // an op at pc-2, this heuristic might double-count — accepted for simplicity.

    // candidates: byte -> (total_hits, settableks_indicator_hits)
    let mut candidates: HashMap<u8, (usize, usize)> = HashMap::new();
    for proto in &chunk.protos {
        for i in 0..proto.code.len().saturating_sub(1) {
            let insn = proto.code[i];
            let op = insn_op(insn);
            if ctx.is_mapped(op) { continue; }
            let a = insn_a(insn);
            let b = insn_b(insn);
            let aux = proto.code[i + 1]; // AUX word holds the constant string index
            // Validate: A and B should be valid registers, AUX should point to a string constant
            if a < proto.max_stack_size && b < proto.max_stack_size
                && (aux as usize) < proto.constants.len()
            {
                if let Some(Constant::String(_)) = proto.constants.get(aux as usize) {
                    let entry = candidates.entry(op).or_insert((0, 0));
                    entry.0 += 1;
                    // SETTABLEKS-indicator check: previous instruction is a value-producer
                    // targeting R(A) (i.e., R(A) holds the value about to be stored).
                    if i >= 1 {
                        let prev = proto.code[i - 1];
                        let prev_op = insn_op(prev);
                        let prev_a = insn_a(prev);
                        if value_producers.contains(&prev_op) && prev_a == a {
                            entry.1 += 1;
                        }
                    }
                }
            }
        }
    }
    // Score: SETTABLEKS-indicator ratio (0..100). High ratio means the byte is SETTABLEKS.
    let settableks_ratio = |(total, set_hits): (usize, usize)| -> usize {
        if total == 0 { 0 } else { set_hits * 100 / total }
    };

    // Sort candidates by total frequency, descending (byte value ascending as tiebreak)
    let mut sorted: Vec<_> = candidates.iter()
        .filter(|(&op, _)| !ctx.is_mapped(op))
        .map(|(&op, &counts)| (op, counts))
        .collect();
    sorted.sort_by(|a, b| b.1.0.cmp(&a.1.0).then_with(|| a.0.cmp(&b.0)));

    let gettableks_done = ctx.assigned[LuauOpcode::GetTableKS as u8 as usize];
    let settableks_done = ctx.assigned[LuauOpcode::SetTableKS as u8 as usize];

    if gettableks_done && !settableks_done {
        // GETTABLEKS already detected — remaining top candidate is SETTABLEKS
        if let Some(&(op, counts)) = sorted.first() {
            if counts.0 >= 2 { ctx.try_assign(op, LuauOpcode::SetTableKS as u8); }
        }
    } else if !gettableks_done && settableks_done {
        // SETTABLEKS already detected — remaining top candidate is GETTABLEKS
        if let Some(&(op, counts)) = sorted.first() {
            if counts.0 >= 3 { ctx.try_assign(op, LuauOpcode::GetTableKS as u8); }
        }
    } else if !gettableks_done && !settableks_done {
        // Neither detected — use settableks-ratio to pick. The candidate with the
        // HIGHER settableks-ratio is SETTABLEKS; the other is GETTABLEKS.
        // We consider only the top 2 candidates by frequency (both must meet minimum).
        if sorted.len() >= 2 && sorted[0].1.0 >= 3 && sorted[1].1.0 >= 2 {
            let r0 = settableks_ratio(sorted[0].1);
            let r1 = settableks_ratio(sorted[1].1);
            let (setks_op, getks_op) = if r0 > r1 {
                (sorted[0].0, sorted[1].0)
            } else {
                (sorted[1].0, sorted[0].0)
            };
            ctx.try_assign(setks_op, LuauOpcode::SetTableKS as u8);
            ctx.try_assign(getks_op, LuauOpcode::GetTableKS as u8);
            // Fallback: if one assignment failed, try the other combination
            if !ctx.assigned[LuauOpcode::GetTableKS as u8 as usize]
                && !ctx.assigned[LuauOpcode::SetTableKS as u8 as usize]
            {
                ctx.try_assign(sorted[0].0, LuauOpcode::GetTableKS as u8);
                ctx.try_assign(sorted[1].0, LuauOpcode::SetTableKS as u8);
            }
        } else if sorted.len() == 1 {
            // Single candidate: use settableks-ratio to decide
            let r = settableks_ratio(sorted[0].1);
            if sorted[0].1.0 >= 3 {
                // High SET-indicator ratio (>20%) → SETTABLEKS
                if r >= 20 {
                    ctx.try_assign(sorted[0].0, LuauOpcode::SetTableKS as u8);
                } else if !ctx.try_assign(sorted[0].0, LuauOpcode::GetTableKS as u8) {
                    ctx.try_assign(sorted[0].0, LuauOpcode::SetTableKS as u8);
                }
            } else if sorted[0].1.0 >= 2 {
                ctx.try_assign(sorted[0].0, LuauOpcode::SetTableKS as u8);
            }
        }
    }

    // DUPTABLE: D is constant index pointing to Table constant
    // Must be followed by SETTABLEKS to fill in the table fields
    let settableks_op = ctx.find_shuffled(LuauOpcode::SetTableKS as u8);
    if let Some(stks_op) = settableks_op {
        let mut dt_cand: HashMap<u8, usize> = HashMap::new();
        for proto in &chunk.protos {
            for i in 0..proto.code.len().saturating_sub(2) {
                let insn = proto.code[i];
                let op = insn_op(insn);
                if ctx.is_mapped(op) { continue; }
                let a = insn_a(insn);
                let d = insn_d(insn);
                if d >= 0 && a < proto.max_stack_size {
                    if let Some(Constant::Table(entries)) = proto.constants.get(d as usize) {
                        if entries.len() >= 2 {
                            // Cross-validate: next instruction should be SETTABLEKS targeting same register
                            let next_op = insn_op(proto.code[i + 1]);
                            let next_b = insn_b(proto.code[i + 1]);
                            if next_op == stks_op && next_b == a {
                                *dt_cand.entry(op).or_insert(0) += 1;
                            }
                        }
                    }
                }
            }
        }
        if let Some((&op, &count)) = dt_cand.iter()
            .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
        {
            if count >= 2 { ctx.try_assign(op, LuauOpcode::DupTable as u8); }
        }
    }
}

fn detect_conditional_jumps(chunk: &Chunk, ctx: &mut DetectCtx) {
    let mut candidates: HashMap<u8, usize> = HashMap::new();
    // Purity bookkeeping. The conforming-occurrence count below cannot expose a
    // non-jump: occurrences that fail the shape test are dropped rather than
    // counted against the byte, so a byte that behaves like a jump a tenth of the
    // time scores exactly like one that always does.
    let mut seen: HashMap<u8, usize> = HashMap::new();
    let mut jumplike: HashMap<u8, usize> = HashMap::new();
    for proto in &chunk.protos {
        for (i, &insn) in proto.code.iter().enumerate() {
            let op = insn_op(insn);
            if ctx.is_mapped(op) { continue; }
            let a = insn_a(insn);
            let d = insn_d(insn) as i32;
            let target = i as i32 + d;
            // A > 0 distinguishes JumpIf/JumpIfNot from JUMP (which has A=0)
            if a > 0 && a < proto.max_stack_size && d > 0 && target >= 0 && (target as usize) < proto.code.len() {
                *candidates.entry(op).or_insert(0) += 1;
            }

            *seen.entry(op).or_insert(0) += 1;
            // The invariant: a real branch never leaves its proto. Under VM
            // semantics it lands at pc + D + 1.
            //
            // Note what is deliberately NOT tested here. "D != 0" is an equally
            // clean separator on synthetic corpora — the compiler never emits a
            // zero-displacement jump — but it misfires badly on real bytecode:
            // an AUX data word whose upper 16 bits are zero decodes as D == 0, so
            // the test marks every such word against whichever byte its low octet
            // happens to match. On a 314-proto Roblox module that alone pushed the
            // true JUMPIFNOT byte below the purity floor and cost 105 extra
            // unresolved instructions. The range test is far less AUX-sensitive:
            // a small AUX value lands harmlessly inside the proto.
            let landing = i as i32 + d + 1;
            if landing >= 0 && landing <= proto.code.len() as i32 {
                *jumplike.entry(op).or_insert(0) += 1;
            }
        }
    }
    // Purity veto.
    //
    // Measured over the 47-program corpus: the true JUMPIF, JUMPIFNOT, JUMP and
    // JUMPBACK bytes satisfy both invariants in 100% of their occurrences. The
    // true LOADN byte fails them in 19% (11% D==0, 10% target outside the proto,
    // because its D is an integer literal rather than a displacement) and the
    // true LOADK byte in 15%. Ranking on absolute conforming counts hands
    // JUMPIFNOT to LOADN regardless, because LOADN is by far the more frequent
    // opcode — 587 occurrences against 5 across the whole corpus.
    //
    // 5% slack absorbs AUX words that happen to carry the candidate's byte value.
    candidates.retain(|op, _| {
        let all = seen.get(op).copied().unwrap_or(0);
        let ok = jumplike.get(op).copied().unwrap_or(0);
        ok * 20 >= all * 19
    });
    // Phase B0.1 fix: reject LOADN-shape false positives via a raw-frequency
    // cap. LOADN instructions are AD-format with A = register (a>0 trivially
    // satisfied, a<max_stack always true) and D = signed literal number
    // (d>0 for positive literals, target = pc+d often in-range for late pc
    // and small d). The existing filter can't discriminate LOADN from
    // JumpIfNot on structure alone — both trivially pass.
    //
    // Empirical JumpIfNot frequency in Roblox bytecode is 0.3-2% of total
    // instructions. On ModuleScript.luac, the real LOADN byte 0x8C had raw
    // frequency 790/11219 = 7.04%, roughly 10x the next candidate (0.94%).
    // A cap at 5% of total (with absolute floor 20 for tiny chunks) cleanly
    // rejects LOADN-shape while preserving real JumpIfNot dominance.
    //
    // This mirrors Phase B0 Patch 4a in `detect_jump`'s JumpX sublogic
    // (same structural class of bug, sibling detector). The validator
    // `validate_frequency_plausibility` does NOT cap JumpIfNot/JumpIf
    // (they're expected-common ops), and the 3rd-pass unconditional re-run
    // at opmap.rs:482 would re-claim the byte even if it did — so the cap
    // MUST live inside `detect_conditional_jumps` itself.
    let cj_freq_cap: u32 = std::cmp::max(20u32, ctx.total_insns / 20);
    candidates.retain(|op, _| ctx.freq[*op as usize] <= cj_freq_cap);
    let mut sorted: Vec<_> = candidates.iter()
        .filter(|(&op, _)| !ctx.is_mapped(op))
        .collect();
    // Deterministic: byte ascending when counts tie (sorted holds &(&u8, &usize)).
    sorted.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    if sorted.len() >= 2 {
        // Force — conditional jump pattern (A>0, forward target) is reliable
        if *sorted[0].1 >= 5 { ctx.try_assign_force(*sorted[0].0, LuauOpcode::JumpIfNot as u8); }
        if *sorted[1].1 >= 3 { ctx.try_assign_force(*sorted[1].0, LuauOpcode::JumpIf as u8); }
    } else if sorted.len() == 1 {
        // Only one candidate — try as JumpIfNot first (more common), fallback to JumpIf
        if *sorted[0].1 >= 3 {
            if !ctx.try_assign_force(*sorted[0].0, LuauOpcode::JumpIfNot as u8) {
                ctx.try_assign_force(*sorted[0].0, LuauOpcode::JumpIf as u8);
            }
        }
    }
}

fn detect_upvalue_ops(chunk: &Chunk, ctx: &mut DetectCtx) {
    // GETUPVAL: A=target, B=upvalue_index, C=0. Only valid in protos with upvalues.
    // SETUPVAL: A=source, B=upvalue_index, C=0. Only valid in protos with upvalues.
    // Key: B must be < proto.num_upvalues AND C=0.
    //
    // To distinguish from unary ops (also C=0, B<max_stack), we cross-validate:
    // The candidate must appear PREDOMINANTLY in protos that have upvalues,
    // and B must always be < num_upvalues for that proto.
    let mut upval_hits: HashMap<u8, usize> = HashMap::new(); // appearances in upval protos
    let mut non_upval_hits: HashMap<u8, usize> = HashMap::new(); // appearances in non-upval protos

    for proto in &chunk.protos {
        for &insn in &proto.code {
            let op = insn_op(insn);
            if ctx.is_mapped(op) { continue; }
            let b = insn_b(insn);
            let c = insn_c(insn);
            if c == 0 && insn_a(insn) < proto.max_stack_size {
                if proto.num_upvalues > 0 && b < proto.num_upvalues {
                    *upval_hits.entry(op).or_insert(0) += 1;
                } else if proto.num_upvalues == 0 {
                    *non_upval_hits.entry(op).or_insert(0) += 1;
                }
            }
        }
    }

    // The real GETUPVAL/SETUPVAL should have HIGH upval_hits and LOW non_upval_hits
    // Unary ops have hits in BOTH upval and non-upval protos
    let mut candidates: Vec<(u8, usize, f64)> = upval_hits.iter()
        .filter(|(&op, &count)| !ctx.is_mapped(op) && count >= 3)
        .map(|(&op, &count)| {
            let non_hits = *non_upval_hits.get(&op).unwrap_or(&0);
            let ratio = if non_hits == 0 { count as f64 } else { count as f64 / non_hits as f64 };
            (op, count, ratio)
        })
        .filter(|(_, _, ratio)| *ratio >= 2.0) // Must be at least 2x more common in upval protos
        .collect();

    // Sort by total hits, byte ascending as deterministic tiebreak.
    candidates.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    if candidates.len() >= 2 {
        if candidates[0].1 >= 5 { ctx.try_assign(candidates[0].0, LuauOpcode::GetUpval as u8); }
        if candidates[1].1 >= 3 { ctx.try_assign(candidates[1].0, LuauOpcode::SetUpval as u8); }
    } else if candidates.len() == 1 && candidates[0].1 >= 5 {
        ctx.try_assign(candidates[0].0, LuauOpcode::GetUpval as u8);
    }
}

// ═══════════════════════════════════════════════════════════════
// TIER 4: Pattern-based detection
// ═══════════════════════════════════════════════════════════════

/// GETGLOBAL/SETGLOBAL: AD format with D typically 0, AUX is index into proto.constants
/// Per the Luau VM (lvmexecute.cpp), AUX indexes into the constant table (proto->k),
/// where the constant must be a String.
fn detect_global_ops(chunk: &Chunk, ctx: &mut DetectCtx) {
    // GETGLOBAL/SETGLOBAL: AD format. D=0 (unused), AUX is index into proto.constants
    // pointing to a String constant (the global name).
    //
    // NEWTABLE-exclusion: if an instruction is followed by SETTABLEKS/SETLIST/SETTABLEN
    // filling R(A), it's NEWTABLE not GETGLOBAL. Also reject if the candidate has strong
    // NEWTABLE signal at proto-start positions (where NEWTABLE R0 is canonical).
    //
    // This detector uses an AUX-aware walk to avoid counting AUX words as candidates,
    // and requires the candidate's "AUX" to point to a String constant PLUS a follow-up
    // call/namecall using R(A) as the object.
    let settableks_op = ctx.find_shuffled(LuauOpcode::SetTableKS as u8);
    let setlist_op = ctx.find_shuffled(LuauOpcode::SetList as u8);
    let settablen_op = ctx.find_shuffled(LuauOpcode::SetTableN as u8);
    let call_op = ctx.find_shuffled(LuauOpcode::Call as u8);
    let namecall_op = ctx.find_shuffled(LuauOpcode::NameCall as u8);
    let newtable_op = ctx.find_shuffled(LuauOpcode::NewTable as u8);

    // Track per-candidate: (total_hits, newtable_like_hits, call_like_hits)
    let mut candidates: HashMap<u8, (usize, usize, usize)> = HashMap::new();
    for proto in &chunk.protos {
        let code = &proto.code;
        // AUX-aware walk: skip AUX words of already-mapped AUX ops
        let mut i = 0usize;
        while i < code.len().saturating_sub(1) {
            let insn = code[i];
            let op = insn_op(insn);
            let mapped = ctx.map[op as usize];
            if mapped != 255 {
                let standard_op = LuauOpcode::from_u8(mapped);
                if standard_op.has_aux() && i + 1 < code.len() { i += 2; } else { i += 1; }
                continue;
            }
            let aux = code[i + 1];
            let a = insn_a(insn);
            let b = insn_b(insn);
            let c = insn_c(insn);
            // GETGLOBAL/SETGLOBAL: AD format with D=0 (unused).
            // In ABC reading: B=0, C=0 ALWAYS (since D=0 packs into bits 16-31 as 0).
            // This rejects OR-K (ORK) instructions which have B=left_reg and C=const_idx
            // (both typically non-zero) from being falsely detected as GETGLOBAL/SETGLOBAL.
            // Also rejects SETTABLEKS (B=table_reg) when table is in non-zero register.
            //
            // Note: some ORK instructions may have C=0 (or-ing with constant at index 0),
            // but B is still a non-zero source register in most cases. The filter below
            // catches the overwhelming majority while correctly accepting GETGLOBAL/SETGLOBAL.
            if a < proto.max_stack_size
                && b == 0 && c == 0  // D=0 validation: GETGLOBAL/SETGLOBAL always have D=0
                && (aux as usize) < proto.constants.len()
                && matches!(proto.constants.get(aux as usize), Some(Constant::String(_)))
            {
                // Scan forward for NEWTABLE-fill pattern or call-like pattern
                let mut newtable_like = false;
                let mut call_like = false;
                let scan_end = code.len().min(i + 12);
                for j in (i + 2)..scan_end {
                    let fop = insn_op(code[j]);
                    let fa = insn_a(code[j]);
                    let fb = insn_b(code[j]);
                    // NEWTABLE fill: SETTABLEKS/SETLIST/SETTABLEN with B == R(A)
                    if (Some(fop) == settableks_op
                        || Some(fop) == setlist_op
                        || Some(fop) == settablen_op)
                        && fb == a
                    {
                        newtable_like = true;
                        break;
                    }
                    // GETGLOBAL call: NAMECALL/CALL with A==our_A (object = loaded global)
                    if (Some(fop) == namecall_op || Some(fop) == call_op) && fa == a {
                        call_like = true;
                        break;
                    }
                }
                // Also check WHOLE-PROTO for fills of R(A) — the canonical NEWTABLE
                // module pattern puts fills 50+ instructions away from the creation.
                if !newtable_like && (i <= 1) {
                    // Only check the proto-start case to avoid O(N^2) cost
                    let mut whole_proto_fills = 0usize;
                    for j in (i + 2)..code.len() {
                        let fop = insn_op(code[j]);
                        let fb = insn_b(code[j]);
                        if (Some(fop) == settableks_op
                            || Some(fop) == setlist_op
                            || Some(fop) == settablen_op)
                            && fb == a
                        {
                            whole_proto_fills += 1;
                            if whole_proto_fills >= 3 {
                                newtable_like = true;
                                break;
                            }
                        }
                    }
                }
                let entry = candidates.entry(op).or_insert((0, 0, 0));
                entry.0 += 1;
                if newtable_like { entry.1 += 1; }
                if call_like { entry.2 += 1; }
            }
            i += 1;
        }
    }
    // If NEWTABLE is already assigned, exclude that byte explicitly.
    // Score each candidate: prefer those with HIGH call-like hits and LOW newtable-like hits.
    let mut sorted: Vec<_> = candidates.iter()
        .filter(|(&op, _)| !ctx.is_mapped(op))
        .filter(|(&op, _)| Some(op) != newtable_op)
        .filter(|(_, (_total, newtable_like, call_like))| {
            // Must have more call-like hits than newtable-like hits (else it's NEWTABLE)
            call_like > newtable_like || (*call_like == 0 && *newtable_like == 0)
        })
        .collect();
    // Sort by call-like descending, then total descending, then byte ascending (determinism).
    sorted.sort_by(|a, b| {
        let ((_ia, (_ta, _na, ca)), (_ib, (_tb, _nb, cb))) = (a, b);
        cb.cmp(ca)
            .then_with(|| b.1.0.cmp(&a.1.0))
            .then_with(|| a.0.cmp(b.0))
    });
    // The two most frequent are likely GETGLOBAL (more common) and SETGLOBAL
    if sorted.len() >= 2 {
        if sorted[0].1.0 >= 5 { ctx.try_assign(*sorted[0].0, LuauOpcode::GetGlobal as u8); }
        if sorted[1].1.0 >= 3 { ctx.try_assign(*sorted[1].0, LuauOpcode::SetGlobal as u8); }
    } else if sorted.len() == 1 && sorted[0].1.0 >= 5 {
        ctx.try_assign(*sorted[0].0, LuauOpcode::GetGlobal as u8);
    }
}

/// FASTCALL1: A = builtin id (0-112), B = arg register, C = jump offset to CALL
/// Unlike FASTCALL (B=0, no arg in instruction) and FASTCALL2/2K/3 (which have AUX),
/// FASTCALL1 has B = valid register and NO AUX word.
fn detect_fastcall1(chunk: &Chunk, ctx: &mut DetectCtx) {
    let call_shuffled = match ctx.find_shuffled(LuauOpcode::Call as u8) {
        Some(op) => op,
        None => return,
    };
    // Track per-candidate: (total_hits, b_nonzero_hits)
    // B=0 matches are ambiguous with FASTCALL, so we count them separately
    let mut candidates: HashMap<u8, (usize, usize)> = HashMap::new();
    for proto in &chunk.protos {
        for i in 0..proto.code.len() {
            let insn = proto.code[i];
            let op = insn_op(insn);
            if ctx.is_mapped(op) { continue; }
            let a = insn_a(insn) as usize; // builtin id
            let b = insn_b(insn);           // arg register
            let c = insn_c(insn) as usize; // jump offset to skip to CALL
            // FASTCALL1: A=builtin (0-112), B=arg register (<maxstack), C=jump offset>0
            if a <= 112 && c > 0 && (b as u8) < proto.max_stack_size {
                // The CALL should be at pc + c + 1
                let call_pc = i + c + 1;
                if call_pc < proto.code.len() && insn_op(proto.code[call_pc]) == call_shuffled {
                    let entry = candidates.entry(op).or_insert((0, 0));
                    entry.0 += 1;
                    if b > 0 { entry.1 += 1; }
                }
            }
        }
    }
    // Prefer the candidate that has the most B>0 instances (distinguishes from FASTCALL
    // which always has B=0). Fall back to total count if B>0 counts are tied.
    if let Some((&op, &(total, b_nonzero))) = candidates.iter()
        .filter(|(_, &(total, _))| total >= 2)
        .max_by(|a, b| {
            let (at, ab) = a.1;
            let (bt, bb) = b.1;
            (ab, at).cmp(&(bb, bt)).then_with(|| b.0.cmp(a.0))
        })
    {
        // Only assign if we have evidence of B>0 (otherwise it is likely FASTCALL)
        if b_nonzero >= 1 || total >= 3 {
            ctx.try_assign(op, LuauOpcode::FastCall1 as u8);
        }
    }
}

/// FASTCALL2: A = builtin id, B = arg1 register, C = jump offset; AUX = arg2 register
/// Distinguished from FASTCALL1 by having an AUX word (B and AUX are both valid registers).
fn detect_fastcall2(chunk: &Chunk, ctx: &mut DetectCtx) {
    let call_shuffled = match ctx.find_shuffled(LuauOpcode::Call as u8) {
        Some(op) => op,
        None => return,
    };
    let mut candidates: HashMap<u8, usize> = HashMap::new();
    for proto in &chunk.protos {
        let ms = proto.max_stack_size as usize;
        for i in 0..proto.code.len().saturating_sub(1) {
            let insn = proto.code[i];
            let op = insn_op(insn);
            if ctx.is_mapped(op) { continue; }
            let a = insn_a(insn) as usize;
            let b = insn_b(insn) as usize;
            let c = insn_c(insn) as usize;
            let aux = proto.code[i + 1];
            // FASTCALL2: A=builtin (<=127), B=arg1 reg (<maxstack), C=jump>0
            // AUX = arg2 register (low byte < maxstack, and the full AUX should be small
            // since high bytes should be 0 for a simple register index)
            if a <= 127 && b < ms && c > 0
                && (aux & 0xFF) < (ms as u32)
                && (aux >> 8) == 0  // AUX should be just a register index, no high bits
            {
                // CALL should be at pc + c + 1
                let call_pc = i + c + 1;
                if call_pc < proto.code.len() && insn_op(proto.code[call_pc]) == call_shuffled {
                    *candidates.entry(op).or_insert(0) += 1;
                }
            }
        }
    }
    if let Some((&op, &count)) = candidates.iter()
        .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
    {
        if count >= 2 { ctx.try_assign(op, LuauOpcode::FastCall2 as u8); }
    }
}

/// NEWTABLE: has AUX word. B = log2 hash-size hint (0-15), AUX = array-size hint.
/// Should be followed by table-filling instructions (SETTABLEKS, SETLIST, SETTABLEN)
/// that use R(A) as their target table.
///
/// Algorithm (rewritten 2026-04-10): FORWARD SCAN for fills, with strict shape.
///   1. For each unmapped op with (c==0, a<max_stack, b<=15) — the hard NEWTABLE
///      shape constraints — forward-walk the remainder of the proto AUX-aware
///      and count SETTABLEKS/SETLIST/SETTABLEN instructions with B==A.
///   2. Score by:
///      - distinct protos (real NEWTABLE appears across many functions)
///      - total fills credited (stronger fills = stronger evidence)
///      - nonempty hints (b>0 or aux>0 = the script hinted at a size)
///   3. Require: winner has >= 20 total instances AND distinct_protos >= 5
///      OR >= 2 fills found (to catch rare but definitively-filled patterns).
///
/// Rejects AD-format jumps via the `b <= 15` filter: JumpIfEq/JumpIfNot have
/// AUX words whose low byte (interpreted as B) is usually > 15.
/// Detect JUMP (canonical 23) as "an unconditional forward jump whose
/// fallthrough is not dead".
///
/// `detect_jump` ranks candidates by how many `A == 0`, forward, in-range
/// instances they have and force-assigns the winner once any byte reaches two.
/// In a small chunk JUMP occurs once or twice and shares that shape with
/// FORNPREP and with a JUMPIF on register 0, so the count-based winner is often
/// the wrong byte — and a wrong assignment costs two slots.
///
/// This adds a purity-gated path for exactly that case. A byte qualifies only if
/// EVERY one of its instruction-position occurrences is an `A == 0` forward
/// in-range jump AND the word immediately after it is the landing site of some
/// OTHER branch. That last clause is the discriminating one: the Luau compiler
/// emits no unreachable code, so the instruction following an unconditional jump
/// must be reachable from elsewhere. Nothing else in the ISA has to satisfy it —
/// it is what separates JUMP from an ADDK or a conditional branch that happens
/// to have `A == 0`.
///
/// Assigns only when EXACTLY ONE byte survives, and only when `detect_jump`
/// found nothing, so it can never displace an existing detection.
fn detect_jump_unconditional_forward(chunk: &Chunk, ctx: &mut DetectCtx) {
    if ctx.find_shuffled(LuauOpcode::Jump as u8).is_some() {
        return;
    }

    let mut seen = [false; 256];
    let mut pure = [true; 256];

    for proto in &chunk.protos {
        let code = &proto.code;
        let n = code.len();
        // How many words branch to each position. Counted over EVERY word,
        // including AUX data, which can only ADD phantom landings — that makes
        // the no-dead-fallthrough test easier to pass, never harder, so the bias
        // is toward recall rather than toward a false positive.
        let mut landings = vec![0u32; n + 2];
        for (j, &w) in code.iter().enumerate() {
            let t = j as i64 + insn_d(w) as i64 + 1;
            if t >= 0 && (t as usize) < landings.len() {
                landings[t as usize] += 1;
            }
        }

        let mut i = 0usize;
        while i < n {
            let insn = code[i];
            let op = insn_op(insn);
            if ctx.is_mapped(op) {
                let std_op = LuauOpcode::from_u8(ctx.map[op as usize]);
                if std_op.has_aux() && i + 1 < n { i += 2; } else { i += 1; }
                continue;
            }
            seen[op as usize] = true;
            let d = insn_d(insn) as i64;
            let t = i as i64 + d + 1;
            if insn_a(insn) != 0 || d <= 0 || t < 0 || t as usize > n {
                pure[op as usize] = false;
                i += 1;
                continue;
            }
            // No dead fallthrough. `d > 0` means the target is strictly past the
            // fallthrough, so this instruction can never be its own lander and no
            // self-correction is needed. A jump in the final slot has no
            // fallthrough to check.
            let fall = i + 1;
            if fall < n && landings[fall] == 0 {
                pure[op as usize] = false;
            }
            i += 1;
        }
    }

    let survivors: Vec<u8> = (0..=255u8)
        .filter(|&b| seen[b as usize] && pure[b as usize])
        .collect();
    if survivors.len() == 1 {
        ctx.try_assign(survivors[0], LuauOpcode::Jump as u8);
    }
}

/// Detect CLOSEUPVALS (canonical 11) from the one property in the `C == 0`
/// family that is not a pure shape test: it can only appear in a proto that
/// actually creates a closure capturing a local BY REFERENCE.
///
/// MOVE, GETUPVAL, SETUPVAL, NOT, MINUS, LENGTH, LOADNIL, PREPVARARGS and
/// CLOSEUPVALS are all ABC-format with `C == 0`, so shape alone cannot separate
/// them and `detect_closeupvals` (which only asks for `B == 0 && C == 0`) picks
/// by raw count out of a badly degenerate pool. Three additional constraints
/// make the byte identifiable without needing any of its neighbours mapped:
///
///   * ENCODING PURITY — every occurrence must have `B == 0`, `C == 0` and a
///     valid `A`. One occurrence with a nonzero B or C disqualifies the byte.
///   * SCOPE PURITY — every occurrence must sit in a proto that has at least one
///     child proto with upvalues. This is read straight out of the container, so
///     unlike the rest of this family it has no bootstrap dependency at all.
///   * ANCHOR — at least one occurrence is immediately followed by a scope
///     terminator (RETURN / JUMPBACK / FORNLOOP / FORGLOOP), which is where the
///     compiler closes upvalues.
///
/// The anchor is deliberately EXISTENTIAL, not universal: the v6 compiler emits
/// a second CLOSEUPVALS on the loop-exit path of a `repeat ... until` that
/// captures by reference, and that one is followed by ordinary code.
///
/// The anchor also rejects an occurrence whose PRECEDING word is a plausible
/// in-proto AD jump. A comparison jump's AUX word is the right-hand register
/// index, so it decodes as `A=reg, B=0, C=0` — a perfect CLOSEUPVALS impostor —
/// and the comparison-jump family is frequently unmapped at this point, so those
/// AUX words are not skipped by the walk.
///
/// Assigns only when EXACTLY ONE byte survives; two or more survivors means the
/// evidence does not identify a byte and nothing is assigned.
fn detect_closeupvals_ref_scope(chunk: &Chunk, ctx: &mut DetectCtx) {
    if ctx.assigned[LuauOpcode::CloseUpvals as usize] {
        return;
    }

    // Which protos create a closure that captures something by reference?
    let has_capturing_child: Vec<bool> = chunk.protos.iter()
        .map(|p| p.child_protos.iter().any(|&c| {
            chunk.protos.get(c as usize).map(|cp| cp.num_upvalues > 0).unwrap_or(false)
        }))
        .collect();

    // byte -> (all occurrences pass encoding+scope purity, has an anchored site)
    let mut pure = [true; 256];
    let mut seen = [false; 256];
    let mut anchored = [false; 256];

    for (pi, proto) in chunk.protos.iter().enumerate() {
        let code = &proto.code;
        let ms = proto.max_stack_size;
        let scope_ok = has_capturing_child.get(pi).copied().unwrap_or(false);
        let mut i = 0usize;
        while i < code.len() {
            let insn = code[i];
            let op = insn_op(insn);
            if ctx.is_mapped(op) {
                let std_op = LuauOpcode::from_u8(ctx.map[op as usize]);
                if std_op.has_aux() && i + 1 < code.len() { i += 2; } else { i += 1; }
                continue;
            }
            seen[op as usize] = true;
            if insn_b(insn) != 0 || insn_c(insn) != 0 || insn_a(insn) >= ms || !scope_ok {
                pure[op as usize] = false;
                i += 1;
                continue;
            }
            // Anchor: the next word must be a scope terminator we already know.
            if !anchored[op as usize] && i + 1 < code.len() {
                let next_std = ctx.map[insn_op(code[i + 1]) as usize];
                let is_terminator = next_std == LuauOpcode::Return as u8
                    || next_std == LuauOpcode::JumpBack as u8
                    || next_std == LuauOpcode::ForNLoop as u8
                    || next_std == LuauOpcode::ForGLoop as u8;
                // Anti-AUX guard: reject if the PREVIOUS word could be an AD jump
                // whose AUX word we are standing on.
                let prev_is_jump = i >= 1 && {
                    let d = insn_d(code[i - 1]) as i32;
                    let t = (i as i32 - 1) + d + 1;
                    d != 0 && t >= 0 && t as usize <= code.len()
                };
                if is_terminator && !prev_is_jump {
                    anchored[op as usize] = true;
                }
            }
            i += 1;
        }
    }

    let survivors: Vec<u8> = (0..=255u8)
        .filter(|&b| seen[b as usize] && pure[b as usize] && anchored[b as usize])
        .collect();
    if survivors.len() == 1 {
        ctx.try_assign(survivors[0], LuauOpcode::CloseUpvals as u8);
    }
}

fn detect_newtable(chunk: &Chunk, ctx: &mut DetectCtx) {
    let settableks_op = ctx.find_shuffled(LuauOpcode::SetTableKS as u8);
    let setlist_op = ctx.find_shuffled(LuauOpcode::SetList as u8);
    let settablen_op = ctx.find_shuffled(LuauOpcode::SetTableN as u8);

    #[derive(Default)]
    struct Cand {
        total: usize,              // raw count of instructions matching shape
        with_fills: usize,         // number of candidate sites with ≥1 fill
        total_fills: usize,        // sum of fills across all sites
        nonempty_hints: usize,     // sites where b>0 or aux>0
        strict_hint_sites: usize,  // sites where b>0 (b=log2 hash hint — "real" hint)
        proto_start: usize,        // sites where pc<=1 (proto-start module pattern)
        first_in_proto_sites: usize, // sites that are the first unmapped candidate in their proto
        distinct_protos: std::collections::HashSet<usize>,
    }
    let mut candidates: HashMap<u8, Cand> = HashMap::new();

    for (pi, proto) in chunk.protos.iter().enumerate() {
        let code = &proto.code;
        let ms = proto.max_stack_size;
        let mut i = 0usize;
        // Track whether any candidate site has been encountered yet in this proto.
        // The FIRST candidate site — after skipping mapped prelude — gets a bonus
        // that makes Path C (single-proto strict-hint) fire for module patterns
        // where NEWTABLE may be preceded by a mapped NAMECALL/GETIMPORT/etc.
        let mut first_candidate_seen = false;
        while i < code.len() {
            let insn = code[i];
            let op = insn_op(insn);
            let mapped = ctx.map[op as usize];
            if mapped != 255 {
                // Skip AUX for mapped ops (AUX-aware walk).
                let std_op = LuauOpcode::from_u8(mapped);
                if std_op.has_aux() && i + 1 < code.len() { i += 2; } else { i += 1; }
                continue;
            }
            let a = insn_a(insn);
            let b = insn_b(insn);
            let c = insn_c(insn);
            // Strict NEWTABLE shape:
            //   - c == 0 (always zero for NEWTABLE)
            //   - a < max_stack (valid register)
            //   - b <= 15 (log2 hash hint, real NEWTABLE is always 0-15)
            if c != 0 || a >= ms || b > 15 {
                i += 1;
                continue;
            }
            let aux = if i + 1 < code.len() { code[i + 1] } else { 0 };
            // AUX (array-size hint) should also be small for typical tables.
            // Accept up to 65535 (64K entries — generous upper bound).
            if aux > 65535 {
                i += 1;
                continue;
            }

            let entry = candidates.entry(op).or_default();
            entry.total += 1;
            entry.distinct_protos.insert(pi);
            if b > 0 || aux > 0 { entry.nonempty_hints += 1; }
            if b > 0 { entry.strict_hint_sites += 1; }
            if i <= 1 { entry.proto_start += 1; }
            if !first_candidate_seen {
                entry.first_in_proto_sites += 1;
                first_candidate_seen = true;
            }

            // Forward-scan the rest of this proto (AUX-aware) for fills targeting R(a).
            //
            // Encoding note: different fill ops put the target-table register in
            // different fields.
            //   SETTABLEKS: R(B)[K(AUX)] = R(A)  → table is in B
            //   SETTABLEN:  R(B)[C+1]   = R(A)  → table is in B
            //   SETLIST:    R(A)[c+AUX] = R(B)..  → table is in A
            // Historically this loop checked `insn_b == a` for all three, which
            // silently dropped every SETLIST fill. For module tables that use
            // SETLIST to bulk-fill array sections, that caused with_fills to be 0
            // and the candidate to be rejected.
            let mut fills_here = 0usize;
            let mut j = i + 2; // skip our own AUX word
            while j < code.len() {
                let fop = insn_op(code[j]);
                let fmapped = ctx.map[fop as usize];
                let table_reg_matches = if Some(fop) == setlist_op {
                    insn_a(code[j]) == a
                } else if Some(fop) == settableks_op || Some(fop) == settablen_op {
                    insn_b(code[j]) == a
                } else {
                    false
                };
                if table_reg_matches {
                    fills_here += 1;
                    if fills_here >= 32 { break; }
                }
                if fmapped != 255 {
                    let s = LuauOpcode::from_u8(fmapped);
                    if s.has_aux() && j + 1 < code.len() { j += 2; } else { j += 1; }
                } else {
                    j += 1;
                }
            }
            if fills_here > 0 {
                entry.with_fills += 1;
                entry.total_fills += fills_here;
            }

            // Step past our AUX word so we don't count our own AUX as another insn.
            i += 2;
        }
    }

    // Rank candidates by a composite score that favors the real NEWTABLE byte.
    //
    // Weighting rationale:
    //   - with_fills * 100: having any fill is the STRONGEST signal; no-fill
    //     candidates are near-zero noise even if frequent
    //   - total_fills * 20: multiple fills reinforce the structural claim
    //   - distinct_protos * 10: NEWTABLE is used across many functions
    //   - nonempty_hints * 2: mild tiebreaker
    //   - proto_start * 5: module pattern bonus
    //
    // Candidates with with_fills == 0 are HARD-EXCLUDED from ranking — no matter
    // how often the byte appears, if nothing ever fills the "table" it allocates,
    // it is not a table-creator. AD jumps with b > 15 are already excluded by
    // the shape filter above.
    let score_fn = |c: &Cand| -> usize {
        c.with_fills * 100
            + c.total_fills * 20
            + c.distinct_protos.len() * 10
            + c.nonempty_hints * 2
            + c.proto_start * 5
    };

    let mut ranked: Vec<(u8, usize, &Cand)> = candidates.iter()
        .filter(|(&op, c)| !ctx.is_mapped(op) && c.with_fills >= 1)
        .map(|(&op, c)| (op, score_fn(c), c))
        .collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    // Winner selection: prefer UNMAPPED over WRONG. Wrong assignments poison the
    // cache. A candidate is a "passer" only when at least one of these is true:
    //
    //   Path A — Cross-proto fill: the byte has at least one confirmed write-
    //     then-fill chain AND the byte appears as a NEWTABLE shape in at least
    //     3 distinct protos. Real NEWTABLE is used across many functions; pure
    //     noise/coincidence won't span 3+ protos while also having a fill.
    //     distinct_protos >= 3 && with_fills >= 1
    //
    //   Path B — Dominance: the byte is widely used across protos AND has at
    //     least one confirmed fill chain. This is the large-module fallback.
    //     distinct_protos >= 8 && total >= 10 && with_fills >= 1
    //
    //   Path C — Isolated-module strict hint: this byte is the first candidate
    //     in its proto AND has a real b>0 log2 hash hint AND has ≥3 fills of
    //     its A register. Catches single-file module tables where dominance is
    //     impossible (only one proto) but the shape is unambiguous. All three
    //     constraints stacked together (first-in-proto + b>0 + 3 fills) are
    //     strong enough to rule out coincidental matches even though any one
    //     of them is weak in isolation.
    //     first_in_proto_sites >= 1 && strict_hint_sites >= 1
    //     && with_fills >= 1 && total_fills >= 3
    //
    // Acceptance rules:
    //   1. Iterate ranked, skipping candidates that fail ALL paths.
    //   2. The first passer is the winner — but only if its score is ≥ 2× the
    //      next passer (clear_margin) and no instance of this byte appears as
    //      an AD backward jump (d < 0), which would indicate JumpBack/ForGLoop
    //      /ForNLoop masquerading as NEWTABLE.
    //
    // This "rank then filter" order is important: the HIGHEST-SCORING passer
    // wins, not the highest raw candidate. Noise candidates (like 0x00 in a
    // script with incidental SETTABLEKS alignments) may score higher than real
    // NEWTABLE but can still be rejected because they lack a strict hint.
    let passes_any_path = |c: &Cand| -> bool {
        let distinct = c.distinct_protos.len();
        let path_a = c.with_fills >= 1 && distinct >= 3;
        let path_b = distinct >= 8 && c.total >= 10 && c.with_fills >= 1;
        let path_c = c.first_in_proto_sites >= 1 && c.strict_hint_sites >= 1
            && c.with_fills >= 1 && c.total_fills >= 3;
        path_a || path_b || path_c
    };

    let passers: Vec<(u8, usize, &Cand)> = ranked.iter()
        .filter(|&&(_, _, c)| passes_any_path(c))
        .copied()
        .collect();

    if let Some(&(op, score, _cand)) = passers.first() {
        let clear_margin = passers.get(1).map(|&(_, s, _)| score >= s * 2).unwrap_or(true);

        // Cross-check: verify this byte never appears as an AD backward jump
        // (signature of JumpBack/ForGLoop/ForNLoop). A real NEWTABLE always has
        // D >= 0 because D = (B << 8) | C, and the candidate filter guarantees
        // B <= 15 and C == 0, so D = B * 256 ∈ [0, 3840].
        //
        // BUG FIX (B0.28): The old check scanned ALL positions including AUX words
        // of unmapped opcodes (those only advanced by 1, not 2). AUX words of
        // Roblox-specific extensions often have high bits set, giving apparent
        // D < 0 when the NEWTABLE candidate byte appears as their low byte. This
        // caused systematic false rejections across the entire corpus.
        //
        // Fix: apply the same shape filter (c == 0, b <= 15, a < max_stack_size)
        // that the candidate scan uses. Since c == 0 && b <= 15 → D = b*256 ≥ 0,
        // no shape-passing position can ever have D < 0. The check is now vacuously
        // true for all real NEWTABLE bytes but correctly ignores AUX words (which
        // typically have C != 0 or B > 15, failing the shape filter).
        let mut backward_jumps = 0usize;
        for proto in &chunk.protos {
            let mut i = 0usize;
            while i < proto.code.len() {
                let insn = proto.code[i];
                let mapped = ctx.map[insn_op(insn) as usize];
                if mapped != 255 {
                    let s = LuauOpcode::from_u8(mapped);
                    if s.has_aux() && i + 1 < proto.code.len() { i += 2; } else { i += 1; }
                    continue;
                }
                if insn_op(insn) == op {
                    let a = insn_a(insn);
                    let b = insn_b(insn);
                    let c = insn_c(insn);
                    // Apply the same shape filter as the candidate scan so that AUX
                    // words of other unmapped opcodes (which have C != 0 or B > 15)
                    // are excluded. Real NEWTABLE candidates have D = b*256 ≥ 0.
                    if c == 0 && b <= 15 && (a as usize) < proto.max_stack_size as usize {
                        let d = insn_d(insn) as i32;
                        if d < 0 { backward_jumps += 1; }
                    }
                }
                i += 1;
            }
        }
        let not_backward_jump = backward_jumps == 0;

        if clear_margin && not_backward_jump {
            ctx.try_assign(op, LuauOpcode::NewTable as u8);
        }
    }
}

/// SETLIST: A = table reg, B = first value reg, C = count; AUX = table index offset
fn detect_setlist(chunk: &Chunk, ctx: &mut DetectCtx) {
    let newtable_shuffled = ctx.find_shuffled(LuauOpcode::NewTable as u8);
    let duptable_shuffled = ctx.find_shuffled(LuauOpcode::DupTable as u8);

    let mut candidates: HashMap<u8, usize> = HashMap::new();
    for proto in &chunk.protos {
        for i in 0..proto.code.len().saturating_sub(1) {
            let insn = proto.code[i];
            let op = insn_op(insn);
            if ctx.is_mapped(op) { continue; }
            let a = insn_a(insn);
            let b = insn_b(insn);
            let aux = proto.code[i + 1];
            // SETLIST: A < max_stack, B >= A, C is count (0=vararg), AUX is table offset (often 0 or 1).
            // (C is a u8, so it is always <= 255 — no explicit upper bound needed.)
            if a < proto.max_stack_size && b < proto.max_stack_size && aux <= 1024 {
                // Check if there's a NEWTABLE or DUPTABLE before this
                let mut has_table_create = false;
                for j in (0..i).rev().take(20) {
                    let prev_op = insn_op(proto.code[j]);
                    if Some(prev_op) == newtable_shuffled || Some(prev_op) == duptable_shuffled {
                        if insn_a(proto.code[j]) == a {
                            has_table_create = true;
                            break;
                        }
                    }
                }
                if has_table_create {
                    *candidates.entry(op).or_insert(0) += 1;
                }
            }
        }
    }
    if let Some((&op, &count)) = candidates.iter()
        .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
    {
        if count >= 2 { ctx.try_assign(op, LuauOpcode::SetList as u8); }
    }
}

/// GETTABLEN via the sequential-element-read run: `local x, y = t[1], t[2]`.
///
/// The compiler emits that idiom as back-to-back GETTABLENs holding the table
/// register B fixed while the literal index C increments, so the signature is
/// two ADJACENT RAW WORDS sharing an opcode byte and a B field but differing in
/// C. Adjacency must be measured in the raw word array, not the instruction
/// stream: GETTABLEKS is the one other opcode with the same fixed-B/varying-C
/// run shape (its C is a slot-prediction hint), but it carries an AUX word, so
/// two GETTABLEKS are never adjacent as raw words. Requiring raw adjacency is
/// what keeps this off GETTABLEKS entirely.
///
/// The `B != A` invariant is the anti-CALL weapon. CALL reuses the function
/// register as its own base, so B == A in ~24% of real CALLs, whereas a table
/// read never loads through the register it writes: measured 0 of 380 true
/// GETTABLEN instances chunk-wide. Requiring ZERO such instances excludes CALL,
/// SETLIST and NEWTABLE outright.
///
/// This must run BEFORE detect_call, which force-assigns CALL and otherwise
/// steals this byte in table-heavy chunks where GETTABLEN out-counts the real
/// CALL. Measured over 5 permutations of the corpus this fires on 25 true
/// GETTABLEN bytes and 0 non-GETTABLEN bytes, and on nothing at all in the real
/// Roblox samples.
fn detect_gettablen_read_run(chunk: &Chunk, ctx: &mut DetectCtx) {
    if ctx.find_shuffled(LuauOpcode::GetTableN as u8).is_some() {
        return;
    }
    let mut occurrences: HashMap<u8, usize> = HashMap::new();
    let mut in_range: HashMap<u8, usize> = HashMap::new();
    let mut writes_own_source: HashMap<u8, usize> = HashMap::new();
    let mut runs: HashMap<u8, usize> = HashMap::new();
    for proto in &chunk.protos {
        for &insn in &proto.code {
            let op = insn_op(insn);
            *occurrences.entry(op).or_insert(0) += 1;
            if insn_a(insn) < proto.max_stack_size && insn_b(insn) < proto.max_stack_size {
                *in_range.entry(op).or_insert(0) += 1;
            }
            if insn_b(insn) == insn_a(insn) {
                *writes_own_source.entry(op).or_insert(0) += 1;
            }
        }
        for i in 0..proto.code.len().saturating_sub(1) {
            let insn = proto.code[i];
            let next = proto.code[i + 1];
            let op = insn_op(insn);
            if insn_op(next) != op { continue; }
            if insn_b(insn) != insn_b(next) || insn_c(insn) == insn_c(next) { continue; }
            if insn_a(insn) >= proto.max_stack_size
                || insn_b(insn) >= proto.max_stack_size
                || insn_a(next) >= proto.max_stack_size
            {
                continue;
            }
            *runs.entry(op).or_insert(0) += 1;
        }
    }
    let mut viable: Vec<(u8, usize)> = runs
        .iter()
        .filter(|(&op, &run_count)| {
            let total = *occurrences.get(&op).unwrap_or(&0);
            run_count >= 1
                && total >= 2
                && !ctx.is_mapped(op)
                && *writes_own_source.get(&op).unwrap_or(&0) == 0
                && in_range.get(&op).copied().unwrap_or(0) * 10 >= total * 9
        })
        .map(|(&op, &run_count)| (op, run_count))
        .collect();
    // Deterministic: byte ascending when run counts tie.
    viable.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    if let Some(&(op, _)) = viable.first() {
        ctx.try_assign(op, LuauOpcode::GetTableN as u8);
    }
}

/// GETTABLEN/SETTABLEN: ABC format, C is 1-based table index (small integer, usually 1-256)
fn detect_gettablen_settablen(chunk: &Chunk, ctx: &mut DetectCtx) {
    let mut candidates: HashMap<u8, usize> = HashMap::new();
    for proto in &chunk.protos {
        for &insn in &proto.code {
            let op = insn_op(insn);
            if ctx.is_mapped(op) { continue; }
            let a = insn_a(insn);
            let b = insn_b(insn);
            let c = insn_c(insn);
            // GETTABLEN/SETTABLEN: A,B are registers, C is 1-based index (typically 1-10).
            // (C is a u8, so the `<= 255` upper bound is implicit.)
            if a < proto.max_stack_size && b < proto.max_stack_size && c >= 1 {
                *candidates.entry(op).or_insert(0) += 1;
            }
        }
    }
    // This is too broad - need to narrow. GETTABLEN/SETTABLEN should have C concentrated
    // in the small range. Calculate how concentrated C values are.
    let mut filtered: Vec<(u8, usize)> = Vec::new();
    for (&op, &count) in &candidates {
        if ctx.is_mapped(op) || count < 5 { continue; }
        let mut small_c = 0usize;
        let mut total = 0usize;
        // Reduction-chain detection: if many instances of this op form a pattern
        // where A[i+1] == A[i]-1 && B[i+1] == A[i], it's an arithmetic fold, not GETTABLEN.
        // This is critical for ADD/SUB/etc. chains in `a + b + c + d + ...` returns where
        // the result register walks DOWN (down the stack, reusing freed slots) and each
        // subsequent instance reads the previous A as its new B.
        let mut chain_links = 0usize;
        for proto in &chunk.protos {
            let mut prev_op_insn: Option<u32> = None;
            for &insn in &proto.code {
                if insn_op(insn) == op {
                    total += 1;
                    if insn_c(insn) >= 1 && insn_c(insn) <= 10 {
                        small_c += 1;
                    }
                    if let Some(prev) = prev_op_insn {
                        let prev_a = insn_a(prev) as i32;
                        let cur_a = insn_a(insn) as i32;
                        let cur_b = insn_b(insn) as i32;
                        // Chain: A decreases by 1, B re-uses previous A
                        if cur_a == prev_a - 1 && cur_b == prev_a {
                            chain_links += 1;
                        }
                    }
                    prev_op_insn = Some(insn);
                } else {
                    // An unrelated instruction breaks the chain reasoning
                    prev_op_insn = None;
                }
            }
        }
        // Reject reduction-chain patterns: if even 3+ instances form a fold chain
        // (A decreases by 1, B re-uses previous A), this is an arithmetic reduction
        // like `return a + b + c + d + ...`, not table access. Real GETTABLEN
        // chains access table elements with monotonically INCREASING A (stack top).
        if chain_links >= 3 {
            continue;
        }
        // If >=80% of C values are 1-10, it's likely a table index op
        // (raised from 60% to reduce false positives on arith chains)
        if total > 0 && small_c * 100 / total >= 80 {
            filtered.push((op, count));
        }
    }
    // Deterministic: byte ascending when counts tie (HashMap iteration noise).
    filtered.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    if filtered.len() >= 2 {
        ctx.try_assign(filtered[0].0, LuauOpcode::GetTableN as u8);
        ctx.try_assign(filtered[1].0, LuauOpcode::SetTableN as u8);
    } else if filtered.len() == 1 {
        ctx.try_assign(filtered[0].0, LuauOpcode::GetTableN as u8);
    }
}

/// GETTABLE/SETTABLE: dynamic table access with register keys (ABC format)
/// A = target/source, B = table register, C = key register (all must be valid registers)
fn detect_gettable_settable(chunk: &Chunk, ctx: &mut DetectCtx) {
    // Need GETTABLEKS/SETTABLEKS already mapped to distinguish
    let gettableks = ctx.find_shuffled(LuauOpcode::GetTableKS as u8);
    let settableks = ctx.find_shuffled(LuauOpcode::SetTableKS as u8);
    if gettableks.is_none() && settableks.is_none() { return; }

    // Track (total, c_zero_count) per candidate so we can exclude LENGTH-shaped
    // candidates whose 100%-c==0 distribution gives them away. GETTABLE/SETTABLE
    // use C as a register index, so in real code you see a mix of C values across
    // proto usages. LENGTH always has C==0. If a candidate has c_zero == total,
    // it is almost certainly LENGTH masquerading as GETTABLE/SETTABLE.
    let mut candidates: HashMap<u8, (usize, usize)> = HashMap::new();
    for proto in &chunk.protos {
        for &insn in &proto.code {
            let op = insn_op(insn);
            if ctx.is_mapped(op) { continue; }
            let a = insn_a(insn);
            let b = insn_b(insn);
            let c = insn_c(insn);
            // All three operands must be valid registers
            if a < proto.max_stack_size && b < proto.max_stack_size && c < proto.max_stack_size {
                // Distinguish from arithmetic/MOVE-like ops:
                // GETTABLE: A = B[C], so B != A (table != result) and B != C (table != key)
                // SETTABLE: B[C] = A, same constraint
                if a != b && b != c {
                    let entry = candidates.entry(op).or_insert((0, 0));
                    entry.0 += 1;
                    if c == 0 { entry.1 += 1; }
                }
            }
        }
    }
    // GETTABLE/SETTABLE are less common than GETTABLEKS/SETTABLEKS — cap frequency.
    // EXCLUDE candidates with 100% c==0 (LENGTH signature) and also candidates
    // where c==0 is ≥ 80% of hits (likely LENGTH / NOT / MINUS).
    let max_table_freq = if ctx.total_insns > 100 { (ctx.total_insns / 10) as usize } else { usize::MAX };
    let mut sorted: Vec<_> = candidates.iter()
        .filter(|(&op, &(total, c_zero))| {
            !ctx.is_mapped(op)
                && total >= 5
                && total <= max_table_freq
                // c_zero < total * 4 / 5 ⇒ less than 80% of hits have c==0
                && c_zero * 5 < total * 4
        })
        .map(|(&op, &(total, _))| (op, total))
        .collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    // Only assign if we have a clear separation (not too many candidates)
    if sorted.len() <= 6 && sorted.len() >= 2 {
        ctx.try_assign(sorted[0].0, LuauOpcode::GetTable as u8);
        ctx.try_assign(sorted[1].0, LuauOpcode::SetTable as u8);
    }
}

/// Comparison jumps with AUX: JumpIfEq, JumpIfLE, JumpIfLT and their Not variants
/// AD format with AUX word containing register index.
///
/// KEY INSIGHT: For genuine comparison jumps, the AUX word is JUST the register index
/// (0-255) because the Luau compiler writes `aux = right_register` with no upper bits.
/// The VM reads `VM_REG(aux)` directly. So `aux < 256` is a much stronger filter than
/// the older `(aux & 0xFF) < max_stack` check, which would also match any next
/// instruction that happens to start with a register-sized byte.
///
/// Target computation uses `i + d + 1` (the Luau VM jump semantics — PC is advanced
/// past the current instruction before D is added). Using `i + d` would be off-by-one
/// and could wrongly accept out-of-bound jumps as valid.
fn detect_comparison_jumps_aux(chunk: &Chunk, ctx: &mut DetectCtx) {
    let mut candidates: HashMap<u8, usize> = HashMap::new();
    // Boolean-materialisation sites per candidate byte — see the T/F split below.
    let mut bool_mat: HashMap<u8, usize> = HashMap::new();
    // Sites where A == AUX, i.e. the instruction compares a register with
    // itself. Counted separately and NOT credited as candidate evidence: in
    // JumpXEqKN the AUX word is a CONSTANT index, so it lands on the value of A
    // by coincidence often enough to hand a JumpXEqK byte a comparison-jump
    // score it has not earned.
    //
    // Do not escalate this into a rejection rule. Dropping any byte that has a
    // single A == AUX site scores +18 byte-slots across seven corpus seeds and
    // is WRONG: `if x ~= x then` — the standard NaN test — compiles to exactly
    // `JUMPIFEQ Rn Rn` (verified against luau-compile v6), so one NaN guard
    // anywhere in a module would discard a genuine comparison byte along with
    // every real site on it. All of that +18 comes from disqualifying bytes
    // that also carry genuine non-self sites, which is precisely the case a
    // real NaN guard produces. Discounting the sites keeps the sound part of
    // the signal (+4 slots, four of seven seeds, none worse) and none of the
    // hazard. Neither corpus nor the real Roblox module contains a NaN guard,
    // so neither can measure this — hence the compiler check rather than a
    // score.
    let mut self_cmp: HashMap<u8, usize> = HashMap::new();
    for proto in &chunk.protos {
        let ms = proto.max_stack_size as u32;
        // Walk TRUE instruction positions, skipping AUX words of already-mapped
        // AUX-bearing opcodes — without this, an AUX word whose low byte is an
        // unmapped shuffled-byte value (e.g. 0xFF inside a LOADK constant index)
        // wrongly accumulates JumpIfEq candidates, stealing the slot from the
        // real comparison-jump byte. Mirrors detect_jumpxeq below.
        let mut i = 0usize;
        while i + 1 < proto.code.len() {
            let insn = proto.code[i];
            let op = insn_op(insn);
            if ctx.is_mapped(op) {
                let canon = ctx.map[op as usize];
                let luau_op = LuauOpcode::from_u8(canon);
                if luau_op.has_aux() && i + 1 < proto.code.len() {
                    i += 2;
                } else {
                    i += 1;
                }
                continue;
            }
            let a = insn_a(insn);
            let d = insn_d(insn) as i32;
            let aux = proto.code[i + 1];
            // Comparison jumps: A = left register, D = jump offset (nonzero),
            // AUX = full word whose VALUE is the right register index (not just low byte).
            // This rules out the case where "AUX" is actually the next instruction
            // (whose upper bits would be nonzero almost always).
            let target_pc = i as i32 + d + 1;
            if a < proto.max_stack_size && d != 0
                && target_pc >= 0 && (target_pc as usize) < proto.code.len()
                && aux < ms  // full word < max_stack → upper bits zero AND value is a register
            {
                if aux == a as u32 {
                    *self_cmp.entry(op).or_insert(0) += 1;
                } else {
                    *candidates.entry(op).or_insert(0) += 1;
                }
                // Boolean materialisation: `local x = a < b` compiles to a jump
                // over `LOADB dst,false,+1` onto `LOADB dst,true`.
                //
                //   i  : CMP A, D=2 ; AUX = right register
                //   i+2: LOADB dst, 0, +1
                //   i+3: LOADB dst, 1
                //
                // Detected MAP-FREE: the two LOADBs are recognised by sharing an
                // unknown-but-identical opcode byte plus the 0/+1 then 1/0 operand
                // signature. Only the jump-if-TRUE form of an operator can appear
                // here — the compiler emits a NOT-variant solely for the
                // "skip the block when the condition is false" role, and a
                // source-level `not` is applied afterwards by the NOT opcode
                // rather than by flipping the branch. So a byte with any site
                // here can be JumpIfLT / JumpIfLE / JumpIfEq / JumpIfNotEq but
                // never JumpIfNotLT or JumpIfNotLE.
                if d == 2 && i + 3 < proto.code.len() {
                    let w2 = proto.code[i + 2];
                    let w3 = proto.code[i + 3];
                    if insn_op(w2) == insn_op(w3)
                        && insn_a(w2) == insn_a(w3)
                        && insn_b(w2) == 0 && insn_c(w2) == 1
                        && insn_b(w3) == 1 && insn_c(w3) == 0
                    {
                        *bool_mat.entry(op).or_insert(0) += 1;
                    }
                }
                // `repeat ... until cond` puts a second jump-if-TRUE form on
                // record. The guard LEAVES the loop when the condition holds,
                // so it jumps forward over the backward jump that closes the
                // loop — the same "true means take the branch" polarity the
                // boolean-materialisation site above detects, reached by a
                // different shape:
                //
                //   i  : CMP A, D=2 ; AUX = right register
                //   i+2: JUMPBACK    (D < 0, to the top of the body)
                //   i+3: loop exit
                //
                // Recognised MAP-FREE: the word at i+2 must be AD-format with A
                // unused and a negative D landing inside this proto, which no
                // ABC instruction with a live A register can imitate.
                //
                // Worth +5 correct byte-slots across seven permutation seeds
                // (three seeds better, none worse). The one shape that could
                // fool it is `while cond do end` with a genuinely empty body,
                // which compiles to the same three words and IS a NOT form;
                // nothing in the corpus or the real Roblox module writes that.
                if d == 2 && i + 3 < proto.code.len() {
                    let w2 = proto.code[i + 2];
                    let back = insn_d(w2) as i32;
                    let back_target = (i + 2) as i32 + back + 1;
                    if insn_a(w2) == 0
                        && back < 0
                        && back_target >= 0
                        && (back_target as usize) < proto.code.len()
                    {
                        *bool_mat.entry(op).or_insert(0) += 1;
                    }
                }
            }
            i += 1;
        }
    }
    // Comparison jumps are individually uncommon — cap at 5% of total instructions each
    let max_cmp_freq = if ctx.total_insns > 100 { (ctx.total_insns / 20) as usize } else { usize::MAX };
    // count >= 1 (not 2): the strict AUX filter above is strong enough that even a single
    // hit is meaningful. Comparison jumps like `<=` may legitimately appear only once or
    // twice in a small chunk, and the previous threshold was losing them.
    //
    // Purity gate: every occurrence of a REAL comparison-jump byte passes the AUX
    // filter (aux is literally the right-register index). A byte whose matching
    // count is a small fraction of its total frequency is almost certainly a
    // different opcode (typically LOADK with small-index AUX). Observed in the
    // wild: 0xFF = LoadK on variant 0/3 stealing JumpIfEq because its 1–2 false
    // matches beat the real JumpIfEq byte's 1 real match on tiebreak. Require
    // ≥50% purity, but only when the byte has ≥4 occurrences (small-corpus
    // single-hit bytes can legitimately be 100%-pure comparison jumps).
    let mut sorted: Vec<_> = candidates.iter()
        .filter(|(&op, &count)| {
            if ctx.is_mapped(op) || count < 1 || count > max_cmp_freq { return false; }
            let freq = ctx.freq[op as usize] as usize;
            freq < 4 || count * 2 >= freq
        })
        .map(|(&op, &count)| (op, count))
        .collect();
    // Deterministic tiebreak: byte ascending when counts tie. Without this,
    // HashMap iteration noise swaps JumpIfNotLE / JumpIfNotLT between runs when
    // both have 1 occurrence (observed as 0x7D vs 0xF1 flipping on ModuleScript.luac).
    sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    // All six members are byte-identical in encoding, so the candidate SET is as
    // good as it gets and the only remaining question is WHICH member each byte
    // is. Ranking by raw frequency against one fixed list is a positional guess:
    // whichever branch the program happens to use most is handed JumpIfEq. On the
    // `comparisons(a, b)` probe in real Roblox bytecode that mislabels the most
    // frequent branch — a plain `a < b` reads back as `a ~= b`.
    //
    // Split the candidates on the one axis that IS recoverable from structure:
    // whether the byte was ever used to materialise a boolean (see `bool_mat`).
    // Bytes with such a site are jump-if-TRUE forms; bytes without are, on this
    // corpus and on real bytecode, dominated by the NOT forms the compiler emits
    // for `if` / `while` guards.
    let (true_forms, not_forms): (Vec<u8>, Vec<u8>) = sorted.iter()
        .map(|&(op, _)| op)
        .partition(|op| bool_mat.get(op).copied().unwrap_or(0) > 0);

    const TRUE_FORM_ORDER: [LuauOpcode; 4] = [
        LuauOpcode::JumpIfLT, LuauOpcode::JumpIfLE,
        LuauOpcode::JumpIfEq, LuauOpcode::JumpIfNotEq,
    ];
    const NOT_FORM_ORDER: [LuauOpcode; 4] = [
        LuauOpcode::JumpIfNotLT, LuauOpcode::JumpIfNotEq,
        LuauOpcode::JumpIfNotLE, LuauOpcode::JumpIfEq,
    ];
    // Combined order for anything the split could not place. This pass is NOT
    // optional: the two lists hold four entries each, so one class can run dry
    // while the other has spare capacity, and a comparison byte left unmapped
    // here is handed straight to detect_newtable — which mistakes an unmapped AD
    // branch for a NEWTABLE candidate. Consuming the same number of candidate
    // bytes as the old single list keeps that input unchanged.
    const FALLBACK_ORDER: [LuauOpcode; 6] = [
        LuauOpcode::JumpIfNotLT, LuauOpcode::JumpIfLT,
        LuauOpcode::JumpIfNotEq, LuauOpcode::JumpIfNotLE,
        LuauOpcode::JumpIfEq, LuauOpcode::JumpIfLE,
    ];

    let mut leftovers: Vec<u8> = Vec::new();
    for (bytes, order) in [
        (&true_forms, &TRUE_FORM_ORDER[..]),
        (&not_forms, &NOT_FORM_ORDER[..]),
    ] {
        let mut std_idx = 0usize;
        for &op in bytes.iter() {
            while std_idx < order.len() && ctx.assigned[order[std_idx] as usize] {
                std_idx += 1;
            }
            if std_idx >= order.len() || !ctx.try_assign(op, order[std_idx] as u8) {
                leftovers.push(op);
                continue;
            }
            std_idx += 1;
        }
    }

    let mut std_idx = 0usize;
    for &op in leftovers.iter() {
        while std_idx < FALLBACK_ORDER.len()
            && ctx.assigned[FALLBACK_ORDER[std_idx] as usize]
        {
            std_idx += 1;
        }
        if std_idx >= FALLBACK_ORDER.len() { break; }
        if ctx.try_assign(op, FALLBACK_ORDER[std_idx] as u8) {
            std_idx += 1;
        }
    }
}

/// JumpXEqKNil, JumpXEqKB, JumpXEqKN, JumpXEqKS: AD format with specific AUX patterns
///
/// Fixed vs original: exclusive categorisation per instruction (each instruction
/// counts in exactly ONE bucket using priority String > Number > Bool > Nil),
/// correct 24-bit constant-index masking (& 0x00FFFFFF), consistency-ratio
/// scoring so that an opcode where 11/12 hits are string-typed beats one with
/// 10/50 string hits, and correct VM jump target (`i + d + 1`, matching the
/// FORGPREP / comparison-jump convention).
///
/// Differentiated thresholds per variant:
/// - JumpXEqKS / JumpXEqKN: strong AUX validation (constant-type check), safe
///   to accept single-hit evidence when the ratio is ≥ 80% (score ≥ 800).
/// - JumpXEqKB / JumpXEqKNil: weak AUX validation (`aux_low31 <= 1` matches
///   any instruction whose low word is 0 or 1), still require `hits >= 2` to
///   avoid stealing LOADK / MOVE bytes whose "next word" happens to be tiny.
///   (Attempted `hits >= 1` in Phase 4 regressed JumpXEqKNil 45→42 and
///   JumpXEqKB 2→0 while only gaining 1 entry for JumpXEqKS — NOT worth it.)
fn detect_jumpxeq(chunk: &Chunk, ctx: &mut DetectCtx) {
    // Per-opcode counters: (matching_hits, total_hits_as_jumpxeq_candidate)
    let mut str_cand: HashMap<u8, (usize, usize)> = HashMap::new();
    let mut num_cand: HashMap<u8, (usize, usize)> = HashMap::new();
    let mut bool_cand: HashMap<u8, (usize, usize)> = HashMap::new();
    let mut nil_cand: HashMap<u8, (usize, usize)> = HashMap::new();

    for proto in &chunk.protos {
        // Walk TRUE instruction positions, skipping AUX words of known opcodes.
        // This prevents AUX data (e.g. GETIMPORT's 32-bit constant index) from
        // being treated as potential JumpXEqK* instructions — their low byte might
        // coincide with the real ForGLoopINext (Deprecated61) byte and accumulate
        // false JumpXEqKB hits (since aux_low31 <= 1 is very permissive).
        let mut i = 0usize;
        while i + 1 < proto.code.len() {
            let insn = proto.code[i];
            let op = insn_op(insn);
            if ctx.is_mapped(op) {
                // Skip AUX words of known opcodes so they are never processed as
                // potential instructions. Without this, an AUX word at position i+1
                // whose low byte = ForGLoopINext's shuffled byte could accumulate hits.
                let canon = ctx.map[op as usize];
                let luau_op = LuauOpcode::from_u8(canon);
                if luau_op.has_aux() && i + 1 < proto.code.len() {
                    i += 2;
                } else {
                    i += 1;
                }
                continue;
            }
            let a = insn_a(insn);
            let d = insn_d(insn) as i32;
            let aux = proto.code[i + 1];
            // VM jump target is `pc + d + 1` (pc advances past the current AD insn
            // before adding the signed D offset). Must be strictly in-bounds.
            let target_pc = i as i32 + d + 1;
            if a >= proto.max_stack_size || d <= 0
                || target_pc < 0
                || (target_pc as usize) >= proto.code.len()
            {
                i += 1;
                continue;
            }
            // AUX encoding: bit 31 = NOT flag, low 24 bits = constant index
            // (matching the lifter at lifter.rs:1422)
            let kidx = (aux & 0x00FFFFFF) as usize;
            let aux_low31 = aux & 0x7FFFFFFF; // for nil/bool the full low-31 bits matter

            // Exclusive categorisation -- each instruction in exactly one bucket.
            // Priority: String > Number > Bool > Nil.
            // A JumpXEqKS always has AUX pointing to a String constant.
            // A JumpXEqKN always has AUX pointing to a Number constant.
            // A JumpXEqKB has aux_low31 of 0 or 1 (boolean value, NOT a constant index).
            // A JumpXEqKNil has aux_low31 == 0 (nil, no constant reference).
            let is_str = kidx < proto.constants.len()
                && matches!(proto.constants.get(kidx), Some(Constant::String(_)));
            let is_num = kidx < proto.constants.len()
                && matches!(proto.constants.get(kidx), Some(Constant::Number(_)));
            let is_bool = aux_low31 <= 1; // 0 or 1 (fits boolean)
            let is_nil = aux_low31 == 0;   // only 0 (nil)

            if is_str {
                let e = str_cand.entry(op).or_insert((0, 0));
                e.0 += 1; e.1 += 1;
                // Still count this op as a candidate in other maps (total only)
                // so that ratio scoring works -- but do NOT add matching hits.
                num_cand.entry(op).or_insert((0, 0)).1 += 1;
                if is_bool { bool_cand.entry(op).or_insert((0, 0)).1 += 1; }
                if is_nil  { nil_cand.entry(op).or_insert((0, 0)).1 += 1; }
            } else if is_num {
                let e = num_cand.entry(op).or_insert((0, 0));
                e.0 += 1; e.1 += 1;
                str_cand.entry(op).or_insert((0, 0)).1 += 1;
                if is_bool { bool_cand.entry(op).or_insert((0, 0)).1 += 1; }
                if is_nil  { nil_cand.entry(op).or_insert((0, 0)).1 += 1; }
            } else if is_bool && !is_nil {
                // aux_low31 == 1 -- only a boolean candidate, not nil
                let e = bool_cand.entry(op).or_insert((0, 0));
                e.0 += 1; e.1 += 1;
                str_cand.entry(op).or_insert((0, 0)).1 += 1;
                num_cand.entry(op).or_insert((0, 0)).1 += 1;
            } else if is_nil {
                // aux_low31 == 0 -- could be nil OR bool(false)
                // Count as both nil and bool matching hits
                let en = nil_cand.entry(op).or_insert((0, 0));
                en.0 += 1; en.1 += 1;
                let eb = bool_cand.entry(op).or_insert((0, 0));
                eb.0 += 1; eb.1 += 1;
                str_cand.entry(op).or_insert((0, 0)).1 += 1;
                num_cand.entry(op).or_insert((0, 0)).1 += 1;
            } else {
                // AUX doesn't match any JumpXEqK* pattern -- count as total only
                str_cand.entry(op).or_insert((0, 0)).1 += 1;
                num_cand.entry(op).or_insert((0, 0)).1 += 1;
                if is_bool { bool_cand.entry(op).or_insert((0, 0)).1 += 1; }
            }
            i += 1;
        }
    }

    // Score: consistency ratio weighted by volume.
    let score = |hits: usize, total: usize| -> usize {
        if hits < 1 || total == 0 { return 0; }
        let ratio = hits * 1000 / total;
        if ratio < 400 { return 0; } // below 40% consistency -- likely noise
        ratio * hits // weight by both consistency and volume
    };
    // Minimum score guards against single-hit false positives: accept
    //   - hits >= 2 with ratio >= 400 (score = 800)  OR
    //   - hits == 1 with ratio >= 800 (score = 800, i.e. 80% clean single hit)
    let strong_score = |hits: usize, total: usize| -> bool { score(hits, total) >= 800 };

    // JumpXEqKS -- strong AUX validation (String constant): single-hit OK if score ≥ 800
    if let Some((&op, _)) = str_cand.iter()
        .filter(|(&op, &(hits, total))| !ctx.is_mapped(op) && hits >= 1 && strong_score(hits, total))
        .max_by(|a, b| {
            let sa = score(a.1.0, a.1.1);
            let sb = score(b.1.0, b.1.1);
            sa.cmp(&sb).then_with(|| b.0.cmp(a.0))
        })
    {
        ctx.try_assign(op, LuauOpcode::JumpXEqKS as u8);
    }
    // JumpXEqKN -- strong AUX validation (Number constant): single-hit OK if score ≥ 800
    if let Some((&op, _)) = num_cand.iter()
        .filter(|(&op, &(hits, total))| !ctx.is_mapped(op) && hits >= 1 && strong_score(hits, total))
        .max_by(|a, b| {
            let sa = score(a.1.0, a.1.1);
            let sb = score(b.1.0, b.1.1);
            sa.cmp(&sb).then_with(|| b.0.cmp(a.0))
        })
    {
        ctx.try_assign(op, LuauOpcode::JumpXEqKN as u8);
    }
    // JumpXEqKB -- weak AUX validation (aux_low31 <= 1 matches MANY instructions);
    // require hits >= 2 to avoid stealing common AD-format bytes.
    if let Some((&op, _)) = bool_cand.iter()
        .filter(|(&op, &(hits, _))| !ctx.is_mapped(op) && hits >= 2)
        .max_by(|a, b| {
            let sa = score(a.1.0, a.1.1);
            let sb = score(b.1.0, b.1.1);
            sa.cmp(&sb).then_with(|| b.0.cmp(a.0))
        })
    {
        ctx.try_assign(op, LuauOpcode::JumpXEqKB as u8);
    }
    // JumpXEqKNil -- weak AUX validation (aux_low31 == 0); require hits >= 2.
    if let Some((&op, _)) = nil_cand.iter()
        .filter(|(&op, &(hits, _))| !ctx.is_mapped(op) && hits >= 2)
        .max_by(|a, b| {
            let sa = score(a.1.0, a.1.1);
            let sb = score(b.1.0, b.1.1);
            sa.cmp(&sb).then_with(|| b.0.cmp(a.0))
        })
    {
        ctx.try_assign(op, LuauOpcode::JumpXEqKNil as u8);
    }
}

/// JUMPBACK: AD format, D < 0 (backwards jump)
/// Note: A is typically 0 but we don't require it — just check D < 0 and valid target
///
/// VM jump target is `pc + d + 1`, which must be a valid (non-negative) PC.
/// Threshold lowered from `count >= 3` to `count >= 1`: a single backward jump
/// with an in-bounds target is a very strong signal — there is no other AD
/// opcode that uses a *negative* D field, so false positives are effectively
/// impossible (excluded only by A-field or AUX constraints, which we don't
/// apply here because the D-sign check alone is already discriminating).
fn detect_jumpback(chunk: &Chunk, ctx: &mut DetectCtx) {
    // Phase B0.33: distinguish JUMPBACK from FORGLOOP via A-field and AUX-shape.
    //
    // JUMPBACK (canonical 24): AD format, A=0 (unused), D<0, NO AUX after.
    // FORGLOOP  (canonical 59): AD format, A=loop_base (non-zero), D<0, HAS AUX where
    //   AUX = (count & 0xFF) | (is_ipairs << 31), so bits 8-30 are zero and LSB is 1-15.
    //
    // Prior bug: when detect_generic_for failed to find a FORGPREP→FORGLOOP pair
    // (e.g. in corpus variants seeded by FORGLOOP-less small scripts), the real
    // FORGLOOP byte (0x6E in v0) remained unmapped. detect_jumpback then picked
    // 0x6E because its FORGLOOP instances have d<0 and in-range target, leaving
    // the real JUMPBACK byte (0x48) unmapped. v0 cache confirms: JUMPBACK@0x6E,
    // 0x48 UNMAPPED. v2/v3 (seeded by FORGLOOP-having scripts) correctly place
    // FORGLOOP@0x6E and JUMPBACK@0x48.
    //
    // Fix: per candidate byte, count THREE shapes across its d<0 hits:
    //   - jb_shape: A==0 AND next word does NOT look like FORGLOOP AUX
    //   - fg_shape: next word looks like FORGLOOP AUX (count 1-15, mid bits zero)
    //   - neutral:  neither (still a d<0 candidate)
    // Reject candidates where fg_shape > jb_shape (they're FORGLOOP bytes).
    // Among the rest, prefer the one with max jb_shape count.
    let mut jb_shape: HashMap<u8, usize> = HashMap::new();
    let mut fg_shape: HashMap<u8, usize> = HashMap::new();
    let mut any_d_neg: HashMap<u8, usize> = HashMap::new();
    for proto in &chunk.protos {
        let code_len = proto.code.len() as i32;
        for (i, &insn) in proto.code.iter().enumerate() {
            let op = insn_op(insn);
            if ctx.is_mapped(op) { continue; }
            let d = insn_d(insn) as i32;
            let target = i as i32 + d + 1;
            if d < 0 && target >= 0 && target < code_len {
                *any_d_neg.entry(op).or_insert(0) += 1;
                let a = insn_a(insn);
                // Inspect the word at i+1 for FORGLOOP-AUX shape.
                let looks_like_forgloop_aux = if (i + 1) < proto.code.len() {
                    let aux = proto.code[i + 1];
                    let count = aux & 0xFF;
                    let mid = aux & 0x7FFF_FF00;
                    count >= 1 && count <= 15 && mid == 0
                } else { false };
                if looks_like_forgloop_aux {
                    *fg_shape.entry(op).or_insert(0) += 1;
                } else if a == 0 {
                    *jb_shape.entry(op).or_insert(0) += 1;
                }
            }
        }
    }
    // Phase B0.2 raw-frequency cap retained: JUMPBACK is rare (~1/loop), cap at
    // 5% of total instructions (absolute floor 20) to reject LOADN/LOADK-shape
    // false positives whose frequencies scale with chunk size.
    let jb_max: u32 = std::cmp::max(20u32, ctx.total_insns / 20);
    // Build final candidate set: must have at least one jb-shape hit AND more
    // jb-shape than fg-shape (so purely-FORGLOOP bytes are excluded).
    let mut candidates: HashMap<u8, usize> = HashMap::new();
    for (&op, &jb_count) in &jb_shape {
        if ctx.freq[op as usize] > jb_max { continue; }
        let fg_count = fg_shape.get(&op).copied().unwrap_or(0);
        // Require jb_shape to strictly dominate fg_shape.
        if jb_count > fg_count {
            candidates.insert(op, jb_count);
        }
    }
    // Fallback: if strict filter yielded nothing, use old loose d<0 count but
    // still exclude bytes where fg_shape dominates (those are FORGLOOP).
    if candidates.is_empty() {
        for (&op, &total) in &any_d_neg {
            if ctx.freq[op as usize] > jb_max { continue; }
            let fg_count = fg_shape.get(&op).copied().unwrap_or(0);
            // Strict FORGLOOP-exclusion: reject if ≥50% of hits look like FORGLOOP.
            if fg_count * 2 >= total { continue; }
            candidates.insert(op, total);
        }
    }
    if let Some((&op, &count)) = candidates.iter()
        .filter(|(&op, _)| !ctx.is_mapped(op))
        .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
    {
        if count >= 1 { ctx.try_assign(op, LuauOpcode::JumpBack as u8); }
    }
}

// ═══════════════════════════════════════════════════════════════
// TIER 5: Format-based detection (requires most opcodes mapped)
// ═══════════════════════════════════════════════════════════════

/// LOADB: ABC format, B is strictly 0 or 1, C is 0 or small positive jump
fn detect_loadb(chunk: &Chunk, ctx: &mut DetectCtx) {
    let mut candidates: HashMap<u8, usize> = HashMap::new();
    for proto in &chunk.protos {
        for &insn in &proto.code {
            let op = insn_op(insn);
            if ctx.is_mapped(op) { continue; }
            let a = insn_a(insn);
            let b = insn_b(insn);
            let c = insn_c(insn);
            // LOADB: A = target register, B = 0 or 1 (boolean), C = jump offset (0 = none)
            if a < proto.max_stack_size && b <= 1 && c <= 1 {
                *candidates.entry(op).or_insert(0) += 1;
            }
        }
    }
    // Verify: ALL instances must have B in {0,1}
    let mut verified: Vec<(u8, usize)> = Vec::new();
    for (&op, &count) in &candidates {
        if ctx.is_mapped(op) || count < 3 { continue; }
        let mut all_bool = true;
        for proto in &chunk.protos {
            for &insn in &proto.code {
                if insn_op(insn) == op && insn_b(insn) > 1 {
                    all_bool = false;
                    break;
                }
            }
            if !all_bool { break; }
        }
        if all_bool {
            verified.push((op, count));
        }
    }
    // Deterministic: byte ascending when counts tie (HashMap iteration noise).
    verified.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    if let Some(&(op, count)) = verified.first() {
        if count >= 3 { ctx.try_assign(op, LuauOpcode::LoadB as u8); }
    }
}

/// LOADN: AD format, loads a signed integer into register A
fn detect_loadn(chunk: &Chunk, ctx: &mut DetectCtx) {
    // B0.76: Stronger LOADN discriminator.
    //
    // LOADN is AD-format: A=register, D=signed integer literal value.
    // Key properties that distinguish from ABC-format imposters:
    //   1. D can be NEGATIVE (e.g., LOADN R0, -1). For ABC-format ops,
    //      D = B|(C<<8) with B,C < max_stack (~100), so D > 0 always.
    //   2. D can exceed the constant table size. LOADK has D = const_index
    //      which is always < constants.len(). LOADN has D = literal value
    //      (e.g., 1000, 9999).
    //   3. D values are heavily concentrated: LOADN(0), LOADN(1), LOADN(2)
    //      account for a large fraction of instances.
    //
    // Score: negative_d_count + exceeds_constants_count + d_concentration.
    // ABC-format bytes score ~0. Real LOADN scores high.
    let max_const_count = chunk.protos.iter()
        .map(|p| p.constants.len())
        .max()
        .unwrap_or(0) as i32;

    let mut candidates: HashMap<u8, usize> = HashMap::new();
    for proto in &chunk.protos {
        for &insn in &proto.code {
            let op = insn_op(insn);
            if ctx.is_mapped(op) { continue; }
            if insn_a(insn) < proto.max_stack_size {
                *candidates.entry(op).or_insert(0) += 1;
            }
        }
    }

    let mut verified: Vec<(u8, usize, i32)> = Vec::new(); // (byte, count, score)
    for (&op, &count) in &candidates {
        if ctx.is_mapped(op) || count < 5 { continue; }

        let mut total = 0usize;
        let mut negative_d = 0usize;
        let mut exceeds_const = 0usize;
        let mut d_freq: HashMap<i32, usize> = HashMap::new();

        for proto in &chunk.protos {
            let pc = proto.constants.len() as i32;
            for &insn in &proto.code {
                if insn_op(insn) == op {
                    total += 1;
                    let d = insn_d(insn) as i32;
                    if d < 0 { negative_d += 1; }
                    if d >= pc.max(max_const_count) || d < 0 { exceeds_const += 1; }
                    *d_freq.entry(d).or_insert(0) += 1;
                }
            }
        }
        if total < 5 { continue; }

        // D-value concentration: top-3 most common D values as % of total.
        // LOADN: often 80%+ (0, 1, 2 dominate). ABC: more spread out.
        let mut freqs: Vec<usize> = d_freq.values().copied().collect();
        freqs.sort_unstable_by(|a, b| b.cmp(a));
        let top3: usize = freqs.iter().take(3).sum();
        let concentration = (top3 * 100 / total) as i32;

        // Score: rewards AD-format properties, penalizes ABC-format signals.
        // negative_d is the strongest signal (impossible for ABC with small stacks).
        let mut score: i32 = 0;
        if negative_d > 0 { score += 30; }
        if exceeds_const > 0 { score += 20; }
        if concentration >= 50 { score += concentration / 5; }
        // Still require small-D majority (70%+) as basic sanity check
        let small_d = d_freq.iter()
            .filter(|(&d, _)| d >= -1000 && d <= 10000)
            .map(|(_, &c)| c)
            .sum::<usize>();
        if small_d * 100 / total < 70 { continue; }

        score += (count as i32).min(200); // frequency bonus, capped

        verified.push((op, count, score));
    }

    // Sort by score descending, then count, then byte for determinism.
    verified.sort_by(|a, b| b.2.cmp(&a.2)
        .then_with(|| b.1.cmp(&a.1))
        .then_with(|| a.0.cmp(&b.0)));
    if let Some(&(op, _, _)) = verified.first() {
        ctx.try_assign(op, LuauOpcode::LoadN as u8);
    }
}

/// LOADNIL: AD format (or ABC with B=C=0), sets register A to nil
fn detect_loadnil(chunk: &Chunk, ctx: &mut DetectCtx) {
    let mut candidates: HashMap<u8, usize> = HashMap::new();
    for proto in &chunk.protos {
        for &insn in &proto.code {
            let op = insn_op(insn);
            if ctx.is_mapped(op) { continue; }
            let a = insn_a(insn);
            let b = insn_b(insn);
            let c = insn_c(insn);
            // LOADNIL: A = target register, B=0, C=0 (rest of word unused)
            if a < proto.max_stack_size && b == 0 && c == 0 {
                *candidates.entry(op).or_insert(0) += 1;
            }
        }
    }
    // LOADNIL should have ALL instances with B=0, C=0
    let mut verified: Vec<(u8, usize)> = Vec::new();
    for (&op, &count) in &candidates {
        if ctx.is_mapped(op) || count < 2 { continue; }
        let mut all_zero = true;
        for proto in &chunk.protos {
            for &insn in &proto.code {
                if insn_op(insn) == op && (insn_b(insn) != 0 || insn_c(insn) != 0) {
                    all_zero = false;
                    break;
                }
            }
            if !all_zero { break; }
        }
        if all_zero {
            verified.push((op, count));
        }
    }
    // Deterministic: byte ascending when counts tie (HashMap iteration noise).
    verified.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    if let Some(&(op, _)) = verified.first() {
        ctx.try_assign(op, LuauOpcode::LoadNil as u8);
    }
}

/// MOVE: ABC format, A = target, B = source, C = 0 (always). Highest frequency instruction.
/// Key distinction from GETUPVAL: MOVE appears in ALL protos (including those with 0 upvalues),
/// while GETUPVAL only appears in protos WITH upvalues.
/// Key distinction from unary ops (NOT/MINUS/LENGTH): MOVE allows A==B, unaries always have A!=B.
/// MOVE is also far more frequent than any unary op.
fn detect_move(chunk: &Chunk, ctx: &mut DetectCtx) {
    // Collect per-candidate stats for refined scoring
    let mut totals: HashMap<u8, usize> = HashMap::new();
    let mut a_ne_b_counts: HashMap<u8, usize> = HashMap::new();
    let mut in_no_upval: HashMap<u8, usize> = HashMap::new();
    let mut all_c_zero: HashMap<u8, bool> = HashMap::new();
    // Every occurrence of the byte, and the subset carrying MOVE's full operand
    // shape. `totals` above only counts occurrences that already pass the register
    // window, so on its own it cannot register an occurrence that disproves MOVE.
    let mut seen: HashMap<u8, usize> = HashMap::new();
    let mut move_shape: HashMap<u8, usize> = HashMap::new();

    for proto in &chunk.protos {
        for &insn in &proto.code {
            let op = insn_op(insn);
            if ctx.is_mapped(op) { continue; }
            let a = insn_a(insn);
            let b = insn_b(insn);
            let c = insn_c(insn);
            *seen.entry(op).or_insert(0) += 1;
            if a < proto.max_stack_size && b < proto.max_stack_size && c == 0 {
                *move_shape.entry(op).or_insert(0) += 1;
            }
            if a < proto.max_stack_size && b < proto.max_stack_size {
                *totals.entry(op).or_insert(0) += 1;
                if c != 0 { all_c_zero.insert(op, false); }
                else { all_c_zero.entry(op).or_insert(true); }
                if a != b { *a_ne_b_counts.entry(op).or_insert(0) += 1; }
                if proto.num_upvalues == 0 { *in_no_upval.entry(op).or_insert(0) += 1; }
            }
        }
    }

    // A single matching instruction is not evidence: this detector FORCE-assigns,
    // and MOVE's shape (A,B registers, C=0) is shared by LOADK, GETVARARGS,
    // GETUPVAL and the unary ops. On a short script that contains no MOVE at all
    // — common; MOVE is absent from 7 of the 47 canonical corpus programs — a
    // lone LOADK would otherwise be force-labelled MOVE and the real LOADK left
    // homeless. Require at least two occurrences before claiming the byte.
    let min_count: usize = if ctx.total_insns > 1000 { 10 } else if ctx.total_insns > 100 { 3 } else { 2 };

    // Filter: must have ALL instances C=0, meet minimum count
    let mut verified: Vec<(u8, usize, f64)> = Vec::new(); // (op, total, score)
    for (&op, &total) in &totals {
        if ctx.is_mapped(op) || total < min_count { continue; }
        if all_c_zero.get(&op) != Some(&true) { continue; }

        // Operand-shape purity gate.
        //
        // MOVE's A and B are both register indices and its C is unused, so EVERY
        // occurrence of the real MOVE byte satisfies A,B < max_stack_size and
        // C == 0. LOADN is AD-format with D = an integer literal; a small
        // non-negative literal decodes as B = literal, C = 0 — exactly the shape
        // scored above — and LOADN is both more frequent than MOVE and present in
        // more protos, so absolute counts favour it. Nothing else here separates
        // the two.
        //
        // The separator is the literal escaping the register window, or spilling
        // into C once it exceeds 255. Measured over the 47-program corpus, the
        // true MOVE byte satisfies the full shape in 262/262 occurrences and the
        // true LOADN byte in 407/587 (69%); on a 314-proto Roblox module the true
        // MOVE byte scored 454/454. Testing the whole shape over EVERY occurrence
        // — rather than the register window over the surviving ones — is what
        // makes the ratio meaningful.
        //
        // 5% slack absorbs AUX words that happen to carry this byte value.
        let all_seen = seen.get(&op).copied().unwrap_or(0);
        let shaped = move_shape.get(&op).copied().unwrap_or(0);
        if shaped * 20 < all_seen * 19 { continue; }

        // Score the candidate: MOVE should be high-frequency AND appear in non-upval protos
        let mut score = total as f64;

        // Bonus: appears in protos with NO upvalues (rules out GETUPVAL/SETUPVAL)
        let no_upval_count = in_no_upval.get(&op).copied().unwrap_or(0);
        if no_upval_count > 0 {
            score *= 1.5;
        }

        // Bonus: has significant A!=B ratio (real register copies, not all no-ops)
        // Real MOVE almost always has a mix of A==B and A!=B, with A!=B being dominant.
        let a_ne_b = a_ne_b_counts.get(&op).copied().unwrap_or(0);
        if total > 5 && a_ne_b * 100 / total > 30 {
            score *= 1.2;
        }

        verified.push((op, total, score));
    }

    // Sort by score descending (integer-scaled to avoid NaN), byte ascending as tiebreak.
    verified.sort_by(|a, b| {
        let sa = (a.2 * 1000.0) as i64;
        let sb = (b.2 * 1000.0) as i64;
        sb.cmp(&sa).then_with(|| a.0.cmp(&b.0))
    });
    if let Some(&(op, _, _)) = verified.first() {
        // Force assignment — C=0-for-all-instances + high frequency is a very strong discriminator
        ctx.try_assign_force(op, LuauOpcode::Move as u8);
    }
}

/// Sequence-based arithmetic detector.
///
/// Detects arithmetic ladders — sequences of 5+ consecutive ABC instructions where:
///   - A is monotonically increasing by 1 (each inst targets the next register)
///   - B is identical across all instructions (shared left operand)
///   - C is consistent: either always the SAME register, OR always a (possibly varying)
///     constant index pointing to a Number
///   - All opcode bytes are distinct (each inst uses a different arith op)
///   - The sequence bytes are NOT already mapped to non-arith standard ops
///
/// Such sequences are strongly characteristic of arithmetic ladders like:
///   local sum = a + b; local diff = a - b; local prod = a * b; ...
///
/// Anchoring: if at least one byte in the sequence is already mapped to an expected
/// arith op (e.g., ADD detected via reduction-chain), the position of that anchor
/// determines the standard-op offset for the rest of the sequence.
///
/// Standard Luau arith order (source order for `+ - * / % ^ //`):
///   reg-reg: ADD(33), SUB(34), MUL(35), DIV(36), MOD(37), POW(38), IDIV(76)
///   reg-K:   ADDK(39), SUBK(40), MULK(41), DIVK(42), MODK(43), POWK(44), IDIVK(77)
/// ADDK/SUBK/MULK/DIVK from a compound-assignment ladder on ONE register.
///
/// `detect_arith_sequence` looks for a monotonic-A run, which compound
/// assignment never produces: `n += 1; n -= 2; n *= 3; n /= 4` writes the SAME
/// register every time, so A == B == r throughout and the run length under the
/// monotonic test is 1. `detect_arithmetic_k` cannot see these either — each op
/// occurs once or twice, far below its `count >= 3` floor. The ladder itself is
/// the only evidence: consecutive words, one register, distinct opcode bytes,
/// every C indexing a Number.
///
/// Only the FIRST FOUR positions are claimed, and that cap is load-bearing.
/// Source order fixes the emission order, and the two ladders available agree on
/// ADDK, SUBK, MULK, DIVK but diverge immediately after: the corpus writes
/// `^ % //` while the real Roblox module's 7-long ladder reads
/// ADDK SUBK MULK DIVK IDIVK MODK POWK. Assigning positions 4+ from a fixed list
/// would therefore mislabel three opcodes on real bytecode.
///
/// Run length must be exactly >= 4. Measured over 5 permutations of the corpus:
/// at 4 the rule fires 5 times with 20 correct and 0 wrong slot predictions; at 3
/// it fires 30 times and gets 75 predictions wrong; at 5 it never fires at all.
/// On the real module it fires once and reproduces the existing map byte for byte.
fn detect_same_register_arith_k_ladder(chunk: &Chunk, ctx: &mut DetectCtx) {
    const LADDER: [LuauOpcode; 4] = [
        LuauOpcode::AddK, LuauOpcode::SubK, LuauOpcode::MulK, LuauOpcode::DivK,
    ];
    const MIN_RUN: usize = 4;
    // All-or-nothing: a misaligned ladder would shift every label by one, so only
    // act when the whole head is free.
    if LADDER.iter().any(|&op| ctx.assigned[op as usize]) {
        return;
    }
    for proto in &chunk.protos {
        let code = &proto.code;
        let shaped = |word: u32, r: u8| -> bool {
            insn_a(word) == r
                && insn_b(word) == r
                && r < proto.max_stack_size
                && matches!(proto.constants.get(insn_c(word) as usize),
                            Some(Constant::Number(_)))
        };
        let mut i = 0usize;
        while i < code.len() {
            let r = insn_a(code[i]);
            if !shaped(code[i], r) {
                i += 1;
                continue;
            }
            let mut run: Vec<u8> = vec![insn_op(code[i])];
            let mut j = i + 1;
            while j < code.len()
                && shaped(code[j], r)
                && !run.contains(&insn_op(code[j]))
            {
                run.push(insn_op(code[j]));
                j += 1;
            }
            if run.len() >= MIN_RUN && run.iter().take(4).all(|&op| !ctx.is_mapped(op)) {
                for (pos, &op) in run.iter().take(4).enumerate() {
                    ctx.try_assign(op, LADDER[pos] as u8);
                }
                return;
            }
            i = if j > i { j } else { i + 1 };
        }
    }
}

fn detect_arith_sequence(chunk: &Chunk, ctx: &mut DetectCtx) {
    let reg_arith_ops: [u8; 7] = [
        LuauOpcode::Add as u8,
        LuauOpcode::Sub as u8,
        LuauOpcode::Mul as u8,
        LuauOpcode::Div as u8,
        LuauOpcode::Mod as u8,
        LuauOpcode::Pow as u8,
        LuauOpcode::IDiv as u8,
    ];
    let k_arith_ops: [u8; 7] = [
        LuauOpcode::AddK as u8,
        LuauOpcode::SubK as u8,
        LuauOpcode::MulK as u8,
        LuauOpcode::DivK as u8,
        LuauOpcode::ModK as u8,
        LuauOpcode::PowK as u8,
        LuauOpcode::IDivK as u8,
    ];

    for proto in &chunk.protos {
        let code = &proto.code;
        let mut i = 0usize;
        while i < code.len() {
            let insn_i = code[i];
            let a_i = insn_a(insn_i);
            let b_i = insn_b(insn_i);
            let c_i = insn_c(insn_i);

            // Starting instruction must have A and B as valid registers
            if a_i >= proto.max_stack_size || b_i >= proto.max_stack_size {
                i += 1;
                continue;
            }

            // Walk forward collecting a matching-shape sequence
            let mut seq_ops: Vec<u8> = vec![insn_op(insn_i)];
            let mut seq_c: Vec<u8> = vec![c_i];
            let mut j = i + 1;
            while j < code.len() && seq_ops.len() < 7 {
                let insn_j = code[j];
                let a_j = insn_a(insn_j);
                let b_j = insn_b(insn_j);
                let c_j = insn_c(insn_j);
                let op_j = insn_op(insn_j);

                // Monotonic A (exact +1 per step)
                let expected_a = a_i.checked_add((j - i) as u8);
                if expected_a.map_or(true, |e| a_j != e) { break; }
                // Same B
                if b_j != b_i { break; }
                // Must have valid A/B registers
                if a_j >= proto.max_stack_size || b_j >= proto.max_stack_size { break; }
                // Unique byte (detector relies on distinct arith ops)
                if seq_ops.contains(&op_j) { break; }
                // Don't include bytes already mapped to NON-arith standard ops
                if ctx.is_mapped(op_j) {
                    let std_op = ctx.map[op_j as usize];
                    let is_reg_arith = reg_arith_ops.contains(&std_op);
                    let is_k_arith = k_arith_ops.contains(&std_op);
                    if !is_reg_arith && !is_k_arith { break; }
                }

                seq_ops.push(op_j);
                seq_c.push(c_j);
                j += 1;
            }

            if seq_ops.len() < 5 {
                i += 1;
                continue;
            }

            // Classify sequence: reg-reg vs reg-K
            // Reg-reg: all C values equal AND C is a valid register
            // Reg-K: all C values are valid constant indices pointing to Number constants
            let first_c = seq_c[0];
            let all_c_equal = seq_c.iter().all(|&c| c == first_c);
            let c_is_reg = first_c > 0 && (first_c as usize) < proto.max_stack_size as usize;
            let c_is_const_number = |c: u8| -> bool {
                let ci = c as usize;
                ci < proto.constants.len()
                    && matches!(proto.constants.get(ci), Some(Constant::Number(_)))
            };
            let all_c_const_num = seq_c.iter().all(|&c| c_is_const_number(c));

            let is_reg_seq = all_c_equal && c_is_reg;
            let is_k_seq = all_c_const_num && !is_reg_seq;

            if !is_reg_seq && !is_k_seq {
                i += 1;
                continue;
            }

            let ops: &[u8] = if is_reg_seq { &reg_arith_ops } else { &k_arith_ops };

            // Find an anchor: a byte already mapped to an op in our target set
            let mut anchor_offset: Option<isize> = None;
            for (k, &byte) in seq_ops.iter().enumerate() {
                if ctx.is_mapped(byte) {
                    let std_op = ctx.map[byte as usize];
                    if let Some(pos) = ops.iter().position(|&o| o == std_op) {
                        anchor_offset = Some(pos as isize - k as isize);
                        break;
                    }
                }
            }

            // If no anchor and sequence length < 7, too risky — skip
            // If no anchor but sequence is maximal (7 distinct bytes), assume it starts at 0
            let base = match anchor_offset {
                Some(off) => off,
                None if seq_ops.len() >= 7 => 0,
                None => {
                    i += 1;
                    continue;
                }
            };

            // Assign each sequence byte to its position-implied standard op
            let mut assigned_any = false;
            for (k, &byte) in seq_ops.iter().enumerate() {
                let op_idx = base + k as isize;
                if op_idx < 0 || (op_idx as usize) >= ops.len() { continue; }
                let standard = ops[op_idx as usize];
                if ctx.is_mapped(byte) { continue; }
                if ctx.assigned[standard as usize] { continue; }
                if ctx.try_assign(byte, standard) {
                    assigned_any = true;
                }
            }

            if assigned_any {
                i = j;
            } else {
                i += 1;
            }
        }
    }
}

/// Arithmetic ops: ABC format, A=target, B=left, C=right, all registers
fn detect_arithmetic(chunk: &Chunk, ctx: &mut DetectCtx) {
    let mut candidates: HashMap<u8, usize> = HashMap::new();
    for proto in &chunk.protos {
        for &insn in &proto.code {
            let op = insn_op(insn);
            if ctx.is_mapped(op) { continue; }
            let a = insn_a(insn);
            let b = insn_b(insn);
            let c = insn_c(insn);
            // Arithmetic: all three fields are registers, C > 0 (not MOVE)
            // This constraint ensures we don't match MOVE (which has C=0)
            if a < proto.max_stack_size && b < proto.max_stack_size && c < proto.max_stack_size
                && c > 0
            {
                *candidates.entry(op).or_insert(0) += 1;
            }
        }
    }
    // Arithmetic ops are individually uncommon — each typically < 5% of instructions.
    // Filter out candidates with suspiciously high frequency (likely CALL, GETTABLE, etc.)
    // Be stricter: 3% instead of 10% to avoid false positives
    let max_arith_freq = if ctx.total_insns > 100 { ctx.total_insns / 30 } else { u32::MAX };
    let mut sorted: Vec<_> = candidates.iter()
        .filter(|(&op, &count)| !ctx.is_mapped(op) && count >= 3
            && (count as u32) < max_arith_freq)
        .map(|(&op, &count)| (op, count))
        .collect();
    // Deterministic: byte ascending when counts tie (HashMap iteration noise).
    sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    // Assign in frequency order: ADD, SUB, MUL, DIV, MOD, POW
    // Skip standard opcodes that are already assigned
    let arith_ops = [
        LuauOpcode::Add, LuauOpcode::Sub, LuauOpcode::Mul,
        LuauOpcode::Div, LuauOpcode::Mod, LuauOpcode::Pow,
    ];
    let mut std_idx = 0;
    for &(op, _) in sorted.iter() {
        while std_idx < arith_ops.len() && ctx.assigned[arith_ops[std_idx] as usize] {
            std_idx += 1;
        }
        if std_idx >= arith_ops.len() { break; }
        if ctx.try_assign(op, arith_ops[std_idx] as u8) {
            std_idx += 1;
        }
    }
}

/// MODK together with the JUMPXEQKN that tests its result — a parity pair.
///
/// MODK is unreachable by frequency: it appears once or twice per script, far
/// under detect_arithmetic_k's `count >= 3` floor, and it is byte-identical in
/// shape to every other reg-K arithmetic op. The one thing that separates it is
/// a VM invariant on the value rather than the encoding: `x % k` always lands in
/// `[0, |k|)`. So a compare-against-constant that immediately follows a reg-K
/// arithmetic on the SAME destination register, whose comparand lies inside that
/// range, is a modulo fingerprint — the `if n % 7 == 3` idiom.
///
/// Both halves are claimed together because neither is separable alone, and the
/// pair is claimed BEFORE detect_comparison_jumps_aux: in the parity-test files
/// that detector otherwise takes the JUMPXEQKN byte and labels it JUMPIFNOT or
/// JUMPIFLT, leaving JUMPXEQKN unmapped entirely.
///
/// Gates, all required. C1 is all-instances (one stray shape disqualifies the
/// byte); C3 is a ratio because an unmapped AUX-bearing opcode can desync the
/// walk and manufacture a phantom sighting. Measured over 5 permutations of the
/// corpus: 19 true MODK and 19 true JUMPXEQKN claimed, 0 false positives, and
/// no firing at all on the real Roblox samples.
fn detect_modk_parity_pair(chunk: &Chunk, ctx: &mut DetectCtx) {
    if ctx.find_shuffled(LuauOpcode::ModK as u8).is_some() {
        return;
    }
    // True instruction positions of every still-unmapped byte. Skipping the AUX
    // word of a mapped AUX-bearing opcode keeps arbitrary data out of the shape
    // tests; mirrors the walk in detect_arithmetic_k.
    let mut sites: HashMap<u8, Vec<(usize, usize)>> = HashMap::new();
    for (proto_idx, proto) in chunk.protos.iter().enumerate() {
        let code = &proto.code;
        let mut i = 0usize;
        while i < code.len() {
            let op = insn_op(code[i]);
            if ctx.is_mapped(op) {
                let std_op = LuauOpcode::from_u8(ctx.map[op as usize]);
                if std_op.has_aux() && i + 1 < code.len() { i += 2; } else { i += 1; }
                continue;
            }
            sites.entry(op).or_default().push((proto_idx, i));
            i += 1;
        }
    }

    let divisor_at = |proto: &Proto, insn: u32| -> Option<f64> {
        match proto.constants.get(insn_c(insn) as usize) {
            Some(Constant::Number(v))
                if v.is_finite() && v.fract() == 0.0 && v.abs() >= 2.0 => Some(v.abs()),
            _ => None,
        }
    };

    let mut pairs: Vec<(u8, u8)> = Vec::new();
    for (&x, positions) in &sites {
        // C1: EVERY instance is a plausible `R(A) = R(B) % K(C)` with an
        // integral divisor of magnitude >= 2.
        let all_modulo_shaped = positions.iter().all(|&(pi, i)| {
            let proto = &chunk.protos[pi];
            let insn = proto.code[i];
            insn_a(insn) < proto.max_stack_size
                && insn_b(insn) < proto.max_stack_size
                && divisor_at(proto, insn).is_some()
        });
        if !all_modulo_shaped {
            continue;
        }
        // C2: at least one instance is followed by an AD+AUX compare on the same
        // register whose constant comparand lies in [0, |divisor|).
        for &(pi, i) in positions {
            let proto = &chunk.protos[pi];
            let code = &proto.code;
            if i + 2 >= code.len() { continue; }
            let insn = code[i];
            let cmp = code[i + 1];
            if insn_a(cmp) != insn_a(insn) { continue; }
            let d = insn_d(cmp);
            if d <= 0 { continue; }
            if i + 2 + d as usize > code.len() { continue; }
            let divisor = match divisor_at(proto, insn) { Some(v) => v, None => continue };
            let kidx = (code[i + 2] & 0x00FF_FFFF) as usize;
            let comparand = match proto.constants.get(kidx) {
                Some(Constant::Number(v)) if v.is_finite() => *v,
                _ => continue,
            };
            if !(comparand >= 0.0 && comparand < divisor) { continue; }
            let y = insn_op(cmp);
            if ctx.is_mapped(y) || y == x { continue; }
            // C3: the compare byte must behave like a constant-comparing jump
            // across the whole chunk, not just at this one site.
            let mut total = 0usize;
            let mut number_aux = 0usize;
            for other in &chunk.protos {
                for (j, &w) in other.code.iter().enumerate() {
                    if insn_op(w) != y || j + 1 >= other.code.len() { continue; }
                    total += 1;
                    let ki = (other.code[j + 1] & 0x00FF_FFFF) as usize;
                    if let Some(Constant::Number(_)) = other.constants.get(ki) {
                        number_aux += 1;
                    }
                }
            }
            if total == 0 || number_aux * 100 < total * 80 { continue; }
            pairs.push((x, y));
            break;
        }
    }

    // Ambiguity guard: a joint claim costs two slots if it is wrong, so only act
    // when exactly one byte carries the signature.
    if pairs.len() != 1 {
        return;
    }
    let (x, y) = pairs[0];
    if ctx.try_assign(x, LuauOpcode::ModK as u8) {
        ctx.try_assign(y, LuauOpcode::JumpXEqKN as u8);
    }
}

/// Arithmetic-K ops: ABC format, A=target, B=register, C=constant index
fn detect_arithmetic_k(chunk: &Chunk, ctx: &mut DetectCtx) {
    let mut candidates: HashMap<u8, usize> = HashMap::new();
    for proto in &chunk.protos {
        let code = &proto.code;
        // Walk TRUE instruction positions, skipping the AUX word of an
        // already-mapped AUX-bearing opcode. An AUX word is arbitrary data —
        // a constant index, a register, an import path — so its low byte is
        // some unrelated value, and reading it as an instruction credits
        // whatever byte that happens to be with an arithmetic-K sighting.
        // The shape being matched here is weak enough for that to matter: the
        // test is only "A and B look like registers and C indexes a number",
        // which random data passes often. Mirrors the walk in
        // detect_comparison_jumps_aux and detect_closeupvals.
        //
        // Worth +2 correct byte-slots across seven permutation seeds (two seeds
        // better, none worse) and, more usefully, it stops this detector
        // manufacturing votes for bytes a file does not contain — the ballots
        // in parser::consensus are only as good as the evidence behind them.
        // The real Roblox module's opcode map is unchanged by it.
        let mut i = 0usize;
        while i < code.len() {
            let insn = code[i];
            let op = insn_op(insn);
            if ctx.is_mapped(op) {
                let std_op = LuauOpcode::from_u8(ctx.map[op as usize]);
                if std_op.has_aux() && i + 1 < code.len() { i += 2; } else { i += 1; }
                continue;
            }
            let a = insn_a(insn);
            let b = insn_b(insn);
            let c = insn_c(insn) as usize;
            // ArithK: A,B are registers, C is a constant index pointing to Number
            if a < proto.max_stack_size && b < proto.max_stack_size && c < proto.constants.len() {
                if let Some(Constant::Number(_)) = proto.constants.get(c) {
                    *candidates.entry(op).or_insert(0) += 1;
                }
            }
            i += 1;
        }
    }
    // ArithK ops are individually uncommon — cap at 10% of total instructions
    let max_arithk_freq = if ctx.total_insns > 100 { ctx.total_insns / 10 } else { u32::MAX };
    let mut sorted: Vec<_> = candidates.iter()
        .filter(|(&op, &count)| !ctx.is_mapped(op) && count >= 3
            && (count as u32) < max_arithk_freq)
        .map(|(&op, &count)| (op, count))
        .collect();
    // Deterministic: byte ascending when counts tie (HashMap iteration noise).
    sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    // Assign in frequency order: ADDK, SUBK, MULK, DIVK, MODK, POWK
    // Skip standard opcodes that are already assigned
    let arith_k_ops = [
        LuauOpcode::AddK, LuauOpcode::SubK, LuauOpcode::MulK,
        LuauOpcode::DivK, LuauOpcode::ModK, LuauOpcode::PowK,
    ];
    let mut std_idx = 0;
    for &(op, _) in sorted.iter() {
        while std_idx < arith_k_ops.len() && ctx.assigned[arith_k_ops[std_idx] as usize] {
            std_idx += 1;
        }
        if std_idx >= arith_k_ops.len() { break; }
        if ctx.try_assign(op, arith_k_ops[std_idx] as u8) {
            std_idx += 1;
        }
    }
}

/// Register-Register Arithmetic ops: ABC format, detects SUB, MUL, DIV, MOD, POW
/// These are detected by looking for patterns near detected K-variant arithmetic opcodes.
/// Since SUB=34 follows ADD=33 and SUBK=40 follows ADDK=39, we look for shuffled bytes
/// that appear in clusters near the K-variant opcodes.
fn detect_register_arithmetic(chunk: &Chunk, ctx: &mut DetectCtx) {
    // First, ensure ADDK through POWK are mapped, as we'll use them to infer the locations
    let subk_shuffled = ctx.find_shuffled(LuauOpcode::SubK as u8);
    let mulk_shuffled = ctx.find_shuffled(LuauOpcode::MulK as u8);
    let divk_shuffled = ctx.find_shuffled(LuauOpcode::DivK as u8);
    let modk_shuffled = ctx.find_shuffled(LuauOpcode::ModK as u8);
    let powk_shuffled = ctx.find_shuffled(LuauOpcode::PowK as u8);

    // If most K-variants aren't found yet, this detection pass won't help
    let k_variants_found = [subk_shuffled, mulk_shuffled, divk_shuffled, modk_shuffled, powk_shuffled]
        .iter()
        .filter(|o| o.is_some())
        .count();
    if k_variants_found < 3 {
        return;
    }

    // Precompute per-byte B and C statistics to identify CALL-shaped bytes.
    // CALL (A=func, B=nargs+1, C=nresults+1): B≤8 for ~99% of calls, C≤5 for ~99%.
    // Register arithmetic (A=dest, B=left, C=right): B and C are register indices
    // distributed across 0..max_stack_size. Real arithmetic bytes will sometimes
    // have B>8 or C>6, especially in deeply-nested expressions.
    //
    // KEY: if a byte has C≤5 for 95%+ of instances AND B≤8 for 95%+ of instances,
    // it looks like CALL not arithmetic. This guards against stealing the CALL byte
    // when detect_call above failed (e.g., too few CALL instances in tiny scripts).
    let mut call_shape: HashMap<u8, [u32; 3]> = HashMap::new(); // [b_small, c_small, total]
    for proto in &chunk.protos {
        for &insn in &proto.code {
            let op = insn_op(insn);
            if ctx.is_mapped(op) { continue; }
            let b = insn_b(insn);
            let c = insn_c(insn);
            let a = insn_a(insn);
            if a < proto.max_stack_size && c > 0 {
                let entry = call_shape.entry(op).or_insert([0, 0, 0]);
                entry[2] += 1;
                if b <= 8 { entry[0] += 1; }
                if c <= 5 { entry[1] += 1; }
            }
        }
    }

    let mut candidates: HashMap<u8, usize> = HashMap::new();
    for proto in &chunk.protos {
        for &insn in &proto.code {
            let op = insn_op(insn);
            if ctx.is_mapped(op) { continue; }
            let a = insn_a(insn);
            let b = insn_b(insn);
            let c = insn_c(insn);
            // Register arithmetic: A, B, C are all registers, C > 0 (to exclude MOVE which has C=0)
            // This is identical to detect_arithmetic logic
            if a < proto.max_stack_size && b < proto.max_stack_size && c < proto.max_stack_size
                && c > 0
            {
                *candidates.entry(op).or_insert(0) += 1;
            }
        }
    }

    // Register arithmetic ops are individually uncommon — each typically < 5% of instructions.
    let max_arith_freq = if ctx.total_insns > 100 { ctx.total_insns / 30 } else { u32::MAX };
    let mut sorted: Vec<_> = candidates.iter()
        .filter(|(&op, &count)| {
            if ctx.is_mapped(op) { return false; }
            if count < 3 { return false; }
            if (count as u32) >= max_arith_freq { return false; }
            // Reject bytes with CALL-shaped B/C distribution.
            // If ≥95% of instances have B≤8 AND ≥95% have C≤5, the byte is
            // more consistent with CALL (bounded arg/result counts) than with
            // register arithmetic (which uses registers across the full stack).
            if let Some(cs) = call_shape.get(&op) {
                let total = cs[2];
                if total >= 10 {
                    let b_small_pct = cs[0] * 100 / total;
                    let c_small_pct = cs[1] * 100 / total;
                    if b_small_pct >= 95 && c_small_pct >= 95 {
                        return false; // looks like CALL, not arithmetic
                    }
                }
            }
            true
        })
        .map(|(&op, &count)| (op, count))
        .collect();
    // Deterministic: byte ascending when counts tie (HashMap iteration noise).
    sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    // Try to assign SUB, MUL, DIV, MOD, POW in frequency order
    let reg_arith_ops = [
        LuauOpcode::Sub, LuauOpcode::Mul, LuauOpcode::Div,
        LuauOpcode::Mod, LuauOpcode::Pow,
    ];
    let mut std_idx = 0;
    for &(op, _) in sorted.iter() {
        while std_idx < reg_arith_ops.len() && ctx.assigned[reg_arith_ops[std_idx] as usize] {
            std_idx += 1;
        }
        if std_idx >= reg_arith_ops.len() { break; }
        if ctx.try_assign(op, reg_arith_ops[std_idx] as u8) {
            std_idx += 1;
        }
    }
}

/// Dedicated detectors for NOT and MINUS unary ops
/// NOT = 50, MINUS = 51 (or Minus in enum)
/// Format: AB with C=0, used as R(A) = op(R(B))
fn detect_unary_not_minus(chunk: &Chunk, ctx: &mut DetectCtx) {
    // Require MOVE to be mapped first to avoid confusion
    let move_mapped = ctx.find_shuffled(LuauOpcode::Move as u8).is_some();
    if !move_mapped {
        return;
    }

    // Collect candidates with per-instance context info for validation.
    // For each (op, instance), track:
    //   - a, b registers
    //   - proto index + pc (to look forward/backward)
    // Then after collecting, score each candidate by how many instances have
    // "real unary" context: target register consumed by a later arithmetic or
    // comparison op, OR source register produced by an earlier numeric-loading
    // op. Candidates with no such context are rejected — much safer than
    // assigning MINUS/NOT blindly and corrupting module-level registers when
    // the real MINUS byte has zero usage in a given script.
    let mut instances: HashMap<u8, Vec<(usize, usize, u8, u8)>> = HashMap::new();
    for (pi, proto) in chunk.protos.iter().enumerate() {
        for (pc, &insn) in proto.code.iter().enumerate() {
            let op = insn_op(insn);
            if ctx.is_mapped(op) { continue; }
            let a = insn_a(insn);
            let b = insn_b(insn);
            let c = insn_c(insn);
            if a < proto.max_stack_size && b < proto.max_stack_size && c == 0 && a != b {
                instances.entry(op).or_default().push((pi, pc, a, b));
            }
        }
    }

    // STRICT numeric consumers — ops where a register input is guaranteed
    // to be used as a NUMBER. Do NOT include SetTableKS (uses A as table),
    // Return (just reads registers), Concat (string), or comparison jumps
    // (those can compare any type). Only pure arithmetic ops qualify.
    let numeric_consumers: [u8; 14] = [
        ctx.find_shuffled(LuauOpcode::Add as u8).unwrap_or(255),
        ctx.find_shuffled(LuauOpcode::Sub as u8).unwrap_or(255),
        ctx.find_shuffled(LuauOpcode::Mul as u8).unwrap_or(255),
        ctx.find_shuffled(LuauOpcode::Div as u8).unwrap_or(255),
        ctx.find_shuffled(LuauOpcode::Mod as u8).unwrap_or(255),
        ctx.find_shuffled(LuauOpcode::Pow as u8).unwrap_or(255),
        ctx.find_shuffled(LuauOpcode::IDiv as u8).unwrap_or(255),
        ctx.find_shuffled(LuauOpcode::AddK as u8).unwrap_or(255),
        ctx.find_shuffled(LuauOpcode::SubK as u8).unwrap_or(255),
        ctx.find_shuffled(LuauOpcode::MulK as u8).unwrap_or(255),
        ctx.find_shuffled(LuauOpcode::DivK as u8).unwrap_or(255),
        ctx.find_shuffled(LuauOpcode::ModK as u8).unwrap_or(255),
        ctx.find_shuffled(LuauOpcode::PowK as u8).unwrap_or(255),
        ctx.find_shuffled(LuauOpcode::IDivK as u8).unwrap_or(255),
    ];
    let loadn_byte = ctx.find_shuffled(LuauOpcode::LoadN as u8).unwrap_or(255);
    let loadk_byte = ctx.find_shuffled(LuauOpcode::LoadK as u8).unwrap_or(255);

    let is_numeric_consumer = |op: u8| -> bool {
        op != 255 && numeric_consumers.contains(&op)
    };
    let is_numeric_producer = |op: u8| -> bool {
        op != 255 && (op == loadn_byte || op == loadk_byte || numeric_consumers.contains(&op))
    };

    // For each candidate, count how many instances show real unary context.
    // Context = target register `a` is read by a numeric consumer within the
    // next ~8 instructions, OR source register `b` was written by a numeric
    // producer in the previous ~8 instructions.
    let mut scored: Vec<(u8, usize, usize)> = Vec::new(); // (op, total_count, context_hits)
    for (&op, insts) in instances.iter() {
        let total = insts.len();
        let mut ctx_hits = 0usize;
        for &(pi, pc, a, b) in insts.iter() {
            let proto = &chunk.protos[pi];
            let code = &proto.code;
            // Forward scan: does a later insn consume R(a)?
            let end = (pc + 8).min(code.len());
            let mut fwd_ok = false;
            for fpc in (pc + 1)..end {
                let fins = code[fpc];
                let fop = insn_op(fins);
                if !is_numeric_consumer(fop) { continue; }
                let fa = insn_a(fins);
                let fb = insn_b(fins);
                let fc = insn_c(fins);
                if fa == a || fb == a || fc == a {
                    fwd_ok = true;
                    break;
                }
            }
            // Backward scan: was R(b) produced by a numeric-producing insn?
            let start = pc.saturating_sub(8);
            let mut bwd_ok = false;
            for bpc in start..pc {
                let bins = code[bpc];
                let bop = insn_op(bins);
                if !is_numeric_producer(bop) { continue; }
                if insn_a(bins) == b {
                    bwd_ok = true;
                    break;
                }
            }
            if fwd_ok || bwd_ok {
                ctx_hits += 1;
            }
        }
        scored.push((op, total, ctx_hits));
    }

    // Unary ops are rare — typically < 5% of instructions
    let max_unary_freq = if ctx.total_insns > 100 { ctx.total_insns / 20 } else { 50 };

    // Strict filter: real unary candidates need
    //   (1) total >= 3 instances (avoids coincidental small counts)
    //   (2) within frequency cap
    //   (3) ctx_hits >= 2 (at least two independent confirmations)
    //   (4) ctx_hits * 2 >= total (majority of instances show real context)
    // A byte that fails the context check is almost certainly a misdetection
    // and must not be assigned to MINUS/NOT — better to leave unmapped than
    // to corrupt output with `(-tbl).field = ...` patterns.
    let mut viable: Vec<(u8, usize, usize)> = scored.into_iter()
        .filter(|&(op, total, ctx_hits)| {
            !ctx.is_mapped(op)
                && total >= 3
                && (total as u32) <= max_unary_freq
                && ctx_hits >= 2
                && ctx_hits * 2 >= total
        })
        .collect();
    // Prefer candidates with higher context ratio, tiebreak by raw count, then byte value.
    // Integer-scaled ratio avoids f64 NaN comparison pitfalls and produces deterministic
    // ordering even when counts are equal (byte ascending as final tiebreak).
    viable.sort_by(|a, b| {
        let ra = if a.1 > 0 { (a.2 * 1000) / a.1 } else { 0 };
        let rb = if b.1 > 0 { (b.2 * 1000) / b.1 } else { 0 };
        rb.cmp(&ra)
            .then(b.1.cmp(&a.1))
            .then(a.0.cmp(&b.0))
    });

    // Try to assign NOT and MINUS (in that order, as MINUS is slightly rarer).
    // Don't assign LENGTH here — let detect_unary_ops handle that.
    let unary_ops = [LuauOpcode::Not, LuauOpcode::Minus];
    let mut std_idx = 0;
    for &(op, _total, _hits) in viable.iter() {
        while std_idx < unary_ops.len() && ctx.assigned[unary_ops[std_idx] as usize] {
            std_idx += 1;
        }
        if std_idx >= unary_ops.len() { break; }
        ctx.try_assign(op, unary_ops[std_idx] as u8);
        std_idx += 1;
    }

    // --- Rare-single-instance fallback ---
    // Some modules use MINUS/NOT exactly once each (e.g. `local neg = -x` in a
    // small helper function). The strict `total >= 3` filter above excludes
    // these entirely. For the rare case, accept total in [1..2] only if EVERY
    // instance of the candidate has full numeric-context confirmation
    // (ctx_hits == total). This preserves the "prefer UNMAPPED over WRONG"
    // rule: if even one instance looks non-numeric, we bail.
    if !ctx.assigned[LuauOpcode::Not as usize] || !ctx.assigned[LuauOpcode::Minus as usize] {
        let mut rare: Vec<(u8, usize, usize)> = Vec::new();
        for (&op, insts) in instances.iter() {
            if ctx.is_mapped(op) { continue; }
            let total = insts.len();
            if total == 0 || total > 2 { continue; }
            // Re-score with same logic as main path
            let mut ctx_hits = 0usize;
            for &(pi, pc, a, b) in insts.iter() {
                let proto = &chunk.protos[pi];
                let code = &proto.code;
                let end = (pc + 8).min(code.len());
                let mut fwd_ok = false;
                for fpc in (pc + 1)..end {
                    let fins = code[fpc];
                    let fop = insn_op(fins);
                    if !is_numeric_consumer(fop) { continue; }
                    let fa = insn_a(fins);
                    let fb = insn_b(fins);
                    let fc = insn_c(fins);
                    if fa == a || fb == a || fc == a {
                        fwd_ok = true;
                        break;
                    }
                }
                let start = pc.saturating_sub(8);
                let mut bwd_ok = false;
                for bpc in start..pc {
                    let bins = code[bpc];
                    let bop = insn_op(bins);
                    if !is_numeric_producer(bop) { continue; }
                    if insn_a(bins) == b {
                        bwd_ok = true;
                        break;
                    }
                }
                if fwd_ok || bwd_ok { ctx_hits += 1; }
            }
            if ctx_hits == total {
                rare.push((op, total, ctx_hits));
            }
        }
        // Deterministic: sort by total desc (prefer 2-instance over 1-instance),
        // then byte ascending.
        rare.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

        // Only assign if we have at most 2 viable rare candidates (to avoid
        // picking randomly from a crowded pool). If >2 match, we can't
        // distinguish MINUS from NOT reliably by single-instance data alone.
        if rare.len() <= 2 {
            let mut std_idx = 0;
            for &(op, _total, _hits) in rare.iter() {
                while std_idx < unary_ops.len() && ctx.assigned[unary_ops[std_idx] as usize] {
                    std_idx += 1;
                }
                if std_idx >= unary_ops.len() { break; }
                ctx.try_assign(op, unary_ops[std_idx] as u8);
                std_idx += 1;
            }
        }
    }
}

/// LENGTH ONLY: AB format (A=target, B=source, C=0), numeric-consumer validated.
///
/// Runs ONLY as a fallback for LENGTH after `detect_unary_not_minus` has had its
/// shot. We do NOT assign NOT or MINUS here: `detect_unary_not_minus` already
/// applies context validation (numeric consumers + viable path), and if it
/// declined to assign them, picking from raw frequency here would silently
/// overwrite with the wrong byte — which is exactly how 0xF6 was getting tagged
/// as MINUS on the ground-truth ModuleScript when the real MINUS is 0x39.
///
/// LENGTH is a slightly weaker case because its result is always numeric AND
/// it's commonly used on strings/tables — we confirm the result register R(A)
/// is consumed by a numeric sink (arithmetic, comparison, numeric-for prep,
/// FASTCALL builtin argument) within 8 instructions. Without that confirmation,
/// we leave it unmapped — following the "prefer UNMAPPED over WRONG" rule.
fn detect_unary_ops(chunk: &Chunk, ctx: &mut DetectCtx) {
    // First ensure MOVE, GETUPVAL, SETUPVAL are mapped to avoid false positives.
    let move_mapped = ctx.find_shuffled(LuauOpcode::Move as u8).is_some();
    let getupval_mapped = ctx.find_shuffled(LuauOpcode::GetUpval as u8).is_some();
    if !move_mapped || !getupval_mapped {
        return;
    }

    // Skip entirely if LENGTH is already mapped — nothing to do.
    if ctx.assigned[LuauOpcode::Length as usize] {
        return;
    }

    // Pre-resolve numeric-consumer opcode bytes we'll use to validate R(A).
    let numeric_consumers: Vec<u8> = [
        LuauOpcode::Add, LuauOpcode::Sub, LuauOpcode::Mul,
        LuauOpcode::Div, LuauOpcode::Mod, LuauOpcode::Pow, LuauOpcode::IDiv,
        LuauOpcode::AddK, LuauOpcode::SubK, LuauOpcode::MulK,
        LuauOpcode::DivK, LuauOpcode::ModK, LuauOpcode::PowK, LuauOpcode::IDivK,
        LuauOpcode::JumpIfLT, LuauOpcode::JumpIfLE, LuauOpcode::JumpIfNotLT, LuauOpcode::JumpIfNotLE,
        LuauOpcode::JumpXEqKN, LuauOpcode::ForNPrep, LuauOpcode::ForNLoop,
    ].iter().filter_map(|o| ctx.find_shuffled(*o as u8)).collect();

    // Candidate: ABC op with c == 0 and a != b, counted with ctx_hits where
    // some numeric consumer reads R(A) within 8 instructions after the op.
    // (total, ctx_hits)
    let mut candidates: HashMap<u8, (usize, usize)> = HashMap::new();
    for proto in &chunk.protos {
        let code = &proto.code;
        for i in 0..code.len() {
            let insn = code[i];
            let op = insn_op(insn);
            if ctx.is_mapped(op) { continue; }
            let a = insn_a(insn);
            let b = insn_b(insn);
            let c = insn_c(insn);
            if !(a < proto.max_stack_size && b < proto.max_stack_size && c == 0 && a != b) {
                continue;
            }
            let entry = candidates.entry(op).or_insert((0, 0));
            entry.0 += 1;

            // Scan up to 8 following instructions for a numeric consumer reading R(a).
            let end = (i + 9).min(code.len());
            for j in (i + 1)..end {
                let ni = code[j];
                let nop = insn_op(ni);
                if !numeric_consumers.contains(&nop) { continue; }
                let na = insn_a(ni);
                let nb = insn_b(ni);
                let nc = insn_c(ni);
                if nb == a || nc == a || na == a {
                    entry.1 += 1;
                    break;
                }
            }
        }
    }

    // Cap on frequency — LENGTH is rarely more than ~2% of all instructions.
    let max_length_freq = if ctx.total_insns > 100 { ctx.total_insns / 50 } else { 20 };

    // Gate: total >= 3, ctx_hits >= 2, ctx_hits*2 >= total, within frequency cap.
    // These are the same constraints detect_unary_not_minus applies — a byte
    // that clears them is almost certainly a real unary op.
    let mut viable: Vec<(u8, usize, usize)> = candidates.iter()
        .filter(|(&op, &(total, ctx_hits))| {
            !ctx.is_mapped(op)
                && total >= 3
                && (total as u32) <= max_length_freq
                && ctx_hits >= 2
                && ctx_hits * 2 >= total
        })
        .map(|(&op, &(t, h))| (op, t, h))
        .collect();

    // Sort by integer-scaled ctx ratio, then raw count, then byte (ascending) as tiebreak.
    viable.sort_by(|a, b| {
        let ra = if a.1 > 0 { (a.2 * 1000) / a.1 } else { 0 };
        let rb = if b.1 > 0 { (b.2 * 1000) / b.1 } else { 0 };
        rb.cmp(&ra)
            .then(b.1.cmp(&a.1))
            .then(a.0.cmp(&b.0))
    });

    if let Some(&(op, _, _)) = viable.first() {
        ctx.try_assign(op, LuauOpcode::Length as u8);
        return;
    }

    // --- Rare-single-instance fallback for LENGTH ---
    // Same idea as detect_unary_not_minus's rare path: accept 1-2 instance
    // candidates where EVERY occurrence has numeric-consumer context. Guards
    // against the "logical" function in ground_truth_module.lua where
    // `local len = #s` is the sole LENGTH in the entire module.
    let mut rare: Vec<(u8, usize, usize)> = candidates.iter()
        .filter(|(&op, &(total, ctx_hits))| {
            !ctx.is_mapped(op)
                && total >= 1 && total <= 2
                && ctx_hits == total
                && (total as u32) <= max_length_freq
        })
        .map(|(&op, &(t, h))| (op, t, h))
        .collect();
    rare.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    // Only assign from a small pool — more than 2 rare candidates means we
    // can't tell LENGTH apart from other unmapped single-instance ops.
    if rare.len() <= 2 {
        if let Some(&(op, _, _)) = rare.first() {
            ctx.try_assign(op, LuauOpcode::Length as u8);
        }
    }
}

/// Phase B0.78: Post-augmenter LENGTH rescue detector.
///
/// When LENGTH (52) isn't detected by the normal pipeline, it blocks the
/// entire RBX_EXT cascade (14+ opcodes) because `detect_rbx_ext_ops`
/// requires Not+Minus+Length as prerequisites. This rescue runs after the
/// augmenter, when NOT (50) and MINUS (51) are already mapped, so the
/// unary C=0 candidate pool is much smaller.
///
/// Key differences from `detect_unary_ops`:
/// - Wider consumer window (16 instructions instead of 8)
/// - Broader consumer set: includes CALL, RETURN, comparison jumps
///   (LENGTH result often used as `if #t > 0`, `foo(#t)`, `return #t`)
/// - Relaxed threshold: ≥1 instance with ≥1/3 consumer hits
/// - Accepts broader frequency range (3.3% vs 2%)
fn detect_length_rescue(chunk: &Chunk, ctx: &mut DetectCtx) {
    if ctx.assigned[LuauOpcode::Length as usize] {
        return;
    }
    let move_mapped = ctx.find_shuffled(LuauOpcode::Move as u8).is_some();
    let getupval_mapped = ctx.find_shuffled(LuauOpcode::GetUpval as u8).is_some();
    if !move_mapped || !getupval_mapped {
        return;
    }

    // Broader consumer set than detect_unary_ops — LENGTH results are commonly
    // used in comparisons (`if #t > 0`), passed to calls (`foo(#t)`), or returned.
    let consumer_ops: &[LuauOpcode] = &[
        // Arithmetic (standard LENGTH consumers)
        LuauOpcode::Add, LuauOpcode::Sub, LuauOpcode::Mul,
        LuauOpcode::Div, LuauOpcode::Mod, LuauOpcode::Pow, LuauOpcode::IDiv,
        LuauOpcode::AddK, LuauOpcode::SubK, LuauOpcode::MulK,
        LuauOpcode::DivK, LuauOpcode::ModK, LuauOpcode::PowK, LuauOpcode::IDivK,
        // Comparison jumps (common: `if #t > 0 then`, `if #t == n then`)
        LuauOpcode::JumpIfLT, LuauOpcode::JumpIfLE,
        LuauOpcode::JumpIfNotLT, LuauOpcode::JumpIfNotLE,
        LuauOpcode::JumpXEqKN,
        // Numeric for (`for i = 1, #t do`)
        LuauOpcode::ForNPrep,
        // CALL: LENGTH result as function argument (`foo(#t)`)
        LuauOpcode::Call,
        // RETURN: returning LENGTH result (`return #t`)
        LuauOpcode::Return,
        // FASTCALL: built-in function with LENGTH arg
        LuauOpcode::FastCall1, LuauOpcode::FastCall2,
    ];
    let consumer_bytes: Vec<u8> = consumer_ops.iter()
        .filter_map(|o| ctx.find_shuffled(*o as u8))
        .collect();
    if consumer_bytes.is_empty() {
        return;
    }

    // Scan for unary-shaped (C=0, A!=B) unmapped bytes with consumer context.
    let mut candidates: HashMap<u8, (usize, usize)> = HashMap::new();
    for proto in &chunk.protos {
        let code = &proto.code;
        for i in 0..code.len() {
            let insn = code[i];
            let op = insn_op(insn);
            if ctx.is_mapped(op) { continue; }
            let a = insn_a(insn);
            let b = insn_b(insn);
            let c = insn_c(insn);
            if !(a < proto.max_stack_size && b < proto.max_stack_size && c == 0 && a != b) {
                continue;
            }
            let entry = candidates.entry(op).or_insert((0, 0));
            entry.0 += 1;

            // Wider consumer window: 16 instructions (vs 8 in detect_unary_ops)
            let end = (i + 17).min(code.len());
            for j in (i + 1)..end {
                let ni = code[j];
                let nop = insn_op(ni);
                if !consumer_bytes.contains(&nop) { continue; }
                let na = insn_a(ni);
                let nb = insn_b(ni);
                let nc = insn_c(ni);
                if nb == a || nc == a || na == a {
                    entry.1 += 1;
                    break;
                }
            }
        }
    }

    // Relaxed frequency cap: 3.3% (vs 2% in detect_unary_ops)
    let max_freq = if ctx.total_insns > 100 { ctx.total_insns / 30 } else { 30 };

    let mut viable: Vec<(u8, usize, usize)> = candidates.iter()
        .filter(|(&op, &(total, ctx_hits))| {
            !ctx.is_mapped(op)
                && total >= 1
                && (total as u32) <= max_freq
                && ctx_hits >= 1
                && ctx_hits * 3 >= total  // at least 1/3 have consumer context
        })
        .map(|(&op, &(t, h))| (op, t, h))
        .collect();

    // Sort by context ratio descending, then count, then byte value
    viable.sort_by(|a, b| {
        let ra = if a.1 > 0 { (a.2 * 1000) / a.1 } else { 0 };
        let rb = if b.1 > 0 { (b.2 * 1000) / b.1 } else { 0 };
        rb.cmp(&ra)
            .then(b.1.cmp(&a.1))
            .then(a.0.cmp(&b.0))
    });

    if let Some(&(op, total, hits)) = viable.first() {
        eprintln!("  LENGTH rescue: assigning 0x{:02X} ({} instances, {}/{} consumer hits)",
            op, total, hits, total);
        ctx.try_assign(op, LuauOpcode::Length as u8);
    }
}

/// Phase B0.44B: Extended-scan fallback for single-hit NOT / MINUS / LENGTH.
///
/// `detect_unary_not_minus` and `detect_unary_ops` scan only 8 instructions
/// ahead of an ABC-unary-shaped candidate looking for a numeric consumer of
/// R(A). That window is insufficient for patterns like ModuleScript.luac's
/// `M.logical` — a MINUS at pc=0 whose result feeds a JumpIfLE at pc=10 after
/// intervening LENGTH/NOT/AND/OR/ANDK/ORK (the `return notx or (neg < len)`
/// expression).
///
/// This detector walks the ENTIRE enclosing proto (capped at 200 PCs for
/// safety). To avoid stealing common ABC bytes whose result happens to be
/// used downstream, it applies extra structural guards:
///
/// - Every instance must be the FIRST occurrence of its `R(A)` in the proto
///   (R(A) isn't written by a preceding instruction). This captures the
///   "fresh-register unary result" signature.
/// - B < num_params (source register is a function parameter), so we know
///   the source is a real value at runtime — not a throwaway temporary.
/// - Source register R(B) must NOT be written later between the unary and
///   its consumer (the consumer sees the unary result, not a re-assign).
///
/// Targets: Not (50), Minus (51), Length (52). Runs AFTER the core unary
/// detectors so it only fires when they declined.
///
/// Currently unused — kept for future refinement. When enabled, even with
/// the structural guards the fixed MINUS/NOT/LENGTH assignment order mis-
/// paired real bytes on small modules (ModuleScript.luac put NOT at the byte
/// that actually is MINUS). Re-enabling requires per-opcode structural
/// signatures (NOT feeds short-circuit / return, MINUS + LENGTH feed
/// arithmetic/comparison) rather than a positional order.
#[allow(dead_code)]
fn detect_rare_unary_extended_scan(chunk: &Chunk, ctx: &mut DetectCtx) {
    // Require MOVE mapped to give the format test meaning (without MOVE pinned,
    // every A!=B,C=0 byte in the chunk looks unary).
    let move_mapped = ctx.find_shuffled(LuauOpcode::Move as u8).is_some();
    if !move_mapped {
        return;
    }

    // Done if ALL three targets are already mapped.
    let want_not = !ctx.assigned[LuauOpcode::Not as usize];
    let want_minus = !ctx.assigned[LuauOpcode::Minus as usize];
    let want_length = !ctx.assigned[LuauOpcode::Length as usize];
    if !want_not && !want_minus && !want_length {
        return;
    }

    // Collect byte -> Vec<(proto_idx, pc, a, b)> for unary-shaped unmapped bytes.
    // Strict format: C=0, A!=B, A<max_stack, B<max_stack, AND B < num_params
    // (source must be a parameter, narrowing false positives from ABC bytes
    // that happen to read/write arbitrary registers).
    let mut instances: HashMap<u8, Vec<(usize, usize, u8, u8)>> = HashMap::new();
    for (pi, proto) in chunk.protos.iter().enumerate() {
        for (pc, &insn) in proto.code.iter().enumerate() {
            let op = insn_op(insn);
            if ctx.is_mapped(op) { continue; }
            let a = insn_a(insn);
            let b = insn_b(insn);
            let c = insn_c(insn);
            if c != 0 { continue; }
            if a == b { continue; }
            if a >= proto.max_stack_size || b >= proto.max_stack_size { continue; }
            // Stricter: source must be a parameter register (0 <= B < num_params).
            // A unary on a non-parameter source can still fire but is much rarer;
            // the parameter-source signature is a strong unary marker.
            if proto.num_params == 0 || b >= proto.num_params { continue; }
            instances.entry(op).or_default().push((pi, pc, a, b));
        }
    }

    // Consumers that prove R(A) is used in a numeric / comparison / return context.
    let numeric_consumer_bytes: Vec<u8> = [
        LuauOpcode::Add, LuauOpcode::Sub, LuauOpcode::Mul,
        LuauOpcode::Div, LuauOpcode::Mod, LuauOpcode::Pow, LuauOpcode::IDiv,
        LuauOpcode::AddK, LuauOpcode::SubK, LuauOpcode::MulK,
        LuauOpcode::DivK, LuauOpcode::ModK, LuauOpcode::PowK, LuauOpcode::IDivK,
        LuauOpcode::JumpIfLT, LuauOpcode::JumpIfLE,
        LuauOpcode::JumpIfNotLT, LuauOpcode::JumpIfNotLE,
        LuauOpcode::JumpIfEq, LuauOpcode::JumpIfNotEq,
        LuauOpcode::JumpXEqKN, LuauOpcode::JumpXEqKS, LuauOpcode::JumpXEqKB,
        LuauOpcode::ForNPrep, LuauOpcode::ForNLoop,
        LuauOpcode::Concat,
    ]
    .iter()
    .filter_map(|o| ctx.find_shuffled(*o as u8))
    .collect();
    if numeric_consumer_bytes.is_empty() { return; }

    // Frequency cap: unary ops are rare — strictly < 2% of all instructions.
    let max_freq: u32 = if ctx.total_insns > 100 { ctx.total_insns / 50 } else { 50 };

    // Helper: does any instruction in `code[range]` write register `r`?
    // We only consider WRITES to the destination field A. CALL writes multiple
    // registers but we conservatively treat it as "may write R(a..a+c)".
    let writes_reg = |code: &[u32], range: std::ops::Range<usize>, r: u8| -> bool {
        for pc in range {
            let insn = code[pc];
            let op = insn_op(insn);
            let a = insn_a(insn);
            // Treat any unmapped byte as "may write" — conservative.
            if !ctx.is_mapped(op) {
                if a == r { return true; }
                continue;
            }
            // Mapped: check by canonical opcode. Destination ops write R(A).
            let std = ctx.map[op as usize];
            let luau = LuauOpcode::from_u8(std);
            match luau {
                // Writes R(A)
                LuauOpcode::Move | LuauOpcode::LoadNil | LuauOpcode::LoadB
                | LuauOpcode::LoadN | LuauOpcode::LoadK | LuauOpcode::LoadKX
                | LuauOpcode::GetGlobal | LuauOpcode::GetUpval
                | LuauOpcode::GetImport | LuauOpcode::GetTable
                | LuauOpcode::GetTableKS | LuauOpcode::GetTableN
                | LuauOpcode::NewClosure | LuauOpcode::NameCall
                | LuauOpcode::Not | LuauOpcode::Minus | LuauOpcode::Length
                | LuauOpcode::NewTable | LuauOpcode::DupTable
                | LuauOpcode::GetVarargs
                | LuauOpcode::Add | LuauOpcode::Sub | LuauOpcode::Mul
                | LuauOpcode::Div | LuauOpcode::Mod | LuauOpcode::Pow
                | LuauOpcode::IDiv
                | LuauOpcode::AddK | LuauOpcode::SubK | LuauOpcode::MulK
                | LuauOpcode::DivK | LuauOpcode::ModK | LuauOpcode::PowK
                | LuauOpcode::IDivK
                | LuauOpcode::And | LuauOpcode::Or | LuauOpcode::AndK | LuauOpcode::OrK
                | LuauOpcode::Concat | LuauOpcode::SubRK | LuauOpcode::DivRK
                | LuauOpcode::DupClosure
                | LuauOpcode::Band | LuauOpcode::Bor | LuauOpcode::Bxor
                | LuauOpcode::Bnot | LuauOpcode::Shl | LuauOpcode::Shr
                | LuauOpcode::Bandk | LuauOpcode::Bork
                    => { if a == r { return true; } }
                // CALL writes R(A)..R(A+C-2); conservatively if a == r, a write.
                LuauOpcode::Call => { if a == r { return true; } }
                // Non-writes (jumps, stores, etc.)
                _ => {}
            }
        }
        false
    };

    // For each candidate byte, require EVERY instance to have a consumer AND
    // no intervening re-assign of either R(A) or R(B) between the unary and
    // its first consumer.
    let mut viable: Vec<(u8, usize)> = Vec::new();
    for (&op, insts) in instances.iter() {
        let total = insts.len();
        if total == 0 { continue; }
        if (total as u32) > max_freq { continue; }
        if ctx.freq[op as usize] > max_freq { continue; }
        let mut all_ok = true;
        for &(pi, pc, a, b) in insts {
            let code = &chunk.protos[pi].code;
            // Verify R(A) is fresh (not written by any preceding instruction).
            if writes_reg(code, 0..pc, a) { all_ok = false; break; }
            let end = (pc + 201).min(code.len());
            let mut found_at: Option<usize> = None;
            for fpc in (pc + 1)..end {
                let fins = code[fpc];
                let fop = insn_op(fins);
                if !numeric_consumer_bytes.contains(&fop) { continue; }
                let fa = insn_a(fins);
                let fb = insn_b(fins);
                let fc = insn_c(fins);
                if fa == a || fb == a || fc == a {
                    found_at = Some(fpc);
                    break;
                }
                // AUX-reading comparison jumps: check AUX low byte against R(A).
                let std = ctx.map[fop as usize];
                if std != 255 {
                    let op_std = LuauOpcode::from_u8(std);
                    if op_std.has_aux() && fpc + 1 < code.len() {
                        let aux = code[fpc + 1];
                        if (aux & 0xFF) as u8 == a {
                            found_at = Some(fpc);
                            break;
                        }
                    }
                }
            }
            let Some(consumer_pc) = found_at else { all_ok = false; break; };
            // Source R(B) must not be overwritten between the unary and its consumer.
            if writes_reg(code, (pc + 1)..consumer_pc, b) { all_ok = false; break; }
        }
        if all_ok {
            viable.push((op, total));
        }
    }

    // Deterministic: prefer higher total (multi-hit is stronger), then byte ascending.
    viable.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    if std::env::var("OPMAP_TRACE_B44B").is_ok() {
        eprintln!("[B44B unary] viable: {:?}", viable);
        eprintln!("[B44B unary] consumer_bytes: {:?}", numeric_consumer_bytes);
    }

    // Bail if more than 3 viable bytes — we can't reliably distinguish NOT / MINUS / LENGTH
    // from each other when many bytes share the pattern.
    if viable.len() > 3 { return; }

    // Assign in fixed order: MINUS first (often rarest and first-insn at Proto 2), then NOT,
    // then LENGTH. This ordering matches the ground-truth Luau patterns on small modules
    // where `-x` comes before `not x` / `#s` in the layout.
    let order = [LuauOpcode::Minus, LuauOpcode::Not, LuauOpcode::Length];
    let mut oi = 0;
    for &(op, _) in &viable {
        while oi < order.len() && ctx.assigned[order[oi] as usize] { oi += 1; }
        if oi >= order.len() { break; }
        if ctx.try_assign(op, order[oi] as u8) {
            oi += 1;
        }
    }
}

/// Phase B0.44B: Single-hit JUMPXEQKB / JUMPXEQKNIL detector via return-target
/// structural validation.
///
/// `detect_jumpxeq` requires `hits >= 2` for KB and KNil because the AUX
/// constraint (`aux_low31 <= 1`) is so permissive it would map random
/// LOADK/MOVE bytes whose AUX happens to be 0 or 1. A single-hit assignment
/// is unsafe in general.
///
/// This stricter detector accepts a SINGLE hit when the structural context
/// strongly confirms an XEQ comparison:
///   1. Byte has AD format: A < max_stack, D > 0 (forward jump).
///   2. Target PC `pc + d + 1` is in-bounds AND within 6 PCs.
///   3. Body (pc+2 .. target_pc) contains a mapped RETURN.
///   4. AUX word is EXACTLY 0x00000000 (→ KNil) or EXACTLY 0x00000001 (→ KB).
///      The full 32-bit word must match — upper bits being set indicates a
///      different AUX format (e.g., register-comparison AUX with NOT flag).
///   5. The byte appears AT MOST 2 times across the whole chunk at
///      instruction positions. A higher-frequency byte is almost certainly
///      a common comparison jump (JUMPIFNOTLE etc.), not a rare XEQ variant.
///   6. Every instance of the byte must have a valid AUX=0 or AUX=1 plus
///      short-forward-jump-to-RETURN pattern. Mixed patterns reject.
fn detect_xeq_single_hit_return_target(chunk: &Chunk, ctx: &mut DetectCtx) {
    // Require RETURN mapped for target validation.
    let return_byte = match ctx.find_shuffled(LuauOpcode::Return as u8) {
        Some(b) => b,
        None => return,
    };

    // For each unmapped byte, track whether EVERY occurrence at a TRUE
    // instruction position matches exactly one XEQ pattern (KNil or KB).
    //
    // Definitions:
    //   - INSN_COUNT[byte]   = number of true instruction occurrences
    //   - KNIL_MATCH[byte]   = number of occurrences that satisfy KNil pattern
    //   - KB_MATCH[byte]    = number of occurrences that satisfy KB pattern
    //   - OTHER_SHAPE[byte] = number of occurrences that don't match either
    //
    // Assignment rule (per bucket):
    //   1 <= INSN_COUNT[byte] <= 2
    //   KNIL_MATCH[byte] == INSN_COUNT[byte]  OR  KB_MATCH[byte] == INSN_COUNT[byte]
    //   OTHER_SHAPE[byte] == 0
    //   byte not already mapped, target canonical not assigned.
    let mut insn_count = [0u32; 256];
    let mut knil_match = [0u32; 256];
    let mut kb_match = [0u32; 256];
    let mut other_shape = [0u32; 256];

    for proto in &chunk.protos {
        let code = &proto.code;
        let mut i = 0usize;
        while i + 1 < code.len() {
            let insn = code[i];
            let op = insn_op(insn);
            if ctx.is_mapped(op) {
                let std = ctx.map[op as usize];
                let luau = LuauOpcode::from_u8(std);
                if luau.has_aux() && i + 1 < code.len() { i += 2; } else { i += 1; }
                continue;
            }
            insn_count[op as usize] += 1;

            let a = insn_a(insn);
            let d = insn_d(insn) as i32;
            let aux = code[i + 1];

            let mut matched = false;
            if a < proto.max_stack_size && d > 0 {
                let target_pc = i as i32 + d + 1;
                if target_pc > 0 && (target_pc as usize) < code.len() {
                    let dist = (target_pc as usize).saturating_sub(i + 2);
                    if dist <= 5 {
                        let body_start = i + 2;
                        let mut has_return = false;
                        for bpc in body_start..(target_pc as usize) {
                            if insn_op(code[bpc]) == return_byte {
                                has_return = true;
                                break;
                            }
                        }
                        if has_return {
                            // Strict AUX match: low 31 bits == 0 for KNil,
                            // low 31 bits == 1 for KB. Bit 31 is the NOT flag
                            // and may be set (jump when NOT equal).
                            let aux_low31 = aux & 0x7FFFFFFF;
                            if aux_low31 == 0 {
                                knil_match[op as usize] += 1;
                                matched = true;
                            } else if aux_low31 == 1 {
                                kb_match[op as usize] += 1;
                                matched = true;
                            }
                        }
                    }
                }
            }
            if !matched {
                other_shape[op as usize] += 1;
            }

            i += 1;
        }
    }

    // Collect eligible candidates per bucket (deterministic: byte ascending).
    let mut knil_cands: Vec<u8> = Vec::new();
    let mut kb_cands: Vec<u8> = Vec::new();
    for b in 0..=255u8 {
        let total = insn_count[b as usize];
        if total == 0 || total > 2 { continue; }
        if ctx.is_mapped(b) { continue; }
        if other_shape[b as usize] > 0 { continue; }
        if knil_match[b as usize] == total {
            knil_cands.push(b);
        }
        if kb_match[b as usize] == total {
            kb_cands.push(b);
        }
    }

    // If more than 2 bytes are KNil candidates, we can't pick reliably — bail
    // on that bucket. Same for KB.
    if knil_cands.len() <= 2 && !ctx.assigned[LuauOpcode::JumpXEqKNil as usize] {
        if let Some(&op) = knil_cands.first() {
            ctx.try_assign(op, LuauOpcode::JumpXEqKNil as u8);
        }
    }
    if kb_cands.len() <= 2 && !ctx.assigned[LuauOpcode::JumpXEqKB as usize] {
        // Avoid picking a byte we just assigned to KNil.
        let kb_op = kb_cands.iter().copied()
            .find(|&b| ctx.map[b as usize] == 255);
        if let Some(op) = kb_op {
            ctx.try_assign(op, LuauOpcode::JumpXEqKB as u8);
        }
    }
}

/// CONCAT: ABC format, A = target, B = first source, C = last source (B <= C)
fn detect_concat(chunk: &Chunk, ctx: &mut DetectCtx) {
    let mut candidates: HashMap<u8, (usize, usize)> = HashMap::new(); // (valid_count, total_count)
    for proto in &chunk.protos {
        for &insn in &proto.code {
            let op = insn_op(insn);
            if ctx.is_mapped(op) { continue; }
            let a = insn_a(insn);
            let b = insn_b(insn);
            let c = insn_c(insn);
            if a < proto.max_stack_size && b < proto.max_stack_size && c < proto.max_stack_size {
                let entry = candidates.entry(op).or_insert((0, 0));
                entry.1 += 1;
                // CONCAT: B < C and C - B >= 1 (concatenating at least 2 values)
                if b < c && (c - b) >= 1 && (c - b) <= 20 {
                    entry.0 += 1;
                }
            }
        }
    }
    // Pick the candidate with the highest number of valid (B < C) hits.
    // CONCAT is unique: ALL instances should have B < C (range concat).
    // Collect into Vec + deterministic sort (valid desc, byte asc) so HashMap
    // iteration order cannot flip the winner when candidates tie on valid count.
    let mut viable: Vec<(u8, usize)> = candidates.iter()
        .filter(|(&op, &(valid, total))| {
            !ctx.is_mapped(op)
                && valid >= 2
                && total >= 2
                && (valid * 100 / total) >= 80
        })
        .map(|(&op, &(valid, _))| (op, valid))
        .collect();
    viable.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    if let Some(&(op, _)) = viable.first() {
        ctx.try_assign(op, LuauOpcode::Concat as u8);
    }
}

/// GETVARARGS: AB format, only in vararg protos
fn detect_getvarargs(chunk: &Chunk, ctx: &mut DetectCtx) {
    let mut candidates: HashMap<u8, usize> = HashMap::new();
    for proto in &chunk.protos {
        if !proto.is_vararg { continue; }
        for &insn in &proto.code {
            let op = insn_op(insn);
            if ctx.is_mapped(op) { continue; }
            let a = insn_a(insn);
            let _b = insn_b(insn); // count+1, 0=all
            let c = insn_c(insn);
            // GETVARARGS: A = target register, B = count + 1 (0=all), C = 0
            if a < proto.max_stack_size && c == 0 {
                *candidates.entry(op).or_insert(0) += 1;
            }
        }
    }
    // Only opcodes that appear EXCLUSIVELY in vararg protos
    let mut filtered: Vec<(u8, usize)> = Vec::new();
    for (&op, &count) in &candidates {
        if ctx.is_mapped(op) || count < 2 { continue; }
        let mut non_vararg_count = 0usize;
        for proto in &chunk.protos {
            if proto.is_vararg { continue; }
            for &insn in &proto.code {
                if insn_op(insn) == op {
                    non_vararg_count += 1;
                }
            }
        }
        // If it appears almost exclusively in vararg protos, it's likely GETVARARGS
        if non_vararg_count == 0 || (count > non_vararg_count * 5) {
            filtered.push((op, count));
        }
    }
    // Deterministic: byte ascending when counts tie (HashMap iteration noise).
    filtered.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    if let Some(&(op, _)) = filtered.first() {
        ctx.try_assign(op, LuauOpcode::GetVarargs as u8);
    }
    detect_getvarargs_single_multret(chunk, ctx);
}

/// Sites where an unmapped byte in a vararg proto is immediately followed by a
/// multret RETURN (B == 0) or a multret CALL (B == 0) based at the register the
/// candidate just filled — i.e. `return ...` and `f(...)`.
///
/// This is the positive half of GETVARARGS detection. The look-alikes cannot
/// reach it: `return nil` compiles to LOADNIL plus a FIXED-count RETURN (B == 2),
/// so it never presents a multret successor.
fn getvarargs_multret_sites(chunk: &Chunk, ctx: &DetectCtx) -> HashMap<u8, usize> {
    let mut sites: HashMap<u8, usize> = HashMap::new();
    let return_op = ctx.find_shuffled(LuauOpcode::Return as u8);
    let call_op = ctx.find_shuffled(LuauOpcode::Call as u8);
    if return_op.is_none() && call_op.is_none() {
        return sites;
    }
    for proto in &chunk.protos {
        if !proto.is_vararg { continue; }
        for i in 0..proto.code.len().saturating_sub(1) {
            let insn = proto.code[i];
            let op = insn_op(insn);
            if ctx.is_mapped(op) { continue; }
            let a = insn_a(insn);
            if a >= proto.max_stack_size || insn_c(insn) != 0 { continue; }
            let next = proto.code[i + 1];
            let next_op = insn_op(next);
            let multret_return = Some(next_op) == return_op && insn_b(next) == 0;
            let multret_call = Some(next_op) == call_op
                && insn_b(next) == 0
                && insn_a(next) == a;
            if multret_return || multret_call {
                *sites.entry(op).or_insert(0) += 1;
            }
        }
    }
    sites
}

/// GETVARARGS rescue for protos holding exactly ONE `...` expansion.
///
/// The `count >= 2` gate above deliberately protects the shared degenerate
/// (A, B, C=0) pool — LOADNIL, LOADB and CLOSEUPVALS have the same shape, and in
/// an all-vararg chunk the "appears only in vararg protos" filter separates
/// nothing. That gate also loses every file whose sole GETVARARGS is a bare
/// `return ...` or `f(...)` forward.
///
/// Those two idioms leave a decisive successor signature: the very next word is
/// a multret RETURN (B == 0) or a multret CALL (B == 0) whose call base is the
/// register GETVARARGS just filled. `return nil` compiles to LOADNIL plus a
/// FIXED-count RETURN (B == 2), so the look-alikes cannot match. RETURN and CALL
/// are mapped in earlier tiers, so the successor is readable from here.
fn detect_getvarargs_single_multret(chunk: &Chunk, ctx: &mut DetectCtx) {
    if ctx.find_shuffled(LuauOpcode::GetVarargs as u8).is_some() {
        return;
    }
    let sites = getvarargs_multret_sites(chunk, ctx);
    // Two different bytes both look like the sole GETVARARGS — no way to choose,
    // so claim neither.
    if sites.len() != 1 {
        return;
    }
    if let Some((&op, _)) = sites.iter().next() {
        ctx.try_assign(op, LuauOpcode::GetVarargs as u8);
    }
}

/// CLOSEUPVALS: single register operand A, B=0, C=0
fn detect_closeupvals(chunk: &Chunk, ctx: &mut DetectCtx) {
    let mut candidates: HashMap<u8, usize> = HashMap::new();
    for proto in &chunk.protos {
        if proto.num_upvalues == 0 {
            // CLOSEUPVALS typically appears in protos that CREATE closures with upvalues
            let has_children_with_upvals = proto.child_protos.iter().any(|&idx| {
                chunk.protos.get(idx as usize).map(|p| p.num_upvalues > 0).unwrap_or(false)
            });
            if !has_children_with_upvals { continue; }
        }
        for &insn in &proto.code {
            let op = insn_op(insn);
            if ctx.is_mapped(op) { continue; }
            let b = insn_b(insn);
            let c = insn_c(insn);
            // CLOSEUPVALS: B=0, C=0, only A matters
            if b == 0 && c == 0 {
                *candidates.entry(op).or_insert(0) += 1;
            }
        }
    }
    if let Some((&op, &count)) = candidates.iter()
        .filter(|(&op, _)| !ctx.is_mapped(op))
        .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
    {
        if count >= 2 { ctx.try_assign(op, LuauOpcode::CloseUpvals as u8); }
    }
}

/// AND/OR/ANDK/ORK: These have the same ABC format as arithmetic but semantically different.
/// AND/OR: A=result, B=condition, C=alternative (all registers)
/// ANDK/ORK: A=result, B=condition, C=constant index
/// Key insight: AND/OR often appear near conditional jumps (JumpIf/JumpIfNot)
fn detect_and_or(chunk: &Chunk, ctx: &mut DetectCtx) {
    // AND/OR: ABC format where A=result, B=left, C=right, all registers.
    // After arithmetic opcodes are mapped, the remaining ABC all-register opcodes
    // with valid operands are likely AND, OR, or POW.
    //
    // Additional signal: AND/OR are often followed by a JUMP (unconditional)
    // because the Luau compiler emits: AND A B C; JUMP +1; MOVE A C
    // or similar patterns. We use nearby JUMP as a bonus signal but don't require it.
    let jump_shuffled = ctx.find_shuffled(LuauOpcode::Jump as u8);
    let jumpif_shuffled = ctx.find_shuffled(LuauOpcode::JumpIf as u8);
    let jumpifnot_shuffled = ctx.find_shuffled(LuauOpcode::JumpIfNot as u8);

    let mut candidates: HashMap<u8, (usize, usize)> = HashMap::new(); // (jump_nearby_count, total_count)
    for proto in &chunk.protos {
        for i in 0..proto.code.len() {
            let insn = proto.code[i];
            let op = insn_op(insn);
            if ctx.is_mapped(op) { continue; }
            let a = insn_a(insn) as usize;
            let b = insn_b(insn) as usize;
            let c = insn_c(insn) as usize;
            // ABC format, all valid registers, B != C (AND/OR with same operand is unusual)
            if a < proto.max_stack_size as usize
                && b < proto.max_stack_size as usize
                && c < proto.max_stack_size as usize
            {
                let entry = candidates.entry(op).or_insert((0, 0));
                entry.1 += 1;
                // Bonus: check if a JUMP or conditional jump is within 2 instructions
                for offset in 1..=2 {
                    if i + offset >= proto.code.len() { break; }
                    let next_op = insn_op(proto.code[i + offset]);
                    if Some(next_op) == jump_shuffled
                        || Some(next_op) == jumpif_shuffled
                        || Some(next_op) == jumpifnot_shuffled
                    {
                        entry.0 += 1;
                        break;
                    }
                }
            }
        }
    }

    // AND/OR candidates: ABC all-register, with decent frequency, and preferably near jumps
    // Sort by jump-nearby count first (best signal), then total count
    let mut sorted: Vec<_> = candidates.iter()
        .filter(|(&op, &(_, total))| !ctx.is_mapped(op) && total >= 2)
        .map(|(&op, &(jump_count, total))| (op, jump_count, total))
        .collect();
    // Prefer candidates where a large fraction appear near jumps.
    // Final tiebreak: byte value ascending (deterministic under HashMap iteration).
    sorted.sort_by(|a, b| {
        let a_ratio = if a.2 > 0 { a.1 * 100 / a.2 } else { 0 };
        let b_ratio = if b.2 > 0 { b.1 * 100 / b.2 } else { 0 };
        b_ratio.cmp(&a_ratio)
            .then(b.1.cmp(&a.1))
            .then(b.2.cmp(&a.2))
            .then(a.0.cmp(&b.0))
    });

    for (i, &(op, _, count)) in sorted.iter().enumerate() {
        if count < 2 { break; }
        match i {
            0 => { ctx.try_assign(op, LuauOpcode::And as u8); }
            1 => { ctx.try_assign(op, LuauOpcode::Or as u8); }
            _ => break,
        }
    }

    // ANDK/ORK: A and B are registers, C is a constant index
    let mut k_candidates: HashMap<u8, usize> = HashMap::new();
    for proto in &chunk.protos {
        for i in 0..proto.code.len() {
            let insn = proto.code[i];
            let op = insn_op(insn);
            if ctx.is_mapped(op) { continue; }
            let a = insn_a(insn) as usize;
            let b = insn_b(insn) as usize;
            let c = insn_c(insn) as usize;
            if a < proto.max_stack_size as usize
                && b < proto.max_stack_size as usize
                && c < proto.constants.len()
            {
                // Check if any jump appears within 2 instructions (bonus signal)
                let mut has_jump = false;
                for offset in 1..=2 {
                    if i + offset >= proto.code.len() { break; }
                    let next_op = insn_op(proto.code[i + offset]);
                    if Some(next_op) == jump_shuffled
                        || Some(next_op) == jumpif_shuffled
                        || Some(next_op) == jumpifnot_shuffled
                    {
                        has_jump = true;
                        break;
                    }
                }
                if has_jump {
                    *k_candidates.entry(op).or_insert(0) += 1;
                }
            }
        }
    }
    let mut k_sorted: Vec<_> = k_candidates.iter()
        .filter(|(&op, _)| !ctx.is_mapped(op))
        .collect();
    k_sorted.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    for (i, (&op, &count)) in k_sorted.iter().enumerate() {
        if count < 2 { break; }
        match i {
            0 => { ctx.try_assign(op, LuauOpcode::AndK as u8); }
            1 => { ctx.try_assign(op, LuauOpcode::OrK as u8); }
            _ => break,
        }
    }
}

/// FASTCALL (base): A=builtin_id, B=0 (unused), C=jump offset to CALL. No AUX word.
/// The critical differentiator is B=0 — FASTCALL1 has B=arg register, FASTCALL2/2K/3 have AUX.
fn detect_fastcall(chunk: &Chunk, ctx: &mut DetectCtx) {
    let call_shuffled = match ctx.find_shuffled(LuauOpcode::Call as u8) {
        Some(op) => op,
        None => return,
    };

    // Track (b_zero_count, b_any_count) per candidate
    let mut candidates: HashMap<u8, (usize, usize)> = HashMap::new();
    for proto in &chunk.protos {
        for i in 0..proto.code.len() {
            let insn = proto.code[i];
            let op = insn_op(insn);
            if ctx.is_mapped(op) { continue; }
            let a = insn_a(insn) as usize; // builtin id
            let b = insn_b(insn);
            let c = insn_c(insn) as usize;
            // FASTCALL: A=builtin (<=127), C=jump offset>0, CALL at pc+C+1
            if a <= 127 && c > 0 {
                let call_pc = i + c + 1;
                if call_pc < proto.code.len() && insn_op(proto.code[call_pc]) == call_shuffled {
                    let entry = candidates.entry(op).or_insert((0, 0));
                    if b == 0 { entry.0 += 1; }
                    entry.1 += 1;
                }
            }
        }
    }
    // FASTCALL should have B=0 in ALL instances. Pick the candidate with the highest
    // ratio of b_zero/total, requiring b_zero == total (or very close).
    if let Some((&op, &(b_zero, _total))) = candidates.iter()
        .filter(|(_, &(b_zero, total))| total >= 2 && b_zero * 100 / total >= 90)
        .max_by(|a, b| a.1.0.cmp(&b.1.0).then_with(|| b.0.cmp(a.0)))
    {
        let _ = b_zero;
        ctx.try_assign(op, LuauOpcode::FastCall as u8);
    }
}

/// IDIVK: ABC format where B is register, C is constant index
fn detect_idivk(chunk: &Chunk, ctx: &mut DetectCtx) {
    // Only detect if IDiv is already mapped
    if ctx.find_shuffled(LuauOpcode::IDiv as u8).is_none() {
        return;
    }
    // Also need all standard ArithK to be mapped to avoid confusion
    let all_arithk_mapped = [
        LuauOpcode::AddK, LuauOpcode::SubK, LuauOpcode::MulK,
        LuauOpcode::DivK, LuauOpcode::ModK, LuauOpcode::PowK
    ].iter().all(|op| ctx.find_shuffled(*op as u8).is_some());
    if !all_arithk_mapped { return; }

    let mut candidates: HashMap<u8, usize> = HashMap::new();
    for proto in &chunk.protos {
        for &insn in &proto.code {
            let op = insn_op(insn);
            if ctx.is_mapped(op) { continue; }
            let a = insn_a(insn) as usize;
            let b = insn_b(insn) as usize;
            let c = insn_c(insn) as usize;
            if a < proto.max_stack_size as usize
                && b < proto.max_stack_size as usize
                && c < proto.constants.len()
            {
                if matches!(proto.constants.get(c), Some(Constant::Number(_))) {
                    *candidates.entry(op).or_insert(0) += 1;
                }
            }
        }
    }
    // IDivK is rare — cap at 2% of total instructions
    let max_idivk_freq = if ctx.total_insns > 100 { (ctx.total_insns / 50) as usize } else { usize::MAX };
    if let Some((&op, &count)) = candidates.iter()
        .filter(|(&op, &count)| !ctx.is_mapped(op) && count <= max_idivk_freq)
        .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
    {
        if count >= 3 { ctx.try_assign(op, LuauOpcode::IDivK as u8); }
    }
}

/// Post-detection validation: unassign any mappings where the frequency distribution
/// is wildly implausible. This catches false positives from speculative detectors.
fn validate_frequency_plausibility(_chunk: &Chunk, ctx: &mut DetectCtx) {
    if ctx.total_insns < 100 { return; } // Too few instructions to validate

    // Check each mapping for frequency plausibility
    let mut to_unassign: Vec<(u8, u8)> = Vec::new(); // (shuffled, standard)

    for shuffled in 0..=255u8 {
        let standard = ctx.map[shuffled as usize];
        if standard == 255 { continue; }
        if ctx.locked[shuffled as usize] { continue; } // cache-seeded — authoritative
        let freq = ctx.freq[shuffled as usize];
        let pct = freq * 1000 / ctx.total_insns; // per-mille (0.1%)

        let op = LuauOpcode::from_u8(standard);
        let implausible = match op {
            // NOP, BREAK, Coverage should be near-zero in compiled bytecode
            LuauOpcode::Nop | LuauOpcode::Break | LuauOpcode::Coverage | LuauOpcode::NativeCall => {
                freq > 5 // More than 5 instances is suspicious
            }
            // IDiv, IDivK, SubRK, DivRK are very rare in most bytecode
            LuauOpcode::IDiv | LuauOpcode::IDivK | LuauOpcode::SubRK | LuauOpcode::DivRK => {
                pct > 30 // More than 3% is implausible for these rare ops
            }
            // Unary ops should not be extremely frequent
            LuauOpcode::Not | LuauOpcode::Minus | LuauOpcode::Length => {
                pct > 80 // More than 8% is suspicious for unary ops
            }
            // LoadKX is only used when constants > 32768, extremely rare
            LuauOpcode::LoadKX => {
                freq > 10
            }
            // FastCall3 is very rare
            LuauOpcode::FastCall3 => {
                pct > 20 // More than 2%
            }
            // Concat, GetVarargs, CloseUpvals, JumpX are uncommon
            LuauOpcode::Concat | LuauOpcode::GetVarargs | LuauOpcode::CloseUpvals | LuauOpcode::JumpX => {
                pct > 50 // More than 5%
            }
            // SETTABLEN should not be one of the top opcodes
            LuauOpcode::SetTableN => {
                pct > 60 // More than 6% is suspicious
            }
            // CALL, RETURN, MOVE should be reasonably frequent
            LuauOpcode::Call | LuauOpcode::Return | LuauOpcode::Move => {
                // These should each be at least 1% of instructions
                // But don't unassign if based on strong detectors (tier 1-3)
                false // Don't unassign common ops — they were detected by strong detectors
            }
            _ => false,
        };

        if implausible {
            to_unassign.push((shuffled, standard));
        }
    }

    // Unassign implausible mappings
    for (shuffled, standard) in to_unassign {
        ctx.map[shuffled as usize] = 255;
        ctx.assigned[standard as usize] = false;
    }
}

/// Validate AUX alignment: for each mapped AUX-using opcode, check that its AUX words
/// don't look like valid instructions. If they do, the mapping is probably wrong.
fn validate_aux_alignment(chunk: &Chunk, ctx: &mut DetectCtx) {
    let mut to_unassign: Vec<(u8, u8)> = Vec::new();

    for shuffled in 0..=255u8 {
        let standard = ctx.map[shuffled as usize];
        if standard == 255 { continue; }
        if ctx.locked[shuffled as usize] { continue; } // cache-seeded — authoritative
        let op = LuauOpcode::from_u8(standard);
        if !op.has_aux() { continue; }

        // For this AUX-using opcode, check if the word after each instance looks like
        // a valid (already-mapped) instruction. If so, this mapping is probably wrong
        // because the AUX word should NOT be a valid instruction start.
        let mut aux_is_mapped = 0u32;
        let mut total = 0u32;

        for proto in &chunk.protos {
            for i in 0..proto.code.len().saturating_sub(1) {
                if insn_op(proto.code[i]) == shuffled {
                    total += 1;
                    let aux_word = proto.code[i + 1];
                    // A word below 256 has EVERY operand field zero (`OP 0,0,0`,
                    // or `OP A=0 D=0`). That is what a small ordinal AUX looks
                    // like — SETLIST's 1-based start index, a comparison jump's
                    // right-hand register, NEWTABLE's array-size hint — and it
                    // is not a shape the compiler emits as a standalone
                    // instruction mid-stream. Counting those as "looks like an
                    // instruction" made this check fire on correct mappings
                    // whose AUX is a small integer by construction, while the
                    // misalignments it exists to catch (AUX words carrying
                    // packed constant indices or the bit-31 NOT flag) are all
                    // well above 256 and are still counted.
                    if aux_word < 256 { continue; }
                    let aux_byte = insn_op(aux_word);
                    if ctx.is_mapped(aux_byte) && ctx.map[aux_byte as usize] != standard {
                        // The AUX word looks like a valid instruction → suspicious
                        aux_is_mapped += 1;
                    }
                }
            }
        }

        // If AUX words look like valid instructions at a rate significantly above the
        // expected base rate, this mapping is probably wrong.
        // Base rate: ~mapped_count/256 of random bytes will match a mapped opcode.
        // But AUX data (string/constant indices) clusters in the low range (0-83),
        // so the effective base rate is much higher — often 60-80%.
        // Only reject when the rate is very high AND well above the base rate.
        let mapped_count = ctx.map.iter().filter(|&&v| v != 255).count() as u32;
        let base_rate_pct = mapped_count * 100 / 256; // expected % by chance
        // Reject only if observed rate exceeds 85% AND is at least 20 points above base rate
        let threshold = std::cmp::max(85, base_rate_pct + 20);
        if total >= 10 && aux_is_mapped * 100 / total > threshold {
            to_unassign.push((shuffled, standard));
        }
    }

    for (shuffled, standard) in to_unassign {
        ctx.map[shuffled as usize] = 255;
        ctx.assigned[standard as usize] = false;
    }
}

/// Process-of-elimination pass: try to map remaining opcodes by format constraints
/// This runs AFTER all pattern-based detectors and picks up stragglers.
fn detect_elimination_pass(chunk: &Chunk, ctx: &mut DetectCtx) {
    // Check if there are any unmapped standard opcodes left
    let has_remaining = (0..84u8).any(|std_op| !ctx.assigned[std_op as usize]);
    if !has_remaining { return; }

    // Frequency-based: count how often each unmapped shuffled opcode appears
    let mut freq: HashMap<u8, usize> = HashMap::new();
    for proto in &chunk.protos {
        for &insn in &proto.code {
            let op = insn_op(insn);
            if !ctx.is_mapped(op) {
                *freq.entry(op).or_insert(0) += 1;
            }
        }
    }

    // NOP and BREAK are very rare in compiled bytecode — map the rarest unmapped opcodes
    let remaining_nop = !ctx.assigned[LuauOpcode::Nop as usize];
    let remaining_break = !ctx.assigned[LuauOpcode::Break as usize];
    let remaining_coverage = !ctx.assigned[LuauOpcode::Coverage as usize];
    let remaining_nativecall = !ctx.assigned[LuauOpcode::NativeCall as usize];

    // These opcodes typically have 0 occurrences in most bytecode
    // Map them to unmapped shuffled bytes that have 0 or very low frequency
    let mut zero_freq: Vec<u8> = (0..=255u8)
        .filter(|&op| !ctx.is_mapped(op) && freq.get(&op).copied().unwrap_or(0) == 0)
        .collect();

    // Assign rare standard opcodes to zero-frequency shuffled bytes
    // These are pseudo-opcodes that don't appear in normal compiled bytecode
    let rare_opcodes = [
        (remaining_nop, LuauOpcode::Nop as u8),
        (remaining_break, LuauOpcode::Break as u8),
        (remaining_coverage, LuauOpcode::Coverage as u8),
        (remaining_nativecall, LuauOpcode::NativeCall as u8),
    ];
    for (needs_mapping, std_op) in rare_opcodes {
        if needs_mapping {
            if let Some(shuffled) = zero_freq.pop() {
                ctx.try_assign(shuffled, std_op);
            }
        }
    }
}

/// FASTCALL2K: A=builtin_id, B=arg1 register, C=jump offset; AUX = constant index
/// Distinguished from FASTCALL2 by AUX being a constant index (not a register).
/// Distinguished from FASTCALL1 by having an AUX word at all.
/// FASTCALL2K: ABC + AUX. A=builtin_id, B=arg1 register, C=jump offset, AUX=constant index.
/// Distinguished from FASTCALL2 by AUX being a CONSTANT index (not a register).
///
/// Threshold lowered from `count >= 2` to `count >= 1`: requiring the CALL to
/// land exactly at `pc + c + 1` AND a valid value-type constant at AUX is an
/// extremely strong joint signal — a single match is effectively impossible to
/// fake from random bytecode.
fn detect_fastcall2k(chunk: &Chunk, ctx: &mut DetectCtx) {
    let call_shuffled = match ctx.find_shuffled(LuauOpcode::Call as u8) {
        Some(op) => op,
        None => return,
    };
    let mut candidates: HashMap<u8, usize> = HashMap::new();
    for proto in &chunk.protos {
        let ms = proto.max_stack_size as usize;
        if proto.constants.is_empty() { continue; }
        for i in 0..proto.code.len().saturating_sub(1) {
            let insn = proto.code[i];
            let op = insn_op(insn);
            if ctx.is_mapped(op) { continue; }
            let a = insn_a(insn) as usize;
            let b = insn_b(insn) as usize;
            let c = insn_c(insn) as usize;
            let aux = proto.code[i + 1];
            // FASTCALL2K: A=builtin (<=127), B=arg1 reg (<maxstack), C=jump>0
            // AUX = constant index (must be valid, and typically points to a number/string/bool)
            if a <= 127 && b < ms && c > 0
                && (aux as usize) < proto.constants.len()
                // Constant should be a value type (not nil), since it is a fastcall argument
                && matches!(proto.constants.get(aux as usize),
                    Some(Constant::Number(_)) | Some(Constant::String(_)) | Some(Constant::Boolean(_)))
            {
                let call_pc = i + c + 1;
                if call_pc < proto.code.len() && insn_op(proto.code[call_pc]) == call_shuffled {
                    *candidates.entry(op).or_insert(0) += 1;
                }
            }
        }
    }
    if let Some((&op, &count)) = candidates.iter()
        .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
    {
        if count >= 1 { ctx.try_assign(op, LuauOpcode::FastCall2K as u8); }
    }
}

/// FASTCALL3: A=builtin_id, B=arg1 register, C=jump offset, AUX=arg2+arg3 packed
/// Distinguished from FASTCALL2 by AUX having TWO register indices packed into it.
/// The upper 16 bits of AUX should be 0 (only low two bytes used for arg2/arg3).
fn detect_fastcall3(chunk: &Chunk, ctx: &mut DetectCtx) {
    let call_shuffled = match ctx.find_shuffled(LuauOpcode::Call as u8) {
        Some(op) => op,
        None => return,
    };
    let mut candidates: HashMap<u8, usize> = HashMap::new();
    for proto in &chunk.protos {
        let ms = proto.max_stack_size as usize;
        for i in 0..proto.code.len().saturating_sub(1) {
            let insn = proto.code[i];
            let op = insn_op(insn);
            if ctx.is_mapped(op) { continue; }
            let a = insn_a(insn) as usize;
            let b = insn_b(insn) as usize;
            let c = insn_c(insn) as usize;
            let aux = proto.code[i + 1];
            let aux_lo = (aux & 0xFF) as usize;       // arg2 register
            let aux_hi = ((aux >> 8) & 0xFF) as usize; // arg3 register
            // FASTCALL3: A=builtin (<=127), B=arg1 reg, C=jump>0
            // AUX: low byte = arg2 reg, next byte = arg3 reg, upper 16 bits should be 0
            if a <= 127 && b < ms && c > 0
                && aux_lo < ms
                && aux_hi < ms
                && (aux >> 16) == 0  // upper 16 bits unused
            {
                let call_pc = i + c + 1;
                if call_pc < proto.code.len() && insn_op(proto.code[call_pc]) == call_shuffled {
                    *candidates.entry(op).or_insert(0) += 1;
                }
            }
        }
    }
    if let Some((&op, &count)) = candidates.iter()
        .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
    {
        if count >= 2 { ctx.try_assign(op, LuauOpcode::FastCall3 as u8); }
    }
}

/// Roblox native bitwise operator detection.
///
/// Roblox Luau added native bitwise ops beyond canonical 83 (FastCall3):
///   Band (84): A = B & C  — ABC format, both B and C are registers
///   Bor  (85): A = B | C  — ABC format
///   Bxor (86): A = B ~ C  — ABC format (XOR)
///   Bnot (87): A = ~B     — ABC format, C always 0 (unary)
///   Shl  (88): A = B << C — ABC format
///   Shr  (89): A = B >> C — ABC format
///   Bandk(90): A = B & K(C) — ABC format, C is constant index
///   Bork (91): A = B | K(C) — ABC format, C is constant index
///
/// Detection strategy: scan all instruction positions (AUX-aware) and for each
/// unmapped byte compute: c_zero ratio (→ unary group), c_const ratio (→ K group),
/// c_reg_nonzero ratio (→ binary group). Assign canonical ops within each group
/// sorted by frequency descending.
///
/// Requires all 6 standard arithmetic ops to be mapped first (same ABC format).
fn detect_bitwise_ops(chunk: &Chunk, ctx: &mut DetectCtx) {
    // DISABLED — this pass invents opcodes that are not in the bytecode.
    //
    // Luau source has no bitwise operators. `bit32.band`/`bor`/`bxor` are plain
    // library functions and compile to ordinary calls, so the stock Roblox
    // compiler does not emit opcodes 84-91 at all. Detection here rests only on
    // operand SHAPE (C==0, C within the constant range, ...) — properties every
    // ABC-format instruction also satisfies — so it reliably mislabels real
    // opcodes as bitwise ones.
    //
    // Measured on a 1286-script corpus from a live client: once all six
    // arithmetic ops were pinned from measured ground truth, the guard below
    // started passing and this pass then assigned BAND/BOR/BNOT/SHL/SHR/BANDK
    // across 262 files. It emitted `bit32.band(x, nil)`, `bit32.band(0.5, ...)`
    // and `bit32.band(self.Position, self.Position)` — bitwise operations on
    // nil, on floats and on a Vector3, none of which can occur. One case
    // reduced to `atan2(band(X,Z) - band(Z,X), band(X,X) + band(Z,Z))` where the
    // source plainly computed `X*Z - Z*X`: the byte was MUL. Every emission was
    // wrong. Disabling it took files containing bit32 artifacts from 243 to 1,
    // and that last one is genuine library use in a hash function.
    //
    // Note the failure mode is the one this crate is built to avoid: the output
    // looked plausible and was fiction. An unresolved marker is honest; a
    // fabricated `bit32.band` silently corrupts otherwise-correct output.
    //
    // If a future client genuinely emits these, re-enable behind measured
    // evidence from `probe align`, never shape heuristics.
    let _ = (chunk, ctx);
    #[allow(unreachable_code)]
    return;

    // Require all 6 standard arithmetic ops mapped first — they share the same ABC format.
    // Without this guard, detect_bitwise_ops can steal arith bytes that look like bitwise.
    let all_arith_mapped = [
        LuauOpcode::Add, LuauOpcode::Sub, LuauOpcode::Mul,
        LuauOpcode::Div, LuauOpcode::Mod, LuauOpcode::Pow,
    ].iter().all(|op| ctx.find_shuffled(*op as u8).is_some());
    if !all_arith_mapped { return; }

    // Per-byte statistics gathered at true instruction positions (AUX-aware scan).
    // For each unmapped shuffled byte we count:
    //   pos_hits:        times seen at a real instruction position (A<stack, B<stack)
    //   c_zero:          pos_hits where C == 0  (→ unary group: Bnot, Shl, Shr)
    //   c_reg_nonzero:   pos_hits where C > 0 and C < stack  (→ binary reg-reg group)
    //   c_const_number:  pos_hits where C < const_len and constants[C] = Number (→ K group)
    //   c_oob:           pos_hits where C > 0 and C >= stack (→ strong K-variant signal)
    struct ByteStats {
        pos_hits: usize,
        c_zero: usize,
        c_reg_nonzero: usize,
        c_const_number: usize,
        c_oob: usize,
    }
    let mut stats: HashMap<u8, ByteStats> = HashMap::new();

    for proto in &chunk.protos {
        let code = &proto.code;
        let stack = proto.max_stack_size as usize;
        let const_len = proto.constants.len();
        let mut i = 0;
        while i < code.len() {
            let insn = code[i];
            let op = insn_op(insn);
            let mapped = ctx.map[op as usize];
            if mapped != 255 {
                // Known opcode — skip AUX word if it has one
                if LuauOpcode::from_u8(mapped).has_aux() && i + 1 < code.len() {
                    i += 2;
                } else {
                    i += 1;
                }
                continue;
            }
            // Unmapped byte at a true instruction position
            let a = insn_a(insn) as usize;
            let b = insn_b(insn) as usize;
            let c = insn_c(insn) as usize;
            if a < stack && b < stack {
                let st = stats.entry(op).or_insert(ByteStats {
                    pos_hits: 0, c_zero: 0, c_reg_nonzero: 0, c_const_number: 0, c_oob: 0,
                });
                st.pos_hits += 1;
                if c == 0 {
                    st.c_zero += 1;
                } else if c < stack {
                    st.c_reg_nonzero += 1;
                } else {
                    st.c_oob += 1;
                }
                if c < const_len {
                    if matches!(proto.constants.get(c), Some(Constant::Number(_))) {
                        st.c_const_number += 1;
                    }
                }
            }
            i += 1;
        }
    }

    // Categorize each unmapped byte into one of three groups.
    // Thresholds are deliberately generous — we'd rather assign a byte to the
    // wrong specific operator than leave it unassigned (silent code drop).
    let mut unary_group: Vec<(u8, usize)> = Vec::new();  // → Bnot, then Shl, Shr
    let mut k_group:     Vec<(u8, usize)> = Vec::new();  // → Bandk, Bork
    let mut binary_group: Vec<(u8, usize)> = Vec::new(); // → Band, Bor, Bxor

    for (&op, st) in &stats {
        if ctx.is_mapped(op) { continue; }
        if st.pos_hits < 2 { continue; }

        let c_zero_pct   = st.c_zero * 100 / st.pos_hits;
        let c_oob_pct    = st.c_oob  * 100 / st.pos_hits;
        let c_const_pct  = st.c_const_number * 100 / st.pos_hits;

        if c_zero_pct >= 85 && st.c_reg_nonzero == 0 {
            // C is always 0 and never a valid non-zero register → unary-like
            unary_group.push((op, st.pos_hits));
        } else if c_oob_pct >= 25 || c_const_pct >= 60 {
            // C is often out of register range or maps to a number constant → K-variant
            k_group.push((op, st.pos_hits));
        } else {
            // C varies and is a valid register → binary reg-reg
            binary_group.push((op, st.pos_hits));
        }
    }

    // Sort each group by frequency descending, then byte ascending for determinism.
    let sort_group = |v: &mut Vec<(u8, usize)>| {
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    };
    sort_group(&mut unary_group);
    sort_group(&mut k_group);
    sort_group(&mut binary_group);

    // Assign each group: advance the TARGET index independently from the SOURCE index.
    // This ensures that if target[0] is already assigned, the highest-freq source byte
    // still gets assigned to target[1] rather than being skipped entirely.
    let assign_group = |group: &[(u8, usize)], targets: &[LuauOpcode], ctx: &mut DetectCtx| {
        let mut ti = 0;
        for &(op, _) in group {
            // Skip targets already assigned (from prior detection or cache).
            while ti < targets.len() && ctx.assigned[targets[ti] as u8 as usize] {
                ti += 1;
            }
            if ti >= targets.len() { break; }
            ctx.try_assign_force(op, targets[ti] as u8);
            ti += 1;
        }
    };

    let unary_targets = [LuauOpcode::Bnot, LuauOpcode::Shl, LuauOpcode::Shr];
    assign_group(&unary_group, &unary_targets, ctx);

    let k_targets = [LuauOpcode::Bandk, LuauOpcode::Bork];
    assign_group(&k_group, &k_targets, ctx);

    let bin_targets = [LuauOpcode::Band, LuauOpcode::Bor, LuauOpcode::Bxor];
    assign_group(&binary_group, &bin_targets, ctx);
}

/// Detect Roblox-specific opcodes beyond canonical 91 (RbxExt92-95).
///
/// This runs as a POST-AUGMENTER pass so that the known-shuffle augmenter can operate
/// on a clean canonical-only (0-91) fingerprint. Assigning RbxExt slots before the
/// augmenter would change the fingerprint and cause the wrong variant to be selected,
/// breaking canonical opcode detection.
///
/// Unary shape (C always 0, A=dst, B=src): → RbxExt92, RbxExt93, RbxExt94
/// Binary shape (C varies, is a register): → RbxExt95
///
/// Safety gates mirror detect_bitwise_ops:
/// - All 6 standard arithmetic ops must already be mapped (shared ABC format).
/// - All canonical unary ops (Not, Minus, Length, Bnot, Shl, Shr) must be mapped
///   so we don't steal bytes that still belong to them.
fn detect_rbx_ext_ops(chunk: &Chunk, ctx: &mut DetectCtx) {
    // Require all standard arithmetic AND the known canonical unary/bitwise ops mapped
    // so we don't steal bytes they still need.
    //
    // Phase B0.78: Tolerate LENGTH being the sole missing prerequisite. LENGTH is
    // the most common cascade blocker (its detection is harder than NOT/MINUS), and
    // blocking all 14 RBX_EXT opcodes for one missing unary is disproportionate.
    // When LENGTH is the only gap, proceed but be careful not to assign LENGTH's
    // byte to any RBX_EXT target (it's unary C=0, same shape as RbxExt92-98).
    let prerequisites = [
        LuauOpcode::Add, LuauOpcode::Sub, LuauOpcode::Mul,
        LuauOpcode::Div, LuauOpcode::Mod, LuauOpcode::Pow,
        LuauOpcode::Not, LuauOpcode::Minus, LuauOpcode::Length,
        LuauOpcode::Band, LuauOpcode::Bor, LuauOpcode::Bxor,
        LuauOpcode::Bnot, LuauOpcode::Shl, LuauOpcode::Shr,
    ];
    let missing: Vec<LuauOpcode> = prerequisites.iter()
        .filter(|op| ctx.find_shuffled(**op as u8).is_none())
        .copied()
        .collect();
    if missing.len() > 1 {
        return;
    }
    if missing.len() == 1 && missing[0] != LuauOpcode::Length {
        return;
    }
    let length_missing = !missing.is_empty();

    // Reuse the same per-byte statistics as detect_bitwise_ops.
    struct ByteStats { pos_hits: usize, c_zero: usize, c_reg_nonzero: usize }
    let mut stats: HashMap<u8, ByteStats> = HashMap::new();

    for proto in &chunk.protos {
        let code = &proto.code;
        let stack = proto.max_stack_size as usize;
        let mut i = 0;
        while i < code.len() {
            let insn = code[i];
            let op = insn_op(insn);
            let mapped = ctx.map[op as usize];
            if mapped != 255 {
                if LuauOpcode::from_u8(mapped).has_aux() && i + 1 < code.len() { i += 2; }
                else { i += 1; }
                continue;
            }
            let a = insn_a(insn) as usize;
            let b = insn_b(insn) as usize;
            let c = insn_c(insn) as usize;
            if a < stack && b < stack {
                let st = stats.entry(op).or_insert(ByteStats { pos_hits: 0, c_zero: 0, c_reg_nonzero: 0 });
                st.pos_hits += 1;
                if c == 0 { st.c_zero += 1; }
                else if c < stack { st.c_reg_nonzero += 1; }
            }
            i += 1;
        }
    }

    let mut unary_group: Vec<(u8, usize)> = Vec::new();
    let mut binary_group: Vec<(u8, usize)> = Vec::new();

    for (&op, st) in &stats {
        if ctx.is_mapped(op) { continue; }
        // Allow single-occurrence bytes — the structural signature (C=0 for unary,
        // C=register for binary) is distinctive enough with just 1 occurrence,
        // especially since we require all 15 prerequisite ops to be mapped first.
        if st.pos_hits == 0 { continue; }
        let c_zero_pct = st.c_zero * 100 / st.pos_hits;
        if c_zero_pct >= 85 && st.c_reg_nonzero == 0 {
            unary_group.push((op, st.pos_hits));
        } else if st.c_reg_nonzero * 2 >= st.pos_hits {
            binary_group.push((op, st.pos_hits));
        }
    }

    let sort_group = |v: &mut Vec<(u8, usize)>| {
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    };
    sort_group(&mut unary_group);
    sort_group(&mut binary_group);

    let assign_group = |group: &[(u8, usize)], targets: &[LuauOpcode], ctx: &mut DetectCtx| {
        let mut ti = 0;
        for &(op, _) in group {
            while ti < targets.len() && ctx.assigned[targets[ti] as u8 as usize] { ti += 1; }
            if ti >= targets.len() { break; }
            ctx.try_assign_force(op, targets[ti] as u8);
            ti += 1;
        }
    };

    // Phase B0.78: When LENGTH is the sole missing prerequisite, the highest-
    // frequency unary candidate is most likely LENGTH itself (its C=0 format
    // matches the RBX_EXT unary signature). Skip it to avoid misassignment.
    let unary_to_assign = if length_missing && !unary_group.is_empty() {
        eprintln!("  RBX_EXT: LENGTH missing, skipping top unary candidate 0x{:02X} ({} hits)",
            unary_group[0].0, unary_group[0].1);
        &unary_group[1..]
    } else {
        &unary_group[..]
    };

    let unary_targets = [LuauOpcode::RbxExt92, LuauOpcode::RbxExt93, LuauOpcode::RbxExt94,
                         LuauOpcode::RbxExt96, LuauOpcode::RbxExt97, LuauOpcode::RbxExt98];
    assign_group(unary_to_assign, &unary_targets, ctx);

    let bin_targets = [LuauOpcode::RbxExt95, LuauOpcode::RbxExt99, LuauOpcode::RbxExt100,
                       LuauOpcode::RbxExt101, LuauOpcode::RbxExt102, LuauOpcode::RbxExt103,
                       LuauOpcode::RbxExt104, LuauOpcode::RbxExt105];
    assign_group(&binary_group, &bin_targets, ctx);
}

/// LOADKX: AD format, A = target register, AUX = constant index (for large constants)
///
/// The defining structural invariant of LOADKX is D=0 (bits 16-31 of the instruction
/// are ALWAYS zero — a reserved field). No other Luau opcode has this property:
/// - AD-format opcodes (LOADK, LOADN, JUMP, etc.) use D for the operand/offset (non-zero)
/// - ABC-format opcodes use bits 16-31 for B and C fields (non-zero in practice)
/// - JumpX uses bits 16-31 as the high 16 bits of its 24-bit E offset (non-zero for
///   any meaningful jump)
///
/// Phase B0.15 fixes:
///
/// 1. Removed outer guard `proto.constants.len() <= 32768`: OPCODE_TRACE confirmed
///    0xC1 (LOADKX) appears with AUX=6081 in a large proto. The Roblox compiler
///    emits LOADKX for ALL constant loads in protos where any constant index exceeds
///    the range. Protos with LOADKX may have ANY number of constants as long as ≥1
///    is accessed via LOADKX.
///
/// 2. Removed inner guard `aux >= 32768`: same evidence — AUX=6081 is a valid
///    LOADKX constant index for a proto that has many constants (6081 < constants.len()).
///    The `(aux as usize) < proto.constants.len()` check already validates the index.
///
/// 3. D=0 invariant is now the SOLE discriminator: any unmapped instruction byte with
///    D=0, valid register A, and a valid constant-table-index AUX is a LOADKX hit.
///    This is highly specific — normal instructions rarely have D=0 coincidentally.
///
/// 4. `count >= 1` (was >= 2): a single structural D=0 hit is sufficient.
///
/// 5. `try_assign_force`: bypasses the 2%-frequency cap in `try_assign`. LOADKX's
///    shuffled byte can appear in the low byte of AUX words from GETIMPORT etc.,
///    inflating its raw frequency above the 2% guard.
fn detect_loadkx(chunk: &Chunk, ctx: &mut DetectCtx) {
    // Phase B0.16: LOADKX detection via D=0 purity.
    //
    // LOADKX is defined as: D field = 0 (reserved, ALWAYS zero by spec), A = destination
    // register, AUX = 32-bit constant index. The D=0 invariant is the sole structural
    // discriminator. Roblox emits LOADKX even for small constant tables (confirmed by
    // 0xC1(9) unresolved in Animate.lua with no proto having > 32768 constants), so
    // the old > 32768 const-size guard is removed.
    //
    // PURITY CHECK: every occurrence of the candidate shuffled byte must have D=0.
    // Any byte that has D≠0 in even ONE occurrence is not LOADKX (other opcodes use
    // the D field, but LOADKX never does). This prevents false positives.
    if ctx.find_shuffled(LuauOpcode::LoadK as u8).is_none() { return; } // Need LOADK first

    // Phase 1: count total appearances and D=0-with-valid-AUX appearances per unmapped byte.
    //
    // AUX-AWARE SCAN: We must skip AUX words of already-mapped opcodes. If we process
    // an AUX word as a potential instruction, it creates false purity hits. For example,
    // GetGlobal's AUX is a string index (small integer → D=0, low byte may be unmapped).
    // Without skipping, that AUX word looks like a D=0 occurrence of the unmapped byte,
    // poisoning the purity check and breaking LOADKX detection.
    //
    // `skip_next`: set when a mapped opcode has an AUX word. The very next iteration
    // skips the word entirely — it is data, not an instruction.
    let mut total_appearances: HashMap<u8, u32> = HashMap::new();
    let mut d0_valid: HashMap<u8, u32> = HashMap::new();

    for proto in &chunk.protos {
        let const_len = proto.constants.len();
        if const_len == 0 { continue; } // nothing to reference

        let code_len = proto.code.len();
        let mut skip_next = false;
        for i in 0..code_len {
            let insn = proto.code[i];
            let op = insn_op(insn);

            if skip_next {
                // This word is the AUX of the previous mapped opcode — treat it as data.
                skip_next = false;
                continue;
            }

            if ctx.is_mapped(op) {
                // Mark the next word as AUX data if this opcode needs one.
                let canonical = ctx.map[op as usize];
                let luau_op = LuauOpcode::from_u8(canonical);
                if luau_op.has_aux() {
                    skip_next = true;
                }
                continue;
            }

            // Unmapped byte: candidate for LOADKX.
            // We need i+1 as AUX — only count if the next word exists.
            *total_appearances.entry(op).or_insert(0) += 1;

            if i + 1 < code_len {
                let a = insn_a(insn) as usize;
                let d = insn_d(insn);
                let aux_u = proto.code[i + 1] as usize;

                // LOADKX: D=0, A is a valid destination register, AUX is a valid const index.
                if d == 0 && a < proto.max_stack_size as usize && aux_u < const_len {
                    *d0_valid.entry(op).or_insert(0) += 1;
                    // Skip the next word as AUX data. This prevents the AUX word
                    // (a 32-bit constant index whose low byte may equal our candidate byte)
                    // from appearing as a spurious "instruction" and poisoning purity counts.
                    skip_next = true;
                }
                // If D≠0 or A/AUX invalid, only total increments → purity fails for this byte.
            }
            // If i+1 is out of range, we can't verify AUX → d0_valid not incremented → purity fails.
        }
    }

    // Phase 2: purity filter — only bytes where EVERY appearance satisfies D=0+valid-AUX.
    // Any byte with even one D≠0 occurrence is definitively not LOADKX.
    let mut candidates: Vec<(u8, u32)> = d0_valid.iter()
        .filter(|(&op, &d0_count)| {
            let total = *total_appearances.get(&op).unwrap_or(&0);
            // 100% purity: every occurrence has D=0 + valid AUX
            total > 0 && d0_count == total
        })
        .map(|(&op, &d0_count)| (op, d0_count))
        .collect();

    // Sort: most valid occurrences first, then by op byte for determinism.
    candidates.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    if let Some(&(op, count)) = candidates.first() {
        if count >= 1 {
            // Use try_assign_force to bypass the generic 2%-frequency guard.
            // LOADKX can appear frequently (one per large-constant reference).
            ctx.try_assign_force(op, LuauOpcode::LoadKX as u8);
        }
    }
}

/// IDIV/IDIVK: integer division (bytecode v5+). Same format as Div/DivK.
fn detect_idiv(chunk: &Chunk, ctx: &mut DetectCtx) {
    // These have the same format as regular arithmetic, so they can't be easily
    // distinguished from unmapped arithmetic ops. Only detect if all 6 standard
    // arithmetic ops are already mapped.
    let all_arith_mapped = [
        LuauOpcode::Add, LuauOpcode::Sub, LuauOpcode::Mul,
        LuauOpcode::Div, LuauOpcode::Mod, LuauOpcode::Pow
    ].iter().all(|op| ctx.find_shuffled(*op as u8).is_some());

    if !all_arith_mapped { return; }

    let mut candidates: HashMap<u8, usize> = HashMap::new();
    for proto in &chunk.protos {
        for &insn in &proto.code {
            let op = insn_op(insn);
            if ctx.is_mapped(op) { continue; }
            let a = insn_a(insn) as usize;
            let b = insn_b(insn) as usize;
            let c = insn_c(insn) as usize;
            if a < proto.max_stack_size as usize
                && b < proto.max_stack_size as usize
                && c < proto.max_stack_size as usize
            {
                *candidates.entry(op).or_insert(0) += 1;
            }
        }
    }
    // IDiv is rare — cap at 2% of total instructions
    let max_idiv_freq = if ctx.total_insns > 100 { (ctx.total_insns / 50) as usize } else { usize::MAX };
    if let Some((&op, &count)) = candidates.iter()
        .filter(|(&op, &count)| !ctx.is_mapped(op) && count <= max_idiv_freq)
        .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
    {
        if count >= 3 { ctx.try_assign(op, LuauOpcode::IDiv as u8); }
    }
}

// ═══════════════════════════════════════════════════════════════
// TIER 7: Advanced heuristic detectors
// ═══════════════════════════════════════════════════════════════

/// Detect opcodes that appear to use AUX words by analyzing instruction spacing patterns.
/// An AUX-using opcode will cause the NEXT word in the instruction stream to NOT be
/// a valid instruction start. We detect this by checking if pc+1 is always an already-mapped
/// opcode (suggesting the current instruction does NOT have AUX) or never mapped (suggesting
/// the next word IS an AUX).
fn detect_aux_behavior(chunk: &Chunk, ctx: &DetectCtx) -> HashMap<u8, bool> {
    // For each unmapped opcode, estimate if it has an AUX word.
    // Uses multiple signals:
    // 1. pc+1 mapped → no-AUX vote
    // 2. pc+1 unmapped AND pc+2 mapped → AUX vote
    // 3. pc+1 is NEVER mapped across all instances → strong AUX signal
    let mut has_aux_votes: HashMap<u8, [u32; 2]> = HashMap::new(); // [aux_yes, aux_no]
    let mut next_always_unmapped: HashMap<u8, bool> = HashMap::new();
    let mut instance_count: HashMap<u8, u32> = HashMap::new();

    for proto in &chunk.protos {
        for i in 0..proto.code.len().saturating_sub(1) {
            let op = insn_op(proto.code[i]);
            if ctx.is_mapped(op) { continue; }
            if ctx.freq[op as usize] < 3 { continue; }

            *instance_count.entry(op).or_insert(0) += 1;
            let next_op = insn_op(proto.code[i + 1]);
            let entry = has_aux_votes.entry(op).or_insert([0, 0]);

            if ctx.is_mapped(next_op) {
                // Next word is a known opcode → current instruction does NOT have AUX
                entry[1] += 1;
                next_always_unmapped.entry(op).and_modify(|v| *v = false).or_insert(false);
            } else {
                // Track that pc+1 was unmapped for this instance
                next_always_unmapped.entry(op).or_insert(true);
                // If the word after that (pc+2) IS a known opcode, it's more likely AUX
                if i + 2 < proto.code.len() {
                    let word_after = insn_op(proto.code[i + 2]);
                    if ctx.is_mapped(word_after) {
                        entry[0] += 1; // Strong evidence for AUX
                    }
                }
            }
        }
    }

    let mut result = HashMap::new();
    for (&op, &[aux_yes, aux_no]) in &has_aux_votes {
        let total = aux_yes + aux_no;
        let instances = *instance_count.get(&op).unwrap_or(&0);
        let always_unmapped = *next_always_unmapped.get(&op).unwrap_or(&false);

        if total >= 3 {
            // Primary test: if >65% of counted votes are AUX, classify as AUX
            // (slightly under 70% because AUX data bytes can occasionally match mapped opcode bytes)
            if aux_yes * 100 / total >= 65 {
                result.insert(op, true);
            } else if aux_no * 100 / total >= 65 {
                result.insert(op, false);
            }
            // else: ambiguous, don't insert (will be handled by Phase 5)
        }

        // Strong signal: if pc+1 is NEVER a mapped opcode across ALL instances (>= 5),
        // it's very likely AUX data, even if pc+2 evidence is weak
        if always_unmapped && instances >= 5 && !result.contains_key(&op) {
            result.insert(op, true);
        }
    }
    result
}

/// Validate whether a shuffled byte's AUX words match the expected format for a standard opcode.
/// Returns a confidence score from 0.0 to 1.0.
fn validate_aux_for_opcode(chunk: &Chunk, shuffled: u8, standard: u8) -> f64 {
    let op = LuauOpcode::from_u8(standard);
    let mut valid = 0u32;
    let mut total = 0u32;

    for proto in &chunk.protos {
        for i in 0..proto.code.len().saturating_sub(1) {
            if insn_op(proto.code[i]) != shuffled { continue; }
            total += 1;
            let aux = proto.code[i + 1];
            let insn = proto.code[i];
            let a = insn_a(insn);

            let is_valid = match op {
                LuauOpcode::GetTableKS | LuauOpcode::SetTableKS => {
                    a < proto.max_stack_size
                        && (aux as usize) < proto.constants.len()
                        && matches!(proto.constants.get(aux as usize), Some(Constant::String(_)))
                }
                LuauOpcode::NameCall => {
                    a < proto.max_stack_size
                        && ((aux as usize) < proto.constants.len()
                            && matches!(proto.constants.get(aux as usize), Some(Constant::String(_))))
                }
                LuauOpcode::GetGlobal | LuauOpcode::SetGlobal => {
                    // AUX is typically a 0-based index into proto.constants (String constant).
                    // Accept via strategies matching remap_chunk validation:
                    // 1. proto.constants[aux] is String (primary path)
                    // 2. proto.constants[aux] is Import (dotted name like game.Workspace)
                    // 3. chunk.strings[aux] exists, but only when AUX >= proto.constants.len()
                    //    (avoids false positives when AUX is in range but wrong type)
                    let primary = (aux as usize) < proto.constants.len()
                        && matches!(proto.constants.get(aux as usize), Some(Constant::String(_)));
                    let import = !primary
                        && (aux as usize) < proto.constants.len()
                        && matches!(proto.constants.get(aux as usize), Some(Constant::Import(_)));
                    let chunk_str = !primary && !import
                        && (aux as usize) >= proto.constants.len()
                        && (aux as usize) < chunk.strings.len();
                    a < proto.max_stack_size && (primary || import || chunk_str)
                }
                LuauOpcode::GetImport => {
                    let count = aux >> 30;
                    let d = insn_d(insn);
                    let id0 = (aux >> 20) & 0x3FF;
                    let id1 = (aux >> 10) & 0x3FF;
                    let id2 = aux & 0x3FF;
                    // Import IDs are indices into chunk.strings (global string table)
                    let ids_valid = match count {
                        1 => (id0 as usize) < chunk.strings.len(),
                        2 => (id0 as usize) < chunk.strings.len()
                            && (id1 as usize) < chunk.strings.len(),
                        3 => (id0 as usize) < chunk.strings.len()
                            && (id1 as usize) < chunk.strings.len()
                            && (id2 as usize) < chunk.strings.len(),
                        _ => false,
                    };
                    count >= 1 && count <= 3
                        && ids_valid
                        && d >= 0
                        && (d as u16 as usize) < proto.constants.len()
                }
                LuauOpcode::NewTable => {
                    aux <= 128 && insn_c(insn) == 0 && insn_b(insn) <= 64
                }
                LuauOpcode::SetList => {
                    aux <= 2048
                }
                LuauOpcode::ForGLoop => {
                    // AUX encodes nresults in low bits; bit 31 is the "inext" flag
                    // (signals ipairs-style integer iteration in newer Luau versions).
                    // Mask off bit 31 before validating the variable count.
                    let nresults = aux & 0x7FFFFFFF;
                    nresults >= 1 && nresults <= 10
                }
                LuauOpcode::JumpIfEq | LuauOpcode::JumpIfLE | LuauOpcode::JumpIfLT
                | LuauOpcode::JumpIfNotEq | LuauOpcode::JumpIfNotLE | LuauOpcode::JumpIfNotLT => {
                    let d = insn_d(insn) as i32;
                    (aux & 0xFF) < proto.max_stack_size as u32
                        && d != 0
                        && (i as i32 + d) >= 0
                        && ((i as i32 + d) as usize) < proto.code.len()
                }
                LuauOpcode::JumpXEqKNil => {
                    let d = insn_d(insn) as i32;
                    (aux & 0x7FFFFFFF) == 0 && d > 0
                        && ((i as i32 + d) as usize) < proto.code.len()
                }
                LuauOpcode::JumpXEqKB => {
                    let d = insn_d(insn) as i32;
                    (aux & 0x7FFFFFFF) <= 1 && d > 0
                        && ((i as i32 + d) as usize) < proto.code.len()
                }
                LuauOpcode::JumpXEqKN => {
                    let kidx = (aux & 0x00FFFFFF) as usize;
                    let d = insn_d(insn) as i32;
                    kidx < proto.constants.len()
                        && matches!(proto.constants.get(kidx), Some(Constant::Number(_)))
                        && d > 0
                }
                LuauOpcode::JumpXEqKS => {
                    let kidx = (aux & 0x00FFFFFF) as usize;
                    let d = insn_d(insn) as i32;
                    kidx < proto.constants.len()
                        && matches!(proto.constants.get(kidx), Some(Constant::String(_)))
                        && d > 0
                }
                LuauOpcode::FastCall2 => {
                    (aux & 0xFF) < proto.max_stack_size as u32
                        && insn_a(insn) <= 127
                }
                LuauOpcode::FastCall2K => {
                    (aux as usize) < proto.constants.len()
                        && insn_a(insn) <= 127
                }
                LuauOpcode::FastCall3 => {
                    (aux & 0xFF) < proto.max_stack_size as u32
                        && ((aux >> 8) & 0xFF) < proto.max_stack_size as u32
                        && insn_a(insn) <= 127
                }
                LuauOpcode::LoadKX => {
                    (aux as usize) < proto.constants.len()
                }
                _ => true,
            };

            if is_valid { valid += 1; }
        }
    }

    if total == 0 { return 0.0; }
    valid as f64 / total as f64
}

/// Walk the instruction stream using the current (partial) opcode map to identify
/// shuffled bytes that appear at true instruction positions but aren't yet mapped.
///
/// This is more reliable than raw frequency counting because it uses the known AUX
/// structure of mapped opcodes to skip AUX words, giving a clean picture of which
/// bytes actually appear as opcodes vs which only appear as AUX data.
///
/// When very few standard opcodes remain unmapped (e.g., just the deprecated opcode 61),
/// and exactly one unmapped byte appears consistently at instruction positions, we can
/// confidently assign it.
fn infer_from_instruction_positions(chunk: &Chunk, ctx: &mut DetectCtx) {
    // Count unmapped standard opcodes — EXCLUDING structural-required ones.
    // NEWTABLE, FORGLOOP, FORGPREP variants need dedicated structural evidence;
    // never guess them from frequency/AUX behavior.
    //
    // ALSO exclude rare-never opcodes (Nop, Break, Coverage, NativeCall).
    // These should only be mapped to freq==0 bytes by the dedicated
    // rare-op assignment phases. Letting them participate in the
    // instruction-position zip match causes them to steal low-freq bytes.
    //
    // NOTE: Deprecated61 (ForGLoopINext) is intentionally NOT in this list.
    // Roblox's Luau compiler actively emits it for ipairs loops;
    // detect_forgloopinext handles it via structural evidence.
    //
    // ALSO exclude ALL rare standard opcodes (SubRK, DivRK, LoadKX, IDiv,
    // IDivK, FastCall3, plus the rare-never set). These have dedicated
    // detectors (detect_subrk_divrk, detect_idiv_idivk, etc.); if the
    // dedicated detector couldn't find them, the instruction-position
    // zip-match MUST NOT guess because it has no structural evidence.
    // Enforcing UNMAPPED > WRONG: a 1-hit byte paired by rarity-sort with
    // a rare opcode is a coin flip, and wrong mappings cascade garbage into
    // the lifter (LoadKX eats an AUX word, SubRK/DivRK swallow adjacent
    // instructions). Better to leave the byte unmapped.
    let is_rare_never = |s: u8| matches!(LuauOpcode::from_u8(s),
        LuauOpcode::Nop | LuauOpcode::Break | LuauOpcode::Coverage
        | LuauOpcode::NativeCall);
    let unmapped_standard: Vec<u8> = (0..84u8)
        .filter(|&s| !ctx.assigned[s as usize]
            && !DetectCtx::is_structural_required_standard_opcode(s)
            && !is_rare_never(s)
            && !DetectCtx::is_rare_standard_opcode(s)
            && DetectCtx::opcode_can_appear_in_chunk(chunk, s))
        .collect();

    if unmapped_standard.is_empty() { return; }

    // Walk instruction stream, counting occurrences of unmapped bytes at instruction positions.
    // When we hit a mapped opcode, we know its AUX status and can skip correctly.
    // When we hit an unmapped byte, we step by 1 (conservative — may miscount if it has AUX,
    // but the frequency pattern still helps distinguish opcodes from data).
    let mut insn_pos_freq = [0u32; 256]; // frequency at instruction positions only
    for proto in &chunk.protos {
        let code = &proto.code;
        let mut i = 0;
        while i < code.len() {
            let op = insn_op(code[i]);
            let mapped = ctx.map[op as usize];
            if mapped != 255 {
                // Known opcode — count it and skip AUX if needed
                let standard_op = LuauOpcode::from_u8(mapped);
                if standard_op.has_aux() {
                    i += 2;
                } else {
                    i += 1;
                }
            } else {
                // Unmapped byte at an instruction position
                insn_pos_freq[op as usize] += 1;
                i += 1;
            }
        }
    }

    // Collect unmapped bytes that appear at instruction positions
    let mut candidates: Vec<(u8, u32)> = (0..=255u8)
        .filter(|&s| ctx.map[s as usize] == 255 && insn_pos_freq[s as usize] > 0)
        .map(|s| (s, insn_pos_freq[s as usize]))
        .collect();
    // Deterministic: byte ascending when frequency ties.
    candidates.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    if candidates.is_empty() { return; }

    // If we have exactly as many (or fewer) unmapped standard opcodes as candidates,
    // we can do targeted assignment. Pair them by AUX compatibility + frequency.
    if unmapped_standard.len() <= candidates.len() {
        // Detect AUX behavior for the candidate bytes
        let aux_behavior = detect_aux_behavior(chunk, ctx);

        // Classify unmapped standard opcodes
        let mut noaux_std: Vec<u8> = unmapped_standard.iter()
            .filter(|&&s| !LuauOpcode::from_u8(s).has_aux())
            .copied().collect();
        let mut aux_std: Vec<u8> = unmapped_standard.iter()
            .filter(|&&s| LuauOpcode::from_u8(s).has_aux())
            .copied().collect();

        // Sort by expected rarity (rare opcodes last), then opcode ascending.
        // Deterministic tiebreak ensures the same input produces the same output.
        let rarity = |op: u8| -> u32 {
            if DetectCtx::is_rare_standard_opcode(op) { 10 }
            else if DetectCtx::is_common_standard_opcode(op) { 0 }
            else { 5 }
        };
        noaux_std.sort_by(|a, b| rarity(*a).cmp(&rarity(*b)).then_with(|| a.cmp(b)));
        aux_std.sort_by(|a, b| rarity(*a).cmp(&rarity(*b)).then_with(|| a.cmp(b)));

        // Split candidates by AUX behavior
        let mut noaux_cands: Vec<u8> = candidates.iter()
            .filter(|&&(s, _)| aux_behavior.get(&s) != Some(&true))
            .map(|&(s, _)| s)
            .collect();
        let mut aux_cands: Vec<u8> = candidates.iter()
            .filter(|&&(s, _)| aux_behavior.get(&s) == Some(&true))
            .map(|&(s, _)| s)
            .collect();
        let ambiguous_cands: Vec<u8> = candidates.iter()
            .filter(|&&(s, _)| !aux_behavior.contains_key(&s))
            .map(|&(s, _)| s)
            .collect();

        // Sort by instruction-position frequency descending (common opcodes -> high freq bytes),
        // tiebreak by byte value ascending for deterministic ordering.
        noaux_cands.sort_by(|a, b| insn_pos_freq[*b as usize].cmp(&insn_pos_freq[*a as usize])
            .then_with(|| a.cmp(b)));
        aux_cands.sort_by(|a, b| insn_pos_freq[*b as usize].cmp(&insn_pos_freq[*a as usize])
            .then_with(|| a.cmp(b)));

        // Assign non-AUX: high-freq candidate -> common standard opcode
        for (cand, std_op) in noaux_cands.iter().zip(noaux_std.iter()) {
            if !ctx.is_mapped(*cand) && !ctx.assigned[*std_op as usize] {
                ctx.try_assign_force(*cand, *std_op);
            }
        }

        // Assign AUX: high-freq candidate -> common AUX standard opcode
        for (cand, std_op) in aux_cands.iter().zip(aux_std.iter()) {
            if !ctx.is_mapped(*cand) && !ctx.assigned[*std_op as usize] {
                ctx.try_assign_force(*cand, *std_op);
            }
        }

        // Assign ambiguous to whatever's left (excluding structural-required,
        // rare (dedicated-detector-only), and chunk-impossible opcodes like
        // LoadKX on small files). Same reasoning as unmapped_standard above:
        // rare opcodes should only be assigned by their dedicated detectors.
        let leftover_std: Vec<u8> = (0..84u8)
            .filter(|&s| !ctx.assigned[s as usize]
                && !DetectCtx::is_structural_required_standard_opcode(s)
                && !is_rare_never(s)
                && !DetectCtx::is_rare_standard_opcode(s)
                && DetectCtx::opcode_can_appear_in_chunk(chunk, s))
            .collect();
        for (cand, std_op) in ambiguous_cands.iter().zip(leftover_std.iter()) {
            if !ctx.is_mapped(*cand) && !ctx.assigned[*std_op as usize] {
                ctx.try_assign_force(*cand, *std_op);
            }
        }
    }
}

/// TIER 9: Permutation completion — fills ALL remaining gaps using the bijection constraint.
///
/// The opcode shuffle is a permutation of opcodes 0-83: each real opcode maps to exactly
/// one shuffled byte, and each shuffled byte in the active range maps to exactly one opcode.
/// After heuristic detection finds ~40-60 opcodes, this function resolves the rest by:
///
/// 1. Assigning rare/never-appearing opcodes (NOP, BREAK, etc.) to zero-freq bytes upfront
/// 2. Scoring all remaining (shuffled_byte, standard_opcode) pairs
/// 3. Greedy assignment at progressively lower thresholds (0.30, 0.20, 0.15)
/// 4. For small N (<=8 remaining): brute-force all permutations to find optimal assignment
/// 5. For larger N: scored greedy matching (not blind frequency-rank zip)
fn permutation_complete(chunk: &Chunk, ctx: &mut DetectCtx) {
    // Collect unmapped standard opcodes (0-83, including deprecated 61 which
    // still participates in Roblox's opcode shuffle permutation).
    //
    // EXCLUDE structural-required opcodes: NewTable, ForGLoop, ForGPrep variants,
    // ForNPrep, ForNLoop. These need structural evidence from their dedicated
    // detectors — guessing them via AUX-shape/frequency leads to catastrophic
    // cache poisoning (e.g., `(-tbl).field` because NewTable got mapped to Not).
    // Prefer UNMAPPED over WRONG; cache accumulation will fill them in on
    // subsequent files where the structural signal is clear.
    //
    // ALSO exclude opcodes that cannot structurally appear in THIS chunk
    // (e.g. LoadKX when no proto has > 32768 constants). See
    // `opcode_can_appear_in_chunk`.
    let unmapped_standard: Vec<u8> = (0..84u8)
        .filter(|&s| !ctx.assigned[s as usize]
            && !DetectCtx::is_structural_required_standard_opcode(s)
            && DetectCtx::opcode_can_appear_in_chunk(chunk, s))
        .collect();

    if unmapped_standard.is_empty() { return; }

    // ── Phase 0: Assign rare/never-appearing standard opcodes to zero-freq bytes FIRST ──
    // Opcodes like NOP, BREAK, COVERAGE, NATIVECALL typically never appear in real
    // bytecode, so their shuffled bytes have freq=0. Handle them upfront to keep them
    // out of the main matching logic where they would consume candidate slots.
    //
    // NOTE: DEPRECATED61 (ForGLoopINext) is intentionally NOT included here.
    // Roblox's Luau compiler still emits this opcode for ipairs-style generic for
    // loops; it appears with high frequency (50-70x) in scripts like Animate.lua.
    // detect_forgloopinext finds its true shuffled byte via structural evidence.
    let rare_standard: Vec<u8> = unmapped_standard.iter()
        .filter(|&&s| matches!(LuauOpcode::from_u8(s),
            LuauOpcode::Nop | LuauOpcode::Break | LuauOpcode::Coverage
            | LuauOpcode::NativeCall))
        .copied().collect();

    if !rare_standard.is_empty() {
        let mut zero_freq_bytes: Vec<u8> = (0..=255u8)
            .filter(|&s| !ctx.is_mapped(s) && ctx.freq[s as usize] == 0)
            .collect();
        for std_op in &rare_standard {
            if ctx.assigned[*std_op as usize] { continue; }
            if let Some(shuffled) = zero_freq_bytes.pop() {
                ctx.try_assign_force(shuffled, *std_op);
            }
        }
    }

    // Re-collect after Phase 0 assignments — still exclude structural-required
    // and chunk-impossible opcodes.
    let unmapped_standard: Vec<u8> = (0..84u8)
        .filter(|&s| !ctx.assigned[s as usize]
            && !DetectCtx::is_structural_required_standard_opcode(s)
            && DetectCtx::opcode_can_appear_in_chunk(chunk, s))
        .collect();
    if unmapped_standard.is_empty() { return; }

    // Collect unmapped shuffled bytes that actually appear in bytecode
    let unmapped_shuffled: Vec<u8> = (0..=255u8)
        .filter(|&s| !ctx.is_mapped(s) && ctx.freq[s as usize] > 0)
        .collect();

    if unmapped_shuffled.is_empty() { return; }

    // Detect AUX behavior for unmapped bytes
    let aux_behavior = detect_aux_behavior(chunk, ctx);

    // Classify standard opcodes by whether they use AUX
    let std_has_aux: Vec<bool> = (0..=255u8)
        .map(|s| LuauOpcode::from_u8(s).has_aux())
        .collect();

    // ── Phase 1: Score all (shuffled, standard) pairs ──
    let mut pairs: Vec<(u8, u8, f64)> = Vec::new();

    for &shuffled in &unmapped_shuffled {
        if ctx.is_mapped(shuffled) { continue; }
        let s_freq = ctx.freq[shuffled as usize];
        let s_is_aux = aux_behavior.get(&shuffled) == Some(&true);
        let s_is_noaux = aux_behavior.get(&shuffled) == Some(&false);

        for &standard in &unmapped_standard {
            if ctx.assigned[standard as usize] { continue; }
            let op_wants_aux = std_has_aux[standard as usize];

            // AUX compatibility check — strong signal
            if s_is_aux && !op_wants_aux { continue; } // AUX byte can't be non-AUX opcode
            if s_is_noaux && op_wants_aux { continue; } // non-AUX byte can't be AUX opcode

            let mut score = 0.0f64;

            // AUX content validation (for AUX opcodes)
            if op_wants_aux && (s_is_aux || !s_is_noaux) {
                let aux_score = validate_aux_for_opcode(chunk, shuffled, standard);
                score += 0.40 * aux_score;
            }

            // Format validation (for non-AUX opcodes)
            if !op_wants_aux {
                let fmt_score = format_score_for_opcode(chunk, shuffled, standard);
                score += 0.40 * fmt_score;
            }

            // Frequency plausibility
            let freq_score = frequency_plausibility(ctx, s_freq, standard);
            score += 0.30 * freq_score;

            // AUX agreement bonus
            if s_is_aux && op_wants_aux { score += 0.15; }
            if s_is_noaux && !op_wants_aux { score += 0.15; }

            if score > 0.10 {
                pairs.push((shuffled, standard, score));
            }
        }
    }

    // Sort by score descending (integer-scaled to avoid NaN), then by shuffled byte then
    // standard opcode ascending — deterministic under HashMap iteration noise.
    pairs.sort_by(|a, b| {
        let sa = (a.2 * 1000.0) as i64;
        let sb = (b.2 * 1000.0) as i64;
        sb.cmp(&sa).then_with(|| a.0.cmp(&b.0)).then_with(|| a.1.cmp(&b.1))
    });

    // Greedy assignment — high-confidence pairs only.
    //
    // This cascade used to descend to 0.20 and then 0.15, which is where most of
    // the pipeline's invented mappings were actually made: by the time Phase 2
    // runs there is often nothing left for it to do. A score that low means the
    // AUX/format validator found the pair barely more plausible than chance, and
    // the resulting mapping is indistinguishable in the output from one a
    // structural detector earned. Bytes that clear no threshold are left unmapped
    // so `remap_chunk` can report them as unresolved instructions.
    for threshold in &[COMPLETION_MIN_SCORE] {
        for &(shuffled, standard, score) in &pairs {
            if ctx.is_mapped(shuffled) || ctx.assigned[standard as usize] { continue; }
            if score >= *threshold {
                ctx.try_assign_force(shuffled, standard);
            }
        }
    }

    // ── Phase 2: Small-N permutation brute force ──
    // When few opcodes remain, try all permutations to find the best global assignment.
    // This is far more accurate than frequency-rank zip matching for small N.
    // Still exclude structural-required ops — even brute-force would assign them
    // based on weak AUX-shape validation.
    let remaining_std: Vec<u8> = (0..84u8)
        .filter(|&s| !ctx.assigned[s as usize]
            && !DetectCtx::is_structural_required_standard_opcode(s)
            && DetectCtx::opcode_can_appear_in_chunk(chunk, s))
        .collect();
    let mut remaining_shuffled: Vec<u8> = (0..=255u8)
        .filter(|&s| !ctx.is_mapped(s) && ctx.freq[s as usize] > 0)
        .collect();

    if remaining_std.is_empty() || remaining_shuffled.is_empty() { return; }

    // Sort remaining shuffled by frequency descending, byte ascending as deterministic tiebreak.
    remaining_shuffled.sort_by(|a, b| ctx.freq[*b as usize].cmp(&ctx.freq[*a as usize])
        .then_with(|| a.cmp(b)));

    let n_std = remaining_std.len();
    let n_shuf = remaining_shuffled.len();

    // Brute-force is feasible up to N=8 (40320 permutations). Beyond that, use
    // scored greedy matching as a fallback.
    if n_std <= 8 && n_shuf <= 20 && n_std <= n_shuf {
        // Build a score matrix: score[i][j] = how well remaining_shuffled[j]
        // fits as remaining_std[i]
        let mut score_matrix: Vec<Vec<f64>> = Vec::with_capacity(n_std);
        for &std_op in &remaining_std {
            let mut row = Vec::with_capacity(n_shuf);
            for &shuf in &remaining_shuffled {
                let op_wants_aux = std_has_aux[std_op as usize];
                let s_is_aux = aux_behavior.get(&shuf) == Some(&true);
                let s_is_noaux = aux_behavior.get(&shuf) == Some(&false);

                // Hard incompatibility gets a large penalty
                if (s_is_aux && !op_wants_aux) || (s_is_noaux && op_wants_aux) {
                    row.push(-100.0);
                    continue;
                }

                let mut s = 0.0f64;
                if op_wants_aux {
                    s += validate_aux_for_opcode(chunk, shuf, std_op);
                } else {
                    s += format_score_for_opcode(chunk, shuf, std_op);
                }
                s += 0.3 * frequency_plausibility(ctx, ctx.freq[shuf as usize], std_op);
                if s_is_aux && op_wants_aux { s += 0.1; }
                if s_is_noaux && !op_wants_aux { s += 0.1; }
                row.push(s);
            }
            score_matrix.push(row);
        }

        // Try all permutations to find the best global assignment
        let best = find_best_permutation(&score_matrix, n_std, n_shuf);
        if let Some(assignment) = best {
            for (i, j) in assignment.iter().enumerate() {
                let std_op = remaining_std[i];
                let shuf = remaining_shuffled[*j];
                // The search maximises the TOTAL score, so an individual pair can
                // be arbitrarily bad as long as the rest of the permutation carries
                // it. Nothing here used to check that; whatever the argmax returned
                // was applied. Hold each pair to the same bar the greedy fallback
                // uses, and to the same requirement that it beat the alternatives
                // for its byte — otherwise leave the byte unmapped so it surfaces
                // as an unresolved instruction instead of a confident guess.
                let score = score_matrix[i][*j];
                if score < PERMUTATION_MIN_SCORE { continue; }
                let runner_up = (0..n_shuf)
                    .filter(|&k| k != *j)
                    .map(|k| score_matrix[i][k])
                    .fold(f64::NEG_INFINITY, f64::max);
                if runner_up.is_finite() && score - runner_up < COMPLETION_MIN_MARGIN {
                    continue;
                }
                if !ctx.is_mapped(shuf) && !ctx.assigned[std_op as usize] {
                    ctx.try_assign_force(shuf, std_op);
                }
            }
        }
    } else {
        // ── Fallback: Scored greedy matching for larger N ──
        // Instead of blind frequency-rank zip, score each pair and greedily
        // assign the best-scoring pairs first.
        let mut noaux_std: Vec<u8> = remaining_std.iter()
            .filter(|&&s| !std_has_aux[s as usize])
            .copied().collect();
        let mut aux_std: Vec<u8> = remaining_std.iter()
            .filter(|&&s| std_has_aux[s as usize])
            .copied().collect();

        let noaux_shuffled: Vec<u8> = remaining_shuffled.iter()
            .filter(|&&s| !ctx.is_mapped(s) && aux_behavior.get(&s) != Some(&true))
            .copied().collect();
        let aux_shuffled: Vec<u8> = remaining_shuffled.iter()
            .filter(|&&s| !ctx.is_mapped(s) && aux_behavior.get(&s) == Some(&true))
            .copied().collect();
        let ambiguous_shuffled: Vec<u8> = remaining_shuffled.iter()
            .filter(|&&s| !ctx.is_mapped(s) && !aux_behavior.contains_key(&s))
            .copied().collect();

        // Non-AUX: scored greedy assignment
        scored_greedy_assign(chunk, ctx, &mut noaux_std, &noaux_shuffled, &std_has_aux, &aux_behavior, COMPLETION_MIN_SCORE);

        // AUX: scored greedy assignment
        scored_greedy_assign(chunk, ctx, &mut aux_std, &aux_shuffled, &std_has_aux, &aux_behavior, COMPLETION_MIN_SCORE);

        // The third bucket — bytes whose AUX behaviour could not be classified at
        // all — used to be handed EVERY remaining non-structural opcode with no
        // score floor whatsoever. That is the most speculative assignment in the
        // pipeline: an unknown byte paired with an arbitrary opcode purely to
        // complete the bijection. Leaving those bytes unmapped lets `remap_chunk`
        // report them as unresolved instructions, which is the honest outcome —
        // an unmapped opcode is visible, a wrong one is not.
        let _ = &ambiguous_shuffled;
    }

    // ── Final: assign remaining zero-freq shuffled bytes to any leftover standard opcodes ──
    // (excluding structural-required — those must stay unmapped if not detected)
    let leftover_std: Vec<u8> = (0..84u8)
        .filter(|&s| !ctx.assigned[s as usize]
            && !DetectCtx::is_structural_required_standard_opcode(s))
        .collect();
    if !leftover_std.is_empty() {
        let mut zero_freq: Vec<u8> = (0..=255u8)
            .filter(|&s| !ctx.is_mapped(s) && ctx.freq[s as usize] == 0)
            .collect();
        for standard in leftover_std {
            if let Some(shuffled) = zero_freq.pop() {
                ctx.try_assign_force(shuffled, standard);
            }
        }
    }
}

/// Greedy assignment: for each (standard, shuffled) pair, score compatibility and
/// assign the best-scoring pairs first. Much better than zip-matching because it
/// considers per-pair compatibility instead of relying on frequency-rank alignment.
/// Score below which bijection completion refuses to guess.
///
/// Matches the first of the three thresholds Phase 1 of `permutation_complete`
/// already applies. The greedy fallback below used to apply none at all: it
/// computed a compatibility score for every pair, sorted by it, and then threw it
/// away — the assignment loop destructured it into `_score`. Every remaining byte
/// was therefore mapped no matter how badly it fitted, purely to complete the
/// bijection, and the result was indistinguishable in the output from a mapping
/// the structural detectors had actually earned.
const COMPLETION_MIN_SCORE: f64 = 0.45;

/// How much better the winning opcode must fit a byte than the runner-up before
/// bijection completion is allowed to commit to it. See the ambiguity gate in
/// `scored_greedy_assign`.
const COMPLETION_MIN_MARGIN: f64 = 0.05;

/// Per-pair floor for the small-N brute-force path. Scored on a different scale
/// from `COMPLETION_MIN_SCORE`: that phase caps around 0.85, this one adds an
/// AUX-agreement bonus on top of a full-weight format score and reaches ~1.4.
const PERMUTATION_MIN_SCORE: f64 = 0.75;

fn scored_greedy_assign(
    chunk: &Chunk,
    ctx: &mut DetectCtx,
    std_ops: &mut Vec<u8>,
    shuffled_candidates: &[u8],
    std_has_aux: &[bool],
    aux_behavior: &HashMap<u8, bool>,
    min_score: f64,
) {
    let mut scored_pairs: Vec<(u8, u8, f64)> = Vec::new();
    for &std_op in std_ops.iter() {
        if ctx.assigned[std_op as usize] { continue; }
        let op_wants_aux = std_has_aux[std_op as usize];
        for &shuf in shuffled_candidates {
            if ctx.is_mapped(shuf) { continue; }
            let s_is_aux = aux_behavior.get(&shuf) == Some(&true);
            let s_is_noaux = aux_behavior.get(&shuf) == Some(&false);
            if (s_is_aux && !op_wants_aux) || (s_is_noaux && op_wants_aux) { continue; }

            let mut score = 0.0f64;
            if op_wants_aux {
                score += validate_aux_for_opcode(chunk, shuf, std_op);
            } else {
                score += format_score_for_opcode(chunk, shuf, std_op);
            }
            score += 0.3 * frequency_plausibility(ctx, ctx.freq[shuf as usize], std_op);
            scored_pairs.push((shuf, std_op, score));
        }
    }
    scored_pairs.sort_by(|a, b| {
        let sa = (a.2 * 1000.0) as i64;
        let sb = (b.2 * 1000.0) as i64;
        sb.cmp(&sa).then_with(|| a.0.cmp(&b.0)).then_with(|| a.1.cmp(&b.1))
    });
    // Ambiguity gate.
    //
    // A score floor alone is toothless here, and it is worth saying why. The
    // per-pair score is dominated by `format_score_for_opcode`, which collapses
    // the non-AUX opcodes into a handful of behavioural classes — a dozen of them
    // reduce to "A is a valid register". Most pairs therefore score near the top
    // of the range, and a threshold that rejects anything also rejects everything.
    //
    // What the score CAN express is whether one opcode fits a byte better than
    // the alternatives. When the best and second-best candidates for a byte are
    // indistinguishable, completion is choosing by sort order, not by evidence,
    // and the resulting mapping is a coin flip presented as a fact. Leave those
    // bytes unmapped: `remap_chunk` reports an unmapped opcode as an unresolved
    // instruction, which is visible, whereas a wrong opcode is not.
    let mut best_for_byte: HashMap<u8, (f64, f64)> = HashMap::new(); // shuf -> (best, second)
    for &(shuf, _std_op, score) in &scored_pairs {
        let e = best_for_byte.entry(shuf).or_insert((f64::NEG_INFINITY, f64::NEG_INFINITY));
        if score > e.0 {
            e.1 = e.0;
            e.0 = score;
        } else if score > e.1 {
            e.1 = score;
        }
    }

    for &(shuf, std_op, score) in &scored_pairs {
        if score < min_score { break; } // sorted descending — nothing after this qualifies
        if ctx.is_mapped(shuf) || ctx.assigned[std_op as usize] { continue; }
        if let Some(&(best, second)) = best_for_byte.get(&shuf) {
            if second.is_finite() && best - second < COMPLETION_MIN_MARGIN {
                continue;
            }
        }
        ctx.try_assign_force(shuf, std_op);
    }
}

/// Find the best permutation assignment of n_std standard opcodes to n_shuf shuffled bytes.
/// Returns the column indices for each row (standard opcode) that maximize total score.
/// Uses recursive search with pruning for N <= 8.
fn find_best_permutation(score_matrix: &[Vec<f64>], n_std: usize, n_shuf: usize) -> Option<Vec<usize>> {
    let mut best_score = f64::NEG_INFINITY;
    let mut best_assignment: Option<Vec<usize>> = None;
    let mut current = vec![0usize; n_std];
    let mut used = vec![false; n_shuf];

    fn search(
        row: usize,
        n_std: usize,
        n_shuf: usize,
        score_matrix: &[Vec<f64>],
        current: &mut Vec<usize>,
        used: &mut Vec<bool>,
        running_score: f64,
        best_score: &mut f64,
        best_assignment: &mut Option<Vec<usize>>,
    ) {
        if row == n_std {
            if running_score > *best_score {
                *best_score = running_score;
                *best_assignment = Some(current.clone());
            }
            return;
        }
        // Pruning: even if all remaining rows score 1.5 (theoretical max), can we beat best?
        let max_remaining = (n_std - row) as f64 * 1.5;
        if running_score + max_remaining <= *best_score {
            return;
        }
        for j in 0..n_shuf {
            if used[j] { continue; }
            let pair_score = score_matrix[row][j];
            if pair_score <= -50.0 { continue; } // Skip hard incompatibilities
            used[j] = true;
            current[row] = j;
            search(row + 1, n_std, n_shuf, score_matrix, current, used,
                   running_score + pair_score, best_score, best_assignment);
            used[j] = false;
        }
    }

    search(0, n_std, n_shuf, score_matrix, &mut current, &mut used, 0.0, &mut best_score, &mut best_assignment);
    best_assignment
}

/// Score how well a shuffled byte fits as a specific non-AUX opcode based on format validation.
fn format_score_for_opcode(chunk: &Chunk, shuffled: u8, standard: u8) -> f64 {
    let mut valid = 0u32;
    let mut total = 0u32;
    let op = LuauOpcode::from_u8(standard);

    for proto in &chunk.protos {
        for &insn in &proto.code {
            if insn_op(insn) != shuffled { continue; }
            total += 1;
            let a = insn_a(insn);
            let b = insn_b(insn);
            let c = insn_c(insn);
            let d = insn_d(insn);
            let ms = proto.max_stack_size;
            let _nc = proto.constants.len() as u8;
            let nu = proto.num_upvalues.max(1);

            let ok = match op {
                // AB0 format (C must be 0)
                LuauOpcode::Move | LuauOpcode::Not | LuauOpcode::Minus | LuauOpcode::Length =>
                    c == 0 && a < ms && b < ms,
                LuauOpcode::GetUpval => c == 0 && a < ms && b < nu,
                LuauOpcode::SetUpval => c == 0 && a < ms && b < nu,
                LuauOpcode::CloseUpvals => b == 0 && c == 0 && a < ms,
                // AD format (signed D)
                LuauOpcode::LoadNil => d == 0 && a < ms,
                LuauOpcode::LoadN => a < ms,
                LuauOpcode::LoadK => a < ms && (d as u16 as usize) < proto.constants.len(),
                LuauOpcode::LoadB => a < ms && b <= 1 && c <= 1,
                LuauOpcode::Jump => d != 0,
                LuauOpcode::JumpBack => d < 0,
                LuauOpcode::JumpIf | LuauOpcode::JumpIfNot => a < ms && d != 0,
                // NEWCLOSURE: D is a direct index into proto.child_protos.
                LuauOpcode::NewClosure =>
                    a < ms && (d as u16 as usize) < proto.child_protos.len().max(1),
                // DUPCLOSURE: D is a CONSTANTS index, and constants[D] must be
                // a Closure variant. This is NOT a child_protos index — validating
                // against child_protos.len() rejected legitimate DUPCLOSURE bytes,
                // causing "unresolved closure" markers in the lifter output.
                LuauOpcode::DupClosure =>
                    a < ms
                        && d >= 0
                        && matches!(
                            proto.constants.get(d as u16 as usize),
                            Some(Constant::Closure(_))
                        ),
                LuauOpcode::DupTable => a < ms && (d as u16 as usize) < proto.constants.len(),
                LuauOpcode::ForNPrep | LuauOpcode::ForNLoop => a < ms,
                LuauOpcode::ForGPrep | LuauOpcode::ForGPrepINext | LuauOpcode::ForGPrepNext =>
                    a < ms,
                // ABC reg,reg,reg
                LuauOpcode::Add | LuauOpcode::Sub | LuauOpcode::Mul |
                LuauOpcode::Div | LuauOpcode::Mod | LuauOpcode::Pow |
                LuauOpcode::IDiv =>
                    a < ms && b < ms && c < ms,
                LuauOpcode::And | LuauOpcode::Or | LuauOpcode::GetTable | LuauOpcode::SetTable =>
                    a < ms && b < ms && c < ms,
                // ABC reg,reg,const
                LuauOpcode::AddK | LuauOpcode::SubK | LuauOpcode::MulK |
                LuauOpcode::DivK | LuauOpcode::ModK | LuauOpcode::PowK |
                LuauOpcode::IDivK | LuauOpcode::AndK | LuauOpcode::OrK =>
                    a < ms && b < ms && (c as usize) < proto.constants.len(),
                LuauOpcode::SubRK | LuauOpcode::DivRK =>
                    a < ms && (b as usize) < proto.constants.len() && c < ms,
                // ABC with special constraints
                LuauOpcode::Concat => a < ms && b < ms && c < ms && b <= c,
                LuauOpcode::GetTableN | LuauOpcode::SetTableN => a < ms && b < ms,
                LuauOpcode::Capture => a <= 2,
                LuauOpcode::Call => a < ms,
                LuauOpcode::Return => true,
                LuauOpcode::GetVarargs => a < ms,
                LuauOpcode::PrepVarargs => true,
                LuauOpcode::FastCall | LuauOpcode::FastCall1 => a <= 112,
                // Default: accept if A < maxstack (loose)
                _ => a < ms,
            };
            if ok { valid += 1; }
        }
    }
    if total == 0 { return 0.0; }
    valid as f64 / total as f64
}

/// Score frequency plausibility: does the shuffled byte's frequency match what we'd expect
/// for this standard opcode?
fn frequency_plausibility(ctx: &DetectCtx, shuffled_freq: u32, standard: u8) -> f64 {
    if ctx.total_insns == 0 { return 0.5; }
    let pct = (shuffled_freq as f64 / ctx.total_insns as f64) * 100.0;

    // Expected frequency tiers
    let (min_pct, max_pct) = match LuauOpcode::from_u8(standard) {
        // Very common (>2%)
        LuauOpcode::Move | LuauOpcode::Call | LuauOpcode::Return |
        LuauOpcode::LoadK | LuauOpcode::GetImport | LuauOpcode::GetTableKS |
        LuauOpcode::Jump | LuauOpcode::JumpIfNot | LuauOpcode::Capture =>
            (0.5, 30.0),
        // Common (0.5-5%)
        LuauOpcode::NameCall | LuauOpcode::NewClosure | LuauOpcode::SetTableKS |
        LuauOpcode::JumpIf | LuauOpcode::LoadB | LuauOpcode::LoadN |
        LuauOpcode::LoadNil | LuauOpcode::SetList | LuauOpcode::NewTable =>
            (0.1, 10.0),
        // Moderate (0.01-2%)
        LuauOpcode::Add | LuauOpcode::Sub | LuauOpcode::Mul | LuauOpcode::Div |
        LuauOpcode::GetUpval | LuauOpcode::SetUpval | LuauOpcode::Concat |
        LuauOpcode::And | LuauOpcode::Or | LuauOpcode::GetTable | LuauOpcode::SetTable =>
            (0.01, 5.0),
        // Rare (<0.5%)
        LuauOpcode::Nop | LuauOpcode::Break | LuauOpcode::Coverage |
        LuauOpcode::NativeCall | LuauOpcode::LoadKX | LuauOpcode::JumpX |
        LuauOpcode::FastCall3 | LuauOpcode::SubRK | LuauOpcode::DivRK |
        LuauOpcode::IDiv | LuauOpcode::IDivK =>
            (0.0, 1.0),
        // Default moderate
        _ => (0.0, 10.0),
    };

    if pct >= min_pct && pct <= max_pct { 1.0 }
    else if pct < min_pct { (pct / min_pct.max(0.001)).min(1.0) * 0.5 }
    else { (max_pct / pct).min(1.0) * 0.5 }
}

/// TIER 7: Frequency-rank matching for remaining unmapped opcodes.
/// After all pattern-based detectors run, match remaining unmapped shuffled bytes
/// to remaining unmapped standard opcodes using statistical + structural analysis.
///
/// Strategy:
/// - Phase 1: Map near-zero opcodes to zero-frequency shuffled bytes (safe)
/// - Phase 2: Match non-AUX opcodes by frequency rank (safe — wrong assignment just
///   means wrong op name like ADD vs SUB, but no AUX cascading failures)
/// - Phase 3: Match AUX opcodes with content validation (validates AUX word format)
fn detect_frequency_rank_matching(chunk: &Chunk, ctx: &mut DetectCtx) {
    if ctx.total_insns < 100 { return; }

    // ── Phase 1: Map rare opcodes to zero-frequency shuffled bytes ──
    let rare_standard: Vec<u8> = [
        LuauOpcode::Nop, LuauOpcode::Break,
        LuauOpcode::Coverage, LuauOpcode::NativeCall,
    ].iter()
        .filter(|&&op| !ctx.assigned[op as usize])
        .map(|&op| op as u8)
        .collect();

    let mut zero_freq: Vec<u8> = (0..=255u8)
        .filter(|&s| !ctx.is_mapped(s) && ctx.freq[s as usize] == 0)
        .collect();

    for std_op in rare_standard {
        if let Some(shuffled) = zero_freq.pop() {
            ctx.try_assign(shuffled, std_op);
        }
    }

    // ── Phase 2: Detect AUX behavior for unmapped shuffled bytes ──
    let aux_behavior = detect_aux_behavior(chunk, ctx);

    let aux_standard_ops: std::collections::HashSet<u8> = (0..84u8)
        .filter(|&std_op| LuauOpcode::from_u8(std_op).has_aux())
        .collect();

    // ── Phase 3: Format-validated non-AUX matching ──
    // ONLY assign non-AUX opcodes when we can structurally verify the match.
    // We do NOT do blind frequency-rank matching — that causes catastrophic mis-mappings.
    // Instead, for each remaining unmapped non-AUX standard opcode, check if exactly one
    // unmapped shuffled byte passes format validation for that opcode.
    detect_format_validated_noaux(chunk, ctx, &aux_behavior, &aux_standard_ops);

    // ── Phase 4: Match AUX opcodes with content validation ──
    // For each unmapped AUX standard opcode, try each unmapped AUX-behaving shuffled byte.
    // Validate the AUX word contents for each candidate and only assign if >70% validate.

    // Priority order for AUX opcodes (try most distinctive first).
    //
    // NEWTABLE and FORGLOOP are intentionally EXCLUDED — both have dedicated
    // structural detectors (detect_newtable, detect_generic_for). If those
    // couldn't identify the byte, no amount of AUX-shape matching is trustworthy:
    // `validate_aux_for_opcode(NewTable)` only checks `c==0 && b<=64 && aux<=128`,
    // which matches many wrong bytes. Prefer UNMAPPED over WRONG — cache
    // accumulation from other files will fill in the missing entry.
    let aux_priority: Vec<u8> = [
        // Most distinctive AUX formats (easy to validate)
        LuauOpcode::GetTableKS, LuauOpcode::SetTableKS, LuauOpcode::NameCall,
        LuauOpcode::GetGlobal, LuauOpcode::SetGlobal, LuauOpcode::GetImport,
        LuauOpcode::JumpXEqKS, LuauOpcode::JumpXEqKN,
        LuauOpcode::JumpXEqKNil, LuauOpcode::JumpXEqKB,
        // Comparison jumps
        LuauOpcode::JumpIfEq, LuauOpcode::JumpIfNotEq,
        LuauOpcode::JumpIfLT, LuauOpcode::JumpIfNotLT,
        LuauOpcode::JumpIfLE, LuauOpcode::JumpIfNotLE,
        // Other AUX opcodes (NewTable and ForGLoop removed — structural-required)
        LuauOpcode::SetList,
        LuauOpcode::FastCall2, LuauOpcode::FastCall2K, LuauOpcode::FastCall3,
        LuauOpcode::LoadKX,
    ].iter()
        .map(|&op| op as u8)
        .filter(|&s| !ctx.assigned[s as usize]
            && aux_standard_ops.contains(&s)
            && !DetectCtx::is_structural_required_standard_opcode(s)
            && DetectCtx::opcode_can_appear_in_chunk(chunk, s))
        .collect();

    let unmapped_aux_shuffled: Vec<(u8, u32)> = (0..=255u8)
        .filter(|&s| !ctx.is_mapped(s) && ctx.freq[s as usize] > 0)
        .filter(|&s| aux_behavior.get(&s) == Some(&true))
        .map(|s| (s, ctx.freq[s as usize]))
        .collect();

    for &std_op in &aux_priority {
        if ctx.assigned[std_op as usize] { continue; }
        let mut best_match: Option<(u8, f64)> = None;
        for &(shuffled, _freq) in &unmapped_aux_shuffled {
            if ctx.is_mapped(shuffled) { continue; }
            let score = validate_aux_for_opcode(chunk, shuffled, std_op);
            if score >= 0.55 {
                if best_match.is_none() || score > best_match.unwrap().1 {
                    best_match = Some((shuffled, score));
                }
            }
        }
        if let Some((shuffled, _score)) = best_match {
            ctx.try_assign(shuffled, std_op);
        }
    }

    // ── Phase 5: Handle ambiguous AUX bytes (no confident AUX behavior detected) ──
    // Some shuffled bytes may not have clear AUX/non-AUX classification.
    // For these, try AUX validation — if a candidate validates well for an AUX opcode,
    // assign it; otherwise leave it unmapped (safer than guessing).
    let ambiguous_shuffled: Vec<(u8, u32)> = (0..=255u8)
        .filter(|&s| !ctx.is_mapped(s) && ctx.freq[s as usize] > 0)
        .filter(|&s| !aux_behavior.contains_key(&s))
        .map(|s| (s, ctx.freq[s as usize]))
        .collect();

    for &std_op in &aux_priority {
        if ctx.assigned[std_op as usize] { continue; }
        let mut best_match: Option<(u8, f64)> = None;
        for &(shuffled, _freq) in &ambiguous_shuffled {
            if ctx.is_mapped(shuffled) { continue; }
            let score = validate_aux_for_opcode(chunk, shuffled, std_op);
            if score >= 0.65 { // Slightly higher threshold for ambiguous bytes
                if best_match.is_none() || score > best_match.unwrap().1 {
                    best_match = Some((shuffled, score));
                }
            }
        }
        if let Some((shuffled, _score)) = best_match {
            ctx.try_assign(shuffled, std_op);
        }
    }
}

/// Format-validated matching for non-AUX opcodes.
/// Instead of blind frequency-rank matching, we check structural properties
/// of each candidate to verify it behaves like the target opcode.
fn detect_format_validated_noaux(
    chunk: &Chunk,
    ctx: &mut DetectCtx,
    aux_behavior: &HashMap<u8, bool>,
    _aux_standard_ops: &std::collections::HashSet<u8>,
) {
    // Collect unmapped non-AUX shuffled bytes
    let unmapped: Vec<u8> = (0..=255u8)
        .filter(|&s| !ctx.is_mapped(s) && ctx.freq[s as usize] > 0)
        .filter(|&s| aux_behavior.get(&s) != Some(&true))
        .collect();

    if unmapped.is_empty() { return; }

    // For each unmapped non-AUX standard opcode, check which unmapped shuffled bytes
    // pass format validation. Only assign if exactly ONE candidate passes with high confidence.

    // MOVE: ABC where C must always be 0, A < maxstack, B < maxstack
    if !ctx.assigned[LuauOpcode::Move as usize] {
        try_format_match(chunk, ctx, &unmapped, LuauOpcode::Move as u8, |proto, insn| {
            let a = insn_a(insn);
            let b = insn_b(insn);
            let c = insn_c(insn);
            c == 0 && a < proto.max_stack_size && b < proto.max_stack_size
        }, 0.90, 5);
    }

    // LOADNIL: AD where D is always 0, A < maxstack
    if !ctx.assigned[LuauOpcode::LoadNil as usize] {
        try_format_match(chunk, ctx, &unmapped, LuauOpcode::LoadNil as u8, |proto, insn| {
            let a = insn_a(insn);
            let d = insn_d(insn);
            d == 0 && a < proto.max_stack_size
        }, 0.90, 3);
    }

    // LOADB: ABC where B is 0 or 1 (boolean), C is small jump offset (usually 0 or 1)
    if !ctx.assigned[LuauOpcode::LoadB as usize] {
        try_format_match(chunk, ctx, &unmapped, LuauOpcode::LoadB as u8, |proto, insn| {
            let a = insn_a(insn);
            let b = insn_b(insn);
            let c = insn_c(insn);
            a < proto.max_stack_size && b <= 1 && c <= 1
        }, 0.90, 3);
    }

    // LOADN: AD where A < maxstack (D is signed integer, any value is fine)
    if !ctx.assigned[LuauOpcode::LoadN as usize] {
        try_format_match(chunk, ctx, &unmapped, LuauOpcode::LoadN as u8, |proto, insn| {
            let a = insn_a(insn);
            a < proto.max_stack_size
        }, 0.95, 5);
    }

    // NOT / MINUS / LENGTH are INTENTIONALLY NOT format-matched here.
    // Rationale: their format (C=0, A<maxstack, B<maxstack) is shared by many
    // opcodes (GetUpval, SetUpval, Move, DupTable, etc.), and in scripts where
    // the real byte has only 1-2 instances, the format-match fallback picks
    // the wrong byte from the pool of unmapped candidates. This is exactly how
    // 0xF6 was wrongly tagged as MINUS and 0x1C as NOT on ModuleScript.luac.
    // `detect_unary_not_minus` and `detect_unary_ops` apply context validation
    // (numeric consumers) and are authoritative — if they decline, we leave
    // these unmapped rather than corrupt output.  Rule: UNMAPPED > WRONG.

    // GETUPVAL: AB where C=0, A < maxstack, B < num_upvalues
    if !ctx.assigned[LuauOpcode::GetUpval as usize] {
        try_format_match(chunk, ctx, &unmapped, LuauOpcode::GetUpval as u8, |proto, insn| {
            let a = insn_a(insn);
            let b = insn_b(insn);
            let c = insn_c(insn);
            c == 0 && a < proto.max_stack_size && b < proto.num_upvalues.max(1)
        }, 0.85, 3);
    }

    // SETUPVAL: AB where C=0, A < maxstack, B < num_upvalues
    if !ctx.assigned[LuauOpcode::SetUpval as usize] {
        try_format_match(chunk, ctx, &unmapped, LuauOpcode::SetUpval as u8, |proto, insn| {
            let a = insn_a(insn);
            let b = insn_b(insn);
            let c = insn_c(insn);
            c == 0 && a < proto.max_stack_size && b < proto.num_upvalues.max(1)
        }, 0.85, 3);
    }

    // JUMPIF: AD where A < maxstack, target (i + D) within code bounds
    if !ctx.assigned[LuauOpcode::JumpIf as usize] {
        try_format_match(chunk, ctx, &unmapped, LuauOpcode::JumpIf as u8, |proto, insn| {
            let a = insn_a(insn);
            let d = insn_d(insn) as i32;
            a < proto.max_stack_size && d != 0
        }, 0.85, 3);
    }

    // JUMPIFNOT: AD where A < maxstack, D != 0
    if !ctx.assigned[LuauOpcode::JumpIfNot as usize] {
        try_format_match(chunk, ctx, &unmapped, LuauOpcode::JumpIfNot as u8, |proto, insn| {
            let a = insn_a(insn);
            let d = insn_d(insn) as i32;
            a < proto.max_stack_size && d != 0
        }, 0.85, 3);
    }

    // JUMPBACK: AD where D is negative (jumping backwards)
    if !ctx.assigned[LuauOpcode::JumpBack as usize] {
        try_format_match(chunk, ctx, &unmapped, LuauOpcode::JumpBack as u8, |_proto, insn| {
            let d = insn_d(insn);
            d < 0
        }, 0.85, 3);
    }

    // GETVARARGS: AB where C=0, A < maxstack
    if !ctx.assigned[LuauOpcode::GetVarargs as usize] {
        try_format_match(chunk, ctx, &unmapped, LuauOpcode::GetVarargs as u8, |proto, insn| {
            let a = insn_a(insn);
            let c = insn_c(insn);
            c == 0 && a < proto.max_stack_size
        }, 0.85, 2);
    }

    // CLOSEUPVALS: A only, B=0 C=0
    if !ctx.assigned[LuauOpcode::CloseUpvals as usize] {
        try_format_match(chunk, ctx, &unmapped, LuauOpcode::CloseUpvals as u8, |proto, insn| {
            let a = insn_a(insn);
            let b = insn_b(insn);
            let c = insn_c(insn);
            b == 0 && c == 0 && a < proto.max_stack_size
        }, 0.85, 2);
    }

    // Binary arithmetic (ABC): A < maxstack, B < maxstack, C < maxstack
    let arith_ops = [
        LuauOpcode::Add, LuauOpcode::Sub, LuauOpcode::Mul,
        LuauOpcode::Div, LuauOpcode::Mod, LuauOpcode::Pow,
    ];
    for &arith_op in &arith_ops {
        if ctx.assigned[arith_op as usize] { continue; }
        try_format_match(chunk, ctx, &unmapped, arith_op as u8, |proto, insn| {
            let a = insn_a(insn);
            let b = insn_b(insn);
            let c = insn_c(insn);
            a < proto.max_stack_size && b < proto.max_stack_size && c < proto.max_stack_size
        }, 0.90, 3);
    }

    // Arithmetic-K (ABC): A < maxstack, B < maxstack, C < num_constants
    let arithk_ops = [
        LuauOpcode::AddK, LuauOpcode::SubK, LuauOpcode::MulK,
        LuauOpcode::DivK, LuauOpcode::ModK, LuauOpcode::PowK,
    ];
    for &arithk_op in &arithk_ops {
        if ctx.assigned[arithk_op as usize] { continue; }
        try_format_match(chunk, ctx, &unmapped, arithk_op as u8, |proto, insn| {
            let a = insn_a(insn);
            let b = insn_b(insn);
            let c = insn_c(insn);
            a < proto.max_stack_size && b < proto.max_stack_size
                && (c as usize) < proto.constants.len()
        }, 0.85, 3);
    }

    // CONCAT: ABC where A < maxstack, B < maxstack, C < maxstack, B <= C
    if !ctx.assigned[LuauOpcode::Concat as usize] {
        try_format_match(chunk, ctx, &unmapped, LuauOpcode::Concat as u8, |proto, insn| {
            let a = insn_a(insn);
            let b = insn_b(insn);
            let c = insn_c(insn);
            a < proto.max_stack_size && b < proto.max_stack_size
                && c < proto.max_stack_size && b <= c
        }, 0.85, 3);
    }

    // AND/OR: ABC where A < maxstack, B < maxstack, C < maxstack
    let logic_ops = [LuauOpcode::And, LuauOpcode::Or];
    for &logic_op in &logic_ops {
        if ctx.assigned[logic_op as usize] { continue; }
        try_format_match(chunk, ctx, &unmapped, logic_op as u8, |proto, insn| {
            let a = insn_a(insn);
            let b = insn_b(insn);
            let c = insn_c(insn);
            a < proto.max_stack_size && b < proto.max_stack_size && c < proto.max_stack_size
        }, 0.90, 3);
    }

    // ANDK/ORK: ABC where A < maxstack, B < maxstack, C < num_constants
    let logick_ops = [LuauOpcode::AndK, LuauOpcode::OrK];
    for &logick_op in &logick_ops {
        if ctx.assigned[logick_op as usize] { continue; }
        try_format_match(chunk, ctx, &unmapped, logick_op as u8, |proto, insn| {
            let a = insn_a(insn);
            let b = insn_b(insn);
            let c = insn_c(insn);
            a < proto.max_stack_size && b < proto.max_stack_size
                && (c as usize) < proto.constants.len()
        }, 0.85, 3);
    }

    // GETTABLE/SETTABLE: ABC where all < maxstack
    let table_ops = [LuauOpcode::GetTable, LuauOpcode::SetTable];
    for &table_op in &table_ops {
        if ctx.assigned[table_op as usize] { continue; }
        try_format_match(chunk, ctx, &unmapped, table_op as u8, |proto, insn| {
            let a = insn_a(insn);
            let b = insn_b(insn);
            let c = insn_c(insn);
            a < proto.max_stack_size && b < proto.max_stack_size && c < proto.max_stack_size
        }, 0.90, 3);
    }

    // GETTABLEN/SETTABLEN: ABC where A < maxstack, B < maxstack, C is small index (0-255 but typically small)
    if !ctx.assigned[LuauOpcode::GetTableN as usize] {
        try_format_match(chunk, ctx, &unmapped, LuauOpcode::GetTableN as u8, |proto, insn| {
            let a = insn_a(insn);
            let b = insn_b(insn);
            a < proto.max_stack_size && b < proto.max_stack_size
        }, 0.90, 3);
    }
    if !ctx.assigned[LuauOpcode::SetTableN as usize] {
        try_format_match(chunk, ctx, &unmapped, LuauOpcode::SetTableN as u8, |proto, insn| {
            let a = insn_a(insn);
            let b = insn_b(insn);
            a < proto.max_stack_size && b < proto.max_stack_size
        }, 0.90, 3);
    }

    // CAPTURE: AB where A is capture type (0-2), B < maxstack or upvalue index
    if !ctx.assigned[LuauOpcode::Capture as usize] {
        try_format_match(chunk, ctx, &unmapped, LuauOpcode::Capture as u8, |_proto, insn| {
            let a = insn_a(insn);
            a <= 2  // CaptureVal=0, CaptureRef=1, CaptureUpval=2
        }, 0.90, 5);
    }

    // CALL: ABC where A < maxstack, B is small (0 = vararg, 1-10 typical), C is small (0-10)
    if !ctx.assigned[LuauOpcode::Call as usize] {
        try_format_match(chunk, ctx, &unmapped, LuauOpcode::Call as u8, |proto, insn| {
            let a = insn_a(insn);
            let b = insn_b(insn);
            let c = insn_c(insn);
            a < proto.max_stack_size && b <= 10 && c <= 10
        }, 0.85, 5);
    }

    // RETURN: AB where B is small (0 = vararg, 1-5 typical)
    if !ctx.assigned[LuauOpcode::Return as usize] {
        try_format_match(chunk, ctx, &unmapped, LuauOpcode::Return as u8, |_proto, insn| {
            let b = insn_b(insn);
            b <= 5
        }, 0.85, 5);
    }

    // FASTCALL: A is builtin ID (0-112), C is jump offset
    if !ctx.assigned[LuauOpcode::FastCall as usize] {
        try_format_match(chunk, ctx, &unmapped, LuauOpcode::FastCall as u8, |_proto, insn| {
            let a = insn_a(insn);
            a <= 112
        }, 0.90, 3);
    }

    // FASTCALL1: A is builtin ID (0-112), B < maxstack
    if !ctx.assigned[LuauOpcode::FastCall1 as usize] {
        try_format_match(chunk, ctx, &unmapped, LuauOpcode::FastCall1 as u8, |proto, insn| {
            let a = insn_a(insn);
            let b = insn_b(insn);
            a <= 112 && b < proto.max_stack_size
        }, 0.85, 3);
    }

    // PREPVARARGS: A = num_params, only appears as first instruction of vararg protos
    if !ctx.assigned[LuauOpcode::PrepVarargs as usize] {
        try_format_match(chunk, ctx, &unmapped, LuauOpcode::PrepVarargs as u8, |proto, insn| {
            let a = insn_a(insn);
            a == proto.num_params && proto.is_vararg
        }, 0.85, 1);
    }

    // FORNPREP / FORNLOOP are STRUCTURAL_REQUIRED — detect_numeric_for handles
    // them as an atomic pair. Never format-match them independently: half-pair
    // assignment (FORNPREP without FORNLOOP, or vice versa) pollutes the cache
    // and corrupts loop-reconstruction in the lifter.

    // FORGPREP / FORGPREP_INEXT / FORGPREP_NEXT: REMOVED from elimination_pass.
    //
    // Phase B0.30 fix: these were assigned by a loose `a < maxstack && d > 0`
    // pattern that matches *any* AD-format forward-jump instruction, including
    // JUMP, JUMPIF, JUMPIFNOT, JUMPBACK, NEWCLOSURE, LOADK, etc. In scripts
    // like Animate.lua (96KB, 24 real ipairs loops) the elimination_pass picks
    // a wrong byte (e.g. 0xF6 — a JUMP-shaped byte) for ForGPrepINext and locks
    // it into the per-shuffle cache. Once cached, every subsequent script gets
    // the wrong assignment seeded as prior, blocking the structural pair
    // detector (detect_forgprep_inext_pair) from finding the real byte.
    //
    // The proper detectors are:
    //   - detect_forgprep_variants  (ForGPrep, ForGPrepNext): require a target
    //                                ForGLoop/ForGLoopNext at pc+d+1.
    //   - detect_forgprep_inext_pair (ForGPrepINext, Deprecated61): joint
    //                                pair detection with ≥80% target consistency.
    // Both are far stricter than the elimination_pass loose pattern. Leaving
    // ForGPrep* unmapped is preferable to mis-mapping them to a JUMP byte
    // that then poisons the cache for every other script in the corpus.

    // LOADK: AD format, A < maxstack, D indexes valid constant
    if !ctx.assigned[LuauOpcode::LoadK as usize] {
        try_format_match(chunk, ctx, &unmapped, LuauOpcode::LoadK as u8, |proto, insn| {
            let a = insn_a(insn);
            let d = insn_d(insn);
            a < proto.max_stack_size && d >= 0 && (d as u16 as usize) < proto.constants.len()
        }, 0.90, 5);
    }

    // NEWCLOSURE: AD format, A < maxstack, D is valid child proto index
    if !ctx.assigned[LuauOpcode::NewClosure as usize] {
        try_format_match(chunk, ctx, &unmapped, LuauOpcode::NewClosure as u8, |proto, insn| {
            let a = insn_a(insn);
            let d = insn_d(insn);
            a < proto.max_stack_size && d >= 0 && (d as u16 as usize) < proto.child_protos.len().max(1)
        }, 0.85, 2);
    }

    // DUPCLOSURE: AD format, A < maxstack, D indexes a Closure constant
    if !ctx.assigned[LuauOpcode::DupClosure as usize] {
        try_format_match(chunk, ctx, &unmapped, LuauOpcode::DupClosure as u8, |proto, insn| {
            let a = insn_a(insn);
            let d = insn_d(insn);
            a < proto.max_stack_size && d >= 0
                && matches!(proto.constants.get(d as u16 as usize), Some(Constant::Closure(_)))
        }, 0.85, 2);
    }

    // DUPTABLE: AD format, A < maxstack, D indexes a Table constant
    if !ctx.assigned[LuauOpcode::DupTable as usize] {
        try_format_match(chunk, ctx, &unmapped, LuauOpcode::DupTable as u8, |proto, insn| {
            let a = insn_a(insn);
            let d = insn_d(insn);
            a < proto.max_stack_size && d >= 0
                && matches!(proto.constants.get(d as u16 as usize), Some(Constant::Table(_)))
        }, 0.85, 2);
    }

    // SUBRK: ABC where A < maxstack, B < num_constants (B is constant, left operand), C < maxstack
    if !ctx.assigned[LuauOpcode::SubRK as usize] {
        try_format_match(chunk, ctx, &unmapped, LuauOpcode::SubRK as u8, |proto, insn| {
            let a = insn_a(insn);
            let b = insn_b(insn);
            let c = insn_c(insn);
            a < proto.max_stack_size && (b as usize) < proto.constants.len()
                && c < proto.max_stack_size
                && matches!(proto.constants.get(b as usize), Some(Constant::Number(_)))
        }, 0.85, 2);
    }

    // DIVRK: same format as SUBRK
    if !ctx.assigned[LuauOpcode::DivRK as usize] {
        try_format_match(chunk, ctx, &unmapped, LuauOpcode::DivRK as u8, |proto, insn| {
            let a = insn_a(insn);
            let b = insn_b(insn);
            let c = insn_c(insn);
            a < proto.max_stack_size && (b as usize) < proto.constants.len()
                && c < proto.max_stack_size
                && matches!(proto.constants.get(b as usize), Some(Constant::Number(_)))
        }, 0.85, 2);
    }

    // IDIV: ABC where all < maxstack (integer division)
    if !ctx.assigned[LuauOpcode::IDiv as usize] {
        try_format_match(chunk, ctx, &unmapped, LuauOpcode::IDiv as u8, |proto, insn| {
            let a = insn_a(insn);
            let b = insn_b(insn);
            let c = insn_c(insn);
            a < proto.max_stack_size && b < proto.max_stack_size && c < proto.max_stack_size
        }, 0.90, 2);
    }

    // IDIVK: ABC where A,B < maxstack, C < num_constants
    if !ctx.assigned[LuauOpcode::IDivK as usize] {
        try_format_match(chunk, ctx, &unmapped, LuauOpcode::IDivK as u8, |proto, insn| {
            let a = insn_a(insn);
            let b = insn_b(insn);
            let c = insn_c(insn);
            a < proto.max_stack_size && b < proto.max_stack_size
                && (c as usize) < proto.constants.len()
        }, 0.85, 2);
    }

    // JUMPX: E-format (24-bit signed offset) - very rare
    if !ctx.assigned[LuauOpcode::JumpX as usize] {
        // JUMPX uses the full 24-bit E field as a signed offset.
        // Only appears when jump distance exceeds JUMP's 16-bit D range.
        // Very low frequency: typically 0-5 occurrences per chunk.
        let mut best_jx: Option<(u8, u32)> = None;
        for &s in &unmapped {
            if ctx.is_mapped(s) { continue; }
            let freq = ctx.freq[s as usize];
            if freq == 0 || freq > 10 { continue; }
            let mut valid = 0u32;
            let mut total = 0u32;
            for proto in &chunk.protos {
                for (i, &insn) in proto.code.iter().enumerate() {
                    if insn_op(insn) != s { continue; }
                    total += 1;
                    let e = insn_e(insn);
                    let target = i as i32 + e;
                    if target >= 0 && (target as usize) < proto.code.len()
                        && e.abs() > 127
                    {
                        valid += 1;
                    }
                }
            }
            if total >= 1 && valid == total {
                if best_jx.is_none() || valid > best_jx.unwrap().1 {
                    best_jx = Some((s, valid));
                }
            }
        }
        if let Some((op, _)) = best_jx {
            ctx.try_assign(op, LuauOpcode::JumpX as u8);
        }
    }
}

/// Try to match a single standard opcode to an unmapped shuffled byte using format validation.
/// Only assigns if exactly ONE candidate passes the validation threshold, preventing ambiguity.
fn try_format_match<F>(
    chunk: &Chunk,
    ctx: &mut DetectCtx,
    unmapped: &[u8],
    standard: u8,
    validator: F,
    threshold: f64,
    min_instances: u32,
) where F: Fn(&Proto, u32) -> bool {
    if ctx.assigned[standard as usize] { return; }

    let mut candidates: Vec<(u8, f64, u32)> = Vec::new(); // (shuffled, score, count)

    for &s in unmapped {
        if ctx.is_mapped(s) { continue; }
        if ctx.freq[s as usize] < min_instances { continue; }

        let mut valid = 0u32;
        let mut total = 0u32;
        for proto in &chunk.protos {
            for &insn in &proto.code {
                if insn_op(insn) != s { continue; }
                total += 1;
                if validator(proto, insn) { valid += 1; }
            }
        }
        if total >= min_instances {
            let score = valid as f64 / total as f64;
            if score >= threshold {
                candidates.push((s, score, total));
            }
        }
    }

    // Only assign if we have a unique best candidate with a clear margin
    if candidates.len() == 1 {
        ctx.try_assign(candidates[0].0, standard);
    } else if candidates.len() > 1 {
        // Sort by score descending (integer-scaled to avoid NaN), then count descending, then byte ascending.
        candidates.sort_by(|a, b| {
            let sa = (a.1 * 1000.0) as i64;
            let sb = (b.1 * 1000.0) as i64;
            sb.cmp(&sa)
                .then(b.2.cmp(&a.2))
                .then(a.0.cmp(&b.0))
        });
        // Only assign if the best is clearly better than second-best (>5% margin)
        if candidates[0].1 - candidates[1].1 > 0.05 {
            ctx.try_assign(candidates[0].0, standard);
        }
    }
}

/// SUBRK/DIVRK: reversed operand arithmetic (constant on left). ABC format where B is constant index.
fn detect_subrk_divrk(chunk: &Chunk, ctx: &mut DetectCtx) {
    // SubRK: A = B_const - C_reg, DivRK: A = B_const / C_reg
    // Key difference from SubK/DivK: in RK variants, B is the constant (left operand)
    // and C is the register (right operand). In K variants, B is register, C is constant.
    //
    // Require all 6 standard arithmetic ops to be mapped first. This guarantees we can
    // skip their AUX words (none — arith is single-word) during the instruction-position
    // walk, and it prevents detect_subrk_divrk from stealing arith bytes.
    let all_arith_mapped = [
        LuauOpcode::Add, LuauOpcode::Sub, LuauOpcode::Mul,
        LuauOpcode::Div, LuauOpcode::Mod, LuauOpcode::Pow
    ].iter().all(|op| ctx.find_shuffled(*op as u8).is_some());
    if !all_arith_mapped { return; }

    // Walk instruction positions (not raw words) to avoid counting AUX data as
    // candidate instructions. Track:
    //   - rk_hits: number of instruction-position hits with a valid SUBRK shape
    //   - pos_hits: number of instruction-position hits at all (for purity ratio)
    //   - per_proto: for each proto, an ORDERED list of (pc, candidate_byte)
    //     tuples — used for co-occurrence gating AND pair ordering by position.
    // A byte that's REALLY SUBRK/DIVRK should have purity ≈ 1.0 (all its
    // instruction-position hits match the RK shape). AUX-pollution bytes or
    // other opcodes with coincidentally-similar shapes have lower purity.
    let mut rk_hits: HashMap<u8, usize> = HashMap::new();
    let mut pos_hits: HashMap<u8, usize> = HashMap::new();
    // per_proto_rk: for each proto, ordered list of (first_pc, candidate_byte)
    // We record only the FIRST occurrence of each candidate within a proto
    // so we can use proto-position to deterministically order pair assignment
    // (earlier pc -> SubRK, later pc -> DivRK, matching the reverse-k-arith
    // compiler pattern: `K1 - x` emits SubRK before `K2 / x` emits DivRK).
    let mut per_proto_rk: Vec<Vec<(usize, u8)>> = vec![Vec::new(); chunk.protos.len()];
    for (pi, proto) in chunk.protos.iter().enumerate() {
        let code = &proto.code;
        let mut i = 0;
        while i < code.len() {
            let insn = code[i];
            let op = insn_op(insn);
            let mapped = ctx.map[op as usize];
            if mapped != 255 {
                // Known opcode — skip its AUX if needed
                let standard_op = LuauOpcode::from_u8(mapped);
                if standard_op.has_aux() && i + 1 < code.len() {
                    i += 2;
                } else {
                    i += 1;
                }
                continue;
            }
            // Unmapped byte at a true instruction position
            *pos_hits.entry(op).or_insert(0) += 1;
            let a = insn_a(insn) as usize;
            let b = insn_b(insn) as usize; // constant index for RK variant
            let c = insn_c(insn) as usize; // register (right operand)
            if a < proto.max_stack_size as usize
                && b < proto.constants.len()
                && c < proto.max_stack_size as usize
            {
                if matches!(proto.constants.get(b), Some(Constant::Number(_))) {
                    *rk_hits.entry(op).or_insert(0) += 1;
                    // Record first-occurrence pc only (deduplicated).
                    if !per_proto_rk[pi].iter().any(|&(_, b)| b == op) {
                        per_proto_rk[pi].push((i, op));
                    }
                }
            }
            i += 1;
        }
    }

    // SubRK/DivRK are semantically rare — cap at 0.5% of total instructions
    // (was 2%). Tighter cap filters AUX-pollution look-alikes that happen to
    // have valid RK shape. Always allow at least 5 for small files.
    let max_rk_freq = if ctx.total_insns > 200 {
        ((ctx.total_insns / 200) as usize).max(5)
    } else {
        usize::MAX
    };

    // Score candidates: purity is the primary signal, aux-pollution penalty
    // secondary, then lower hit count preferred (SUBRK/DIVRK are rare).
    //
    // Purity = rk_hits / pos_hits. A byte that appears at instruction positions
    // ONLY with valid RK shapes has purity 1.0. Require ≥ 95% purity.
    //
    // AUX-pollution indicator = raw_freq - pos_hits. Bytes that appear many
    // times as AUX data of unmapped opcodes (raw_freq high, pos_hits low)
    // get penalized because they're probably AUX data, not real opcodes.
    let scored: Vec<(u8, usize, usize, i32)> = rk_hits.iter()
        .filter_map(|(&op, &hits)| {
            if ctx.is_mapped(op) || hits > max_rk_freq { return None; }
            let total_pos = *pos_hits.get(&op).unwrap_or(&hits);
            let purity = if total_pos == 0 { 0 } else { hits * 100 / total_pos };
            if purity < 95 { return None; }
            let raw_freq = ctx.freq[op as usize] as i32;
            let aux_pollution = (raw_freq - total_pos as i32).max(0);
            Some((op, purity, hits, aux_pollution))
        })
        .collect();

    // Co-occurrence gate: prefer candidate pairs (a, b) that appear TOGETHER
    // in the same proto. Real reverse-k-arith sequences like `(100 - x) + (1000 / x)`
    // emit SubRK + DivRK as siblings in the same proto; isolated look-alikes
    // (like a 1-hit AndK in a completely different proto) don't pair up.
    //
    // STRICT reverse_k_arith signature (Phase 12 tightening — 2026-04-11):
    //   1. Proto has EXACTLY 2 co-occurring rk-shape candidates (not more, not less).
    //   2. Proto is small (≤ 32 instructions) — reverse_k_arith is a tiny function.
    //   3. The pair sits at pc 0 and pc 1 — at the very start of the proto.
    //   4. The next instruction (pc 2) is ADD — the reverse_k_arith pattern
    //      combines the SubRK and DivRK results with an Add.
    //
    // Why this strictness: on batch/game bytecode the simpler "exactly 2 in
    // a proto" gate fires on protos that happen to have 2 unmapped bytes mid-code
    // that coincidentally pass the RK shape check. This pollutes ctx.map and
    // cascades wrong assignments through the known_shuffles augmenter. The
    // strict signature matches the exact compiler pattern for `(K1 - x) + (K2 / x)`
    // at a function's top — no false positives observed on batch input.
    //
    // If multiple protos match, pick by proto index asc (deterministic). Within
    // a pair, order by FIRST-OCCURRENCE PC: the candidate at pc 0 is SubRK,
    // the candidate at pc 1 is DivRK. This matches the compiler emission order
    // for `(K1 - x) + (K2 / x)`.
    let add_byte = ctx.find_shuffled(LuauOpcode::Add as u8);
    let is_candidate = |op: u8| scored.iter().any(|s| s.0 == op);
    let mut best_pair: Option<(u8, u8)> = None; // (subrk_byte, divrk_byte) ordered by pc
    for (pi, proto_list) in per_proto_rk.iter().enumerate() {
        let proto_candidates: Vec<(usize, u8)> = proto_list.iter()
            .copied()
            .filter(|&(_, op)| is_candidate(op))
            .collect();
        // Require exactly 2 — not more (noise), not less (no pair).
        if proto_candidates.len() != 2 { continue; }
        let (pc_first, byte_first) = proto_candidates[0];
        let (pc_second, byte_second) = proto_candidates[1];
        debug_assert!(pc_first < pc_second);
        // STRICT: pair must be at proto start (pc 0, pc 1).
        if pc_first != 0 || pc_second != 1 { continue; }
        // STRICT: small proto only — reverse_k_arith is tiny.
        let proto = &chunk.protos[pi];
        if proto.code.len() > 32 { continue; }
        if proto.code.len() < 3 { continue; }
        // STRICT: next instruction must be ADD (combines SubRK+DivRK results).
        let add = match add_byte {
            Some(a) => a,
            None => continue,
        };
        if insn_op(proto.code[2]) != add { continue; }
        // First matching proto wins (deterministic by proto index asc).
        if best_pair.is_none() {
            best_pair = Some((byte_first, byte_second));
        }
    }

    if let Some((op_a, op_b)) = best_pair {
        // Co-occurring pair found. Assign by first-occurrence pc order:
        // op_a is at the earlier pc (SubRK), op_b at the later pc (DivRK).
        // This matches the compiler emission pattern for `(K1 - x) + (K2 / x)`
        // where the SubRK instruction is emitted before the DivRK instruction.
        // Verified on ground_truth_module.lua's reverse_k_arith function
        // (proto 1 pc 0 → 0xF5 SubRK, pc 1 → 0xD8 DivRK on ModuleScript.luac).
        ctx.try_assign(op_a, LuauOpcode::SubRK as u8);
        ctx.try_assign(op_b, LuauOpcode::DivRK as u8);
        return;
    }

    // No co-occurring pair. Be conservative: don't assign.
    // Rationale: with only 1 candidate visible or multiple isolated candidates,
    // we can't reliably distinguish SubRK from DivRK or from look-alikes. Better
    // to leave both unmapped per the UNMAPPED > WRONG rule. Cache accumulation
    // from other files will eventually provide the mapping.
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal single-proto Chunk from a slice of instruction words.
    fn chunk_from_code(code: Vec<u32>, max_stack: u8) -> Chunk {
        Chunk {
            version: 6,
            types_version: 0,
            strings: Vec::new(),
            protos: vec![Proto {
                max_stack_size: max_stack,
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
            }],
            main_proto: 0,
        }
    }

    /// Pack an AD-format instruction. `d` is a signed i16 jump offset.
    fn insn_ad(op: u8, a: u8, d: i16) -> u32 {
        let du = d as u16 as u32;
        (op as u32) | ((a as u32) << 8) | (du << 16)
    }

    /// Pack an ABC-format instruction.
    fn insn_abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
        (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
    }

    /// Build a generic-for loop bytecode using arbitrary shuffled opcode bytes.
    /// Layout (pc=0 is FORGPREP, loops to FORGLOOP at pc=4):
    ///
    ///   0: FORGPREP A=0 D=+3       ; jump to FORGLOOP at 0+3+1 = 4
    ///   1: GETTABLEKS body         ; loop body filler
    ///   2: (AUX for GETTABLEKS)
    ///   3: MOVE body               ; more filler
    ///   4: FORGLOOP  A=0 D=-4      ; jump back to pc=1 (body start) = 4+(-4)+1 = 1
    ///   5: AUX: count=2, bit31=0   ; generic-for with 2 vars (pairs)
    ///   6: RETURN
    fn build_generic_for_proto(forgprep_byte: u8, forgloop_byte: u8) -> Chunk {
        let code = vec![
            insn_ad(forgprep_byte, 0, 3),              // 0: FORGPREP
            insn_abc(0xAA, 1, 0, 0),                   // 1: body insn
            0x00000000,                                 // 2: fake AUX word
            insn_abc(0xBB, 1, 1, 0),                   // 3: body insn
            insn_ad(forgloop_byte, 0, -4),             // 4: FORGLOOP
            0x00000002,                                 // 5: AUX: count=2, pairs
            insn_abc(0xCC, 0, 0, 0),                   // 6: RETURN-like
        ];
        chunk_from_code(code, 4)
    }

    #[test]
    fn detect_generic_for_finds_shuffled_pair() {
        // Use two arbitrary shuffled bytes that don't collide with anything else.
        let forgprep_shuffled: u8 = 0xA1;
        let forgloop_shuffled: u8 = 0xB2;
        let chunk = build_generic_for_proto(forgprep_shuffled, forgloop_shuffled);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_generic_for(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[forgprep_shuffled as usize],
            LuauOpcode::ForGPrep as u8,
            "detect_generic_for failed to map FORGPREP byte 0x{:02X}", forgprep_shuffled
        );
        assert_eq!(
            ctx.map[forgloop_shuffled as usize],
            LuauOpcode::ForGLoop as u8,
            "detect_generic_for failed to map FORGLOOP byte 0x{:02X}", forgloop_shuffled
        );
    }

    #[test]
    fn detect_forgprep_variants_finds_prep_byte() {
        let forgprep_shuffled: u8 = 0xA1;
        let forgloop_shuffled: u8 = 0xB2;
        let chunk = build_generic_for_proto(forgprep_shuffled, forgloop_shuffled);

        // Pre-seed the context as if FORGLOOP was already detected by another pass.
        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        ctx.map[forgloop_shuffled as usize] = LuauOpcode::ForGLoop as u8;
        ctx.assigned[LuauOpcode::ForGLoop as usize] = true;

        detect_forgprep_variants(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[forgprep_shuffled as usize],
            LuauOpcode::ForGPrep as u8,
            "detect_forgprep_variants failed to map FORGPREP byte given known FORGLOOP"
        );
    }

    /// Same shape as `build_generic_for_proto` but the FORGLOOP's AUX carries the
    /// ipairs fast-path flag (bit 31), i.e. the loop the compiler emits for
    /// `for i, v in ipairs(t)`.
    fn build_ipairs_for_proto(forgprep_byte: u8, forgloop_byte: u8) -> Chunk {
        let code = vec![
            insn_ad(forgprep_byte, 0, 3),
            insn_abc(0xAA, 1, 0, 0),
            0x00000000,
            insn_abc(0xBB, 1, 1, 0),
            insn_ad(forgloop_byte, 0, -4),
            0x8000_0002, // AUX: nresults=2, ipairs flag set
            insn_abc(0xCC, 0, 0, 0),
        ];
        chunk_from_code(code, 4)
    }

    #[test]
    fn detect_forgprep_variants_prefers_inext_when_forgloop_is_ipairs() {
        // A chunk whose only generic-for is an ipairs loop must yield
        // FORGPREP_INEXT, not plain FORGPREP. Assigning FORGPREP here — which the
        // frequency-ordered fallback does — leaves the real FORGPREP byte homeless
        // and was the single most seed-stable confusion in the corpus.
        let prep: u8 = 0xA1;
        let loop_b: u8 = 0xB2;
        let chunk = build_ipairs_for_proto(prep, loop_b);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        ctx.map[loop_b as usize] = LuauOpcode::ForGLoop as u8;
        ctx.assigned[LuauOpcode::ForGLoop as usize] = true;

        detect_forgprep_variants(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[prep as usize],
            LuauOpcode::ForGPrepINext as u8,
            "prep byte reaching an ipairs-flagged FORGLOOP must be FORGPREP_INEXT"
        );
        assert!(
            !ctx.assigned[LuauOpcode::ForGPrep as usize],
            "plain FORGPREP must be left free for its own byte"
        );
    }

    #[test]
    fn detect_forgprep_variants_keeps_plain_prep_when_flag_absent() {
        // The converse: an unflagged FORGLOOP is a `pairs`/generic loop, so its
        // prep is plain FORGPREP and FORGPREP_INEXT must stay unassigned.
        let prep: u8 = 0xA1;
        let loop_b: u8 = 0xB2;
        let chunk = build_generic_for_proto(prep, loop_b);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        ctx.map[loop_b as usize] = LuauOpcode::ForGLoop as u8;
        ctx.assigned[LuauOpcode::ForGLoop as usize] = true;

        detect_forgprep_variants(&chunk, &mut ctx);

        assert_eq!(ctx.map[prep as usize], LuauOpcode::ForGPrep as u8);
        assert!(
            !ctx.assigned[LuauOpcode::ForGPrepINext as usize],
            "FORGPREP_INEXT must not be claimed without the ipairs flag"
        );
    }

    #[test]
    fn detect_forgprep_variants_separates_both_variants_in_one_chunk() {
        // Both loop kinds present: each prep byte must land on its own opcode
        // regardless of which is more frequent. The ipairs byte is deliberately
        // given the HIGHER count so frequency order alone would mislabel it.
        let plain_prep: u8 = 0xA1;
        let inext_prep: u8 = 0xA3;
        let loop_b: u8 = 0xB2;
        let code = vec![
            // pairs loop
            insn_ad(plain_prep, 0, 3),
            insn_abc(0xAA, 1, 0, 0),
            0x00000000,
            insn_abc(0xBB, 1, 1, 0),
            insn_ad(loop_b, 0, -4),
            0x0000_0002,
            // two ipairs loops
            insn_ad(inext_prep, 0, 3),
            insn_abc(0xAA, 1, 0, 0),
            0x00000000,
            insn_abc(0xBB, 1, 1, 0),
            insn_ad(loop_b, 0, -4),
            0x8000_0002,
            insn_ad(inext_prep, 0, 3),
            insn_abc(0xAA, 1, 0, 0),
            0x00000000,
            insn_abc(0xBB, 1, 1, 0),
            insn_ad(loop_b, 0, -4),
            0x8000_0002,
            insn_abc(0xCC, 0, 0, 0),
        ];
        let chunk = chunk_from_code(code, 4);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        ctx.map[loop_b as usize] = LuauOpcode::ForGLoop as u8;
        ctx.assigned[LuauOpcode::ForGLoop as usize] = true;

        detect_forgprep_variants(&chunk, &mut ctx);

        assert_eq!(ctx.map[inext_prep as usize], LuauOpcode::ForGPrepINext as u8);
        assert_eq!(ctx.map[plain_prep as usize], LuauOpcode::ForGPrep as u8);
    }

    #[test]
    fn detect_numeric_for_does_not_steal_forgprep_byte() {
        // Regression test: before the detection-order fix, detect_numeric_for could
        // claim the FORGPREP byte for ForNPrep when its AUX hint failed to exclude
        // a genuine FORGLOOP follow-on. With the tightened AUX check AND the
        // i+d+1 target math, the detector must skip the generic-for pair.
        //
        // We need at least 2 generic-for pairs for prep_cand to reach the count>=2
        // threshold in detect_numeric_for. Build two adjacent loops.
        let forgprep_shuffled: u8 = 0xA1;
        let forgloop_shuffled: u8 = 0xB2;
        let code = vec![
            // Loop 1: pc 0..=4 with AUX at 5
            insn_ad(forgprep_shuffled, 0, 3),   // 0
            insn_abc(0xAA, 1, 0, 0),            // 1
            0x00000000,                          // 2
            insn_abc(0xBB, 1, 1, 0),            // 3
            insn_ad(forgloop_shuffled, 0, -4),  // 4 -> back to 1
            0x00000002,                          // 5: AUX (count=2, pairs)
            // Loop 2: pc 6..=10 with AUX at 11
            insn_ad(forgprep_shuffled, 0, 3),   // 6
            insn_abc(0xAA, 1, 0, 0),            // 7
            0x00000000,                          // 8
            insn_abc(0xBB, 1, 1, 0),            // 9
            insn_ad(forgloop_shuffled, 0, -4),  // 10 -> back to 7
            0x00000002,                          // 11: AUX (count=2, pairs)
            insn_abc(0xCC, 0, 0, 0),            // 12
        ];
        let chunk = chunk_from_code(code, 4);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);

        // Simulate no FORGLOOP detected yet — detect_numeric_for must exclude the
        // pair via the AUX hint.
        detect_numeric_for(&chunk, &mut ctx);

        assert_ne!(
            ctx.map[forgprep_shuffled as usize],
            LuauOpcode::ForNPrep as u8,
            "detect_numeric_for wrongly claimed the FORGPREP byte"
        );
        assert_ne!(
            ctx.map[forgloop_shuffled as usize],
            LuauOpcode::ForNLoop as u8,
            "detect_numeric_for wrongly claimed the FORGLOOP byte"
        );
    }

    #[test]
    fn full_detect_tier_order_resolves_generic_for() {
        // End-to-end check: the Tier-2 reorder (generic_for → forgprep_variants
        // → numeric_for) should leave FORGPREP and FORGLOOP correctly mapped
        // when only a generic-for pair is present in the bytecode.
        let forgprep_shuffled: u8 = 0xA1;
        let forgloop_shuffled: u8 = 0xB2;
        let chunk = build_generic_for_proto(forgprep_shuffled, forgloop_shuffled);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        // Run just the for-loop detection trio in the fixed order.
        detect_generic_for(&chunk, &mut ctx);
        detect_forgprep_variants(&chunk, &mut ctx);
        detect_numeric_for(&chunk, &mut ctx);

        assert_eq!(ctx.map[forgprep_shuffled as usize], LuauOpcode::ForGPrep as u8);
        assert_eq!(ctx.map[forgloop_shuffled as usize], LuauOpcode::ForGLoop as u8);
        // ForNPrep/ForNLoop must NOT be assigned — there is no numeric-for here.
        assert!(!ctx.assigned[LuauOpcode::ForNPrep as usize]);
        assert!(!ctx.assigned[LuauOpcode::ForNLoop as usize]);
    }

    /// Build a proto with all 6 comparison-jump opcodes, each using a distinct
    /// shuffled byte. All 6 jumps land on a common target PC.
    ///
    /// Layout:
    ///   PC 0,1  : cmp0 A=0 D=+12 | AUX=1    (target = 0+12+1 = 13)
    ///   PC 2,3  : cmp1 A=0 D=+10 | AUX=1    (target = 2+10+1 = 13)
    ///   PC 4,5  : cmp2 A=0 D=+8  | AUX=1
    ///   PC 6,7  : cmp3 A=0 D=+6  | AUX=1
    ///   PC 8,9  : cmp4 A=0 D=+4  | AUX=1
    ///   PC 10,11: cmp5 A=0 D=+2  | AUX=1
    ///   PC 12   : filler
    ///   PC 13   : return-ish
    fn build_comparison_jump_proto(cmp_bytes: [u8; 6]) -> Chunk {
        let code = vec![
            insn_ad(cmp_bytes[0], 0, 12), 0x00000001,
            insn_ad(cmp_bytes[1], 0, 10), 0x00000001,
            insn_ad(cmp_bytes[2], 0, 8),  0x00000001,
            insn_ad(cmp_bytes[3], 0, 6),  0x00000001,
            insn_ad(cmp_bytes[4], 0, 4),  0x00000001,
            insn_ad(cmp_bytes[5], 0, 2),  0x00000001,
            insn_abc(0xCC, 0, 0, 0), // 12: filler
            insn_abc(0xDD, 0, 0, 0), // 13: target
        ];
        chunk_from_code(code, 8)
    }

    #[test]
    fn detect_comparison_jumps_assigns_all_six_when_signal_is_weak() {
        // Before the fix, `count >= 2` filter dropped rare comparison jumps, so
        // single-use JumpIfLE/JumpIfNotLE would be lost. With `count >= 1` and the
        // strict `aux < max_stack` check, all 6 distinct shuffled bytes must be
        // assigned to the 6 comparison slots.
        let cmp_bytes: [u8; 6] = [0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5];
        let chunk = build_comparison_jump_proto(cmp_bytes);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_comparison_jumps_aux(&chunk, &mut ctx);

        let cmp_ops = [
            LuauOpcode::JumpIfEq, LuauOpcode::JumpIfNotEq,
            LuauOpcode::JumpIfLT, LuauOpcode::JumpIfNotLT,
            LuauOpcode::JumpIfLE, LuauOpcode::JumpIfNotLE,
        ];
        for op in cmp_ops {
            assert!(
                ctx.assigned[op as usize],
                "comparison op {:?} was not assigned — JumpIfLE/JumpIfNotLE regression",
                op
            );
        }

        // Every cmp_byte must map to exactly one comparison op
        let mut assigned_count = 0;
        for &b in &cmp_bytes {
            let std = ctx.map[b as usize];
            if std != 255 {
                assigned_count += 1;
                assert!(
                    cmp_ops.iter().any(|op| *op as u8 == std),
                    "byte 0x{:02X} mapped to non-comparison opcode {}", b, std
                );
            }
        }
        assert_eq!(assigned_count, 6,
            "expected all 6 shuffled bytes mapped, got {}", assigned_count);
    }

    #[test]
    fn detect_comparison_jumps_rejects_non_register_aux() {
        // A fake "comparison jump" whose AUX word looks like a next instruction
        // (high bits set) must NOT be mapped. Old detector only checked
        // `aux & 0xFF < max_stack` which was too loose.
        let fake_byte: u8 = 0xAB;
        let code = vec![
            insn_ad(fake_byte, 0, 2),        // 0: fake comparison jump
            0xDEADBEEF,                       // 1: "AUX" is clearly not a register
            insn_abc(0xCC, 0, 0, 0),          // 2: filler
            insn_abc(0xDD, 0, 0, 0),          // 3: target
        ];
        let chunk = chunk_from_code(code, 8);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_comparison_jumps_aux(&chunk, &mut ctx);

        assert_eq!(ctx.map[fake_byte as usize], 255,
            "detector wrongly mapped byte 0x{:02X} with non-register AUX", fake_byte);
    }

    #[test]
    fn detect_jumpback_assigns_single_backward_jump() {
        // Regression test: before the fix, `count >= 3` dropped tiny protos with
        // only a single backward jump (typical of a `while true do ... end` loop
        // at the end of a short function). A lone JUMPBACK with a valid target
        // must now be mapped.
        let jb_byte: u8 = 0x7E;
        let code = vec![
            insn_abc(0xAA, 0, 0, 0),         // 0: body
            insn_abc(0xBB, 0, 0, 0),         // 1: body
            insn_abc(0xCC, 0, 0, 0),         // 2: body
            insn_ad(jb_byte, 0, -4),          // 3: JUMPBACK target = 3+(-4)+1 = 0
            insn_abc(0xDD, 0, 0, 0),         // 4: after loop (unreachable-ish)
        ];
        let chunk = chunk_from_code(code, 4);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_jumpback(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[jb_byte as usize],
            LuauOpcode::JumpBack as u8,
            "detect_jumpback failed to map single backward jump 0x{:02X}", jb_byte
        );
    }

    #[test]
    fn detect_jumpback_rejects_forward_and_out_of_bounds() {
        // A forward jump (D > 0) or a backward jump whose target lands before PC 0
        // must NOT be mapped as JUMPBACK.
        let fwd: u8 = 0x70;
        let oob: u8 = 0x71;
        let code = vec![
            insn_ad(fwd, 0, 3),    // 0: forward (not a backward jump) target=4
            insn_abc(0xAA, 0, 0, 0),
            insn_abc(0xBB, 0, 0, 0),
            insn_abc(0xCC, 0, 0, 0),
            insn_ad(oob, 0, -20),  // 4: would go to pc=-15 → out of bounds
            insn_abc(0xDD, 0, 0, 0),
        ];
        let chunk = chunk_from_code(code, 4);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_jumpback(&chunk, &mut ctx);

        assert_eq!(ctx.map[fwd as usize], 255,
            "forward jump 0x{:02X} wrongly mapped as JUMPBACK", fwd);
        assert_eq!(ctx.map[oob as usize], 255,
            "out-of-bounds jump 0x{:02X} wrongly mapped as JUMPBACK", oob);
    }

    #[test]
    fn detect_fastcall2k_assigns_single_pair() {
        // Regression test: a single FASTCALL2K → CALL pair with a valid constant
        // in AUX must map even when only one occurrence exists. Old threshold
        // (`count >= 2`) lost tiny protos like `math.max(x, 5)` called exactly
        // once, which is pervasive in guard/clamp code.
        let fc2k_byte: u8 = 0x88;
        let call_byte: u8 = 0x20;
        let code = vec![
            insn_abc(fc2k_byte, 17, 2, 1), // 0: FASTCALL2K builtin=17 arg1=r2 jump=1
            0x00000000,                     // 1: AUX = constant index 0
            insn_abc(call_byte, 2, 3, 2),  // 2: CALL reg=2 (target of FASTCALL2K jump)
            insn_abc(0xDD, 0, 0, 0),       // 3: filler
        ];
        // Build chunk with ONE constant at index 0 (a number) so the AUX passes validation.
        let mut chunk = chunk_from_code(code, 8);
        chunk.protos[0].constants = vec![Constant::Number(5.0)];

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        // Pre-seed CALL so detect_fastcall2k can find it
        ctx.map[call_byte as usize] = LuauOpcode::Call as u8;
        ctx.assigned[LuauOpcode::Call as usize] = true;

        detect_fastcall2k(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[fc2k_byte as usize],
            LuauOpcode::FastCall2K as u8,
            "detect_fastcall2k failed to map single FASTCALL2K→CALL pair"
        );
    }

    #[test]
    fn detect_subrk_divrk_skips_aux_pollution() {
        // Regression test for the 2026-04-11 Phase 8 fix (AUX-pollution skip)
        // combined with the Phase 12 strict reverse_k_arith signature
        // (pair at pc 0/1 + ADD at pc 2).
        //
        // Phase 8: detect_subrk_divrk must not count AUX data bytes as candidate
        // instructions. Walking `for &insn in &proto.code` would treat every
        // 32-bit word as an instruction, so AUX words of GETTABLEKS/NAMECALL
        // (with low bytes matching valid SUBRK shapes) polluted the candidate
        // counts and caused wrong SUBRK/DIVRK assignments.
        //
        // Phase 12: the strict signature requires the real SUBRK/DIVRK pair to
        // sit at pc 0/1 of a small proto followed by ADD at pc 2. This test
        // places them there, then adds a GETTABLEKS after the ADD so its AUX
        // word's low byte (the polluter) would be counted if walking naively.
        let subrk_byte: u8 = 0xF5;
        let divrk_byte: u8 = 0xD8;
        let gettableks_byte: u8 = 0x4D;
        let add_byte: u8 = 0x21;
        let polluter_byte: u8 = 0x11;  // AUX low byte we want to avoid

        // Craft a GETTABLEKS AUX word whose low byte == polluter_byte AND whose
        // a=small, b=small (valid const idx pointing to a Number), c=small.
        let aux_polluter_word: u32 = 0x00000011 | (0x01 << 8) | (0x00 << 16) | (0x00 << 24);

        let code = vec![
            // pc 0: real SUBRK R2 = K[0] - R0 (strict signature position)
            insn_abc(subrk_byte, 2, 0, 0),
            // pc 1: real DIVRK R3 = K[0] / R0 (strict signature position)
            insn_abc(divrk_byte, 3, 0, 0),
            // pc 2: ADD R4 = R2 + R3 (strict signature requirement)
            insn_abc(add_byte, 4, 2, 3),
            // pc 3: GETTABLEKS R1, R0, "key" — its AUX at pc 4 has the polluter byte
            insn_abc(gettableks_byte, 1, 0, 0),
            aux_polluter_word,  // pc 4: AUX word whose low byte is polluter
        ];
        let mut chunk = chunk_from_code(code, 8);
        // Provide a Number constant at index 0 so b=0 passes the "points to Number K" check
        chunk.protos[0].constants = vec![Constant::Number(100.0)];

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        // Pre-seed: GETTABLEKS (to enable AUX skip), ADD, and all 6 arith ops
        // (required by detect_subrk_divrk's gate).
        ctx.map[gettableks_byte as usize] = LuauOpcode::GetTableKS as u8;
        ctx.assigned[LuauOpcode::GetTableKS as usize] = true;
        ctx.map[add_byte as usize] = LuauOpcode::Add as u8;
        ctx.assigned[LuauOpcode::Add as usize] = true;
        ctx.map[0xE0] = LuauOpcode::Sub as u8; ctx.assigned[LuauOpcode::Sub as usize] = true;
        ctx.map[0xE1] = LuauOpcode::Mul as u8; ctx.assigned[LuauOpcode::Mul as usize] = true;
        ctx.map[0xE2] = LuauOpcode::Div as u8; ctx.assigned[LuauOpcode::Div as usize] = true;
        ctx.map[0xE3] = LuauOpcode::Mod as u8; ctx.assigned[LuauOpcode::Mod as usize] = true;
        ctx.map[0xE4] = LuauOpcode::Pow as u8; ctx.assigned[LuauOpcode::Pow as usize] = true;

        detect_subrk_divrk(&chunk, &mut ctx);

        // The polluter byte (0x11) must NOT have been assigned SUBRK or DIVRK —
        // it only appeared inside an AUX word, never at a true instruction position.
        assert_ne!(
            ctx.map[polluter_byte as usize],
            LuauOpcode::SubRK as u8,
            "polluter byte (AUX data) was wrongly assigned to SUBRK"
        );
        assert_ne!(
            ctx.map[polluter_byte as usize],
            LuauOpcode::DivRK as u8,
            "polluter byte (AUX data) was wrongly assigned to DIVRK"
        );
        // pc-order pair assignment: subrk_byte at pc 0 → SubRK, divrk_byte at pc 1 → DivRK.
        assert_eq!(
            ctx.find_shuffled(LuauOpcode::SubRK as u8),
            Some(subrk_byte),
            "SUBRK must be assigned to the byte at pc 0 (strict pc-order)"
        );
        assert_eq!(
            ctx.find_shuffled(LuauOpcode::DivRK as u8),
            Some(divrk_byte),
            "DIVRK must be assigned to the byte at pc 1 (strict pc-order)"
        );
    }

    #[test]
    fn detect_subrk_divrk_purity_gate_and_rarer_first() {
        // Regression test covering the 2026-04-11 Phase 9 purity gate, the
        // Phase 11 co-occurrence / pc-order pair assignment, AND the Phase 12
        // strict reverse_k_arith signature (pc 0/1 + small proto + ADD at pc 2).
        //
        // Phase 9 added a ≥95% purity gate (rk_hits / pos_hits) so that
        // look-alike bytes (whose other instruction-position hits don't match
        // RK shape) get rejected before they can outvote real SUBRK/DIVRK.
        //
        // Phase 11 replaced the rarer-first byte tiebreak with a co-occurrence
        // gate: requires a proto with EXACTLY 2 scored candidates and assigns
        // them by first-occurrence PC order.
        //
        // Phase 12 (batch regression fix) further tightened to require the
        // exact reverse_k_arith signature: pair at pc 0/1, proto ≤ 32 insns,
        // ADD instruction at pc 2. This eliminates false positives on batch
        // input where random unmapped bytes coincidentally pass the weaker
        // co-occurrence gate.
        //
        // Setup (single proto — reverse_k_arith-shaped layout):
        //   pc 0: byte A (rare, 1 rk_hit / 1 pos_hit → 100% purity)  → SubRK
        //   pc 1: byte B (common, 2 rk_hit / 2 pos_hit → 100% purity) → DivRK
        //   pc 2: ADD (pre-mapped — required by strict gate)
        //   pc 3: byte B again (2nd hit after ADD)
        //   pc 4-7: byte C (polluted, 1 rk_hit / 4 pos_hit → 25% purity — FILTERED)
        let rare_pure_byte: u8 = 0x88;
        let common_pure_byte: u8 = 0x99;
        let polluted_byte: u8 = 0x77;
        let add_byte: u8 = 0x21;

        // All candidate instructions sit at true instruction positions (no AUX
        // words between them). `b` is the constant index — index 0 is a Number
        // (→ RK-shape valid), index 1 is a String (→ RK-shape invalid).
        let code = vec![
            // pc 0: byte A — 1 hit, RK-shaped (b=0 → Number). SubRK candidate.
            insn_abc(rare_pure_byte, 1, 0, 0),
            // pc 1: byte B — 1st hit, RK-shaped. DivRK candidate.
            insn_abc(common_pure_byte, 2, 0, 0),
            // pc 2: ADD — Phase 12 strict gate requires this slot to be ADD.
            insn_abc(add_byte, 3, 1, 2),
            // pc 3: byte B — 2nd hit (keeps B's purity at 100%).
            insn_abc(common_pure_byte, 4, 0, 0),
            // pc 4-7: byte C — 4 pos_hits, only pc 4 is RK-shaped.
            insn_abc(polluted_byte, 5, 0, 0), // pc 4 — b=0 → rk_hit
            insn_abc(polluted_byte, 6, 1, 0), // pc 5 — b=1 → pos only
            insn_abc(polluted_byte, 7, 1, 0), // pc 6 — b=1 → pos only
            insn_abc(polluted_byte, 1, 1, 0), // pc 7 — b=1 → pos only
        ];
        let mut chunk = chunk_from_code(code, 8);
        chunk.protos[0].constants = vec![
            Constant::Number(100.0),                  // idx 0 — RK-shape pass
            Constant::String("not a number".to_string()), // idx 1 — RK-shape fail
        ];

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        // Pre-seed ADD + all six arithmetic ops (detector gates on all six mapped).
        ctx.map[add_byte as usize] = LuauOpcode::Add as u8;
        ctx.assigned[LuauOpcode::Add as usize] = true;
        ctx.map[0xE0] = LuauOpcode::Sub as u8; ctx.assigned[LuauOpcode::Sub as usize] = true;
        ctx.map[0xE1] = LuauOpcode::Mul as u8; ctx.assigned[LuauOpcode::Mul as usize] = true;
        ctx.map[0xE2] = LuauOpcode::Div as u8; ctx.assigned[LuauOpcode::Div as usize] = true;
        ctx.map[0xE3] = LuauOpcode::Mod as u8; ctx.assigned[LuauOpcode::Mod as usize] = true;
        ctx.map[0xE4] = LuauOpcode::Pow as u8; ctx.assigned[LuauOpcode::Pow as usize] = true;

        detect_subrk_divrk(&chunk, &mut ctx);

        // pc-order pair assignment: 0x88 at pc 0 (earlier) wins SubRK.
        assert_eq!(
            ctx.map[rare_pure_byte as usize],
            LuauOpcode::SubRK as u8,
            "byte A (100% purity, pc 0) should be assigned SUBRK via pc-order pair"
        );
        // 0x99 at pc 1 (later) wins DivRK.
        assert_eq!(
            ctx.map[common_pure_byte as usize],
            LuauOpcode::DivRK as u8,
            "byte B (100% purity, pc 1) should be assigned DIVRK via pc-order pair"
        );
        // polluted_byte (25% purity) must be rejected outright.
        assert_eq!(
            ctx.map[polluted_byte as usize],
            255,
            "polluted byte (25% purity) must remain unmapped — purity gate failed"
        );
    }

    #[test]
    fn detect_subrk_divrk_cooccurrence_gate_rejects_noisy_proto() {
        // Regression test for the 2026-04-11 Phase 11 co-occurrence gate.
        //
        // The old Phase 9 detector ranked candidates globally by (purity,
        // aux_pollution, hits asc, byte asc) and took the top 2. This broke
        // on ModuleScript.luac where a noisy proto had 6+ rk-shape-looking
        // unmapped bytes and the global ranking mis-selected a look-alike
        // (0xAD, really SETLIST) as SubRK.
        //
        // Phase 11 requires a proto with EXACTLY 2 scored candidates to win
        // the SUBRK/DIVRK pair. A proto with ≥3 candidates is treated as
        // noise and skipped. If no proto has exactly 2 survivors, nothing
        // is assigned (UNMAPPED > WRONG).
        //
        // Setup (two protos):
        //   Proto 0 — "noise" proto with 4 distinct rk-shape bytes. All pass
        //             the purity gate (100% each) but the exactly-2 gate
        //             rejects the proto, so none are assigned from here.
        //   Proto 1 — "real" proto with exactly 2 rk-shape bytes at pc 0/1
        //             (the reverse_k_arith signature). These survive the
        //             purity gate AND the exactly-2 gate. Earlier pc wins
        //             SubRK.
        //
        // Expected:
        //   Proto 0's 4 noise bytes → all unmapped.
        //   Proto 1's (real_subrk_byte, real_divrk_byte) pair → assigned.
        let noise_a: u8 = 0xA1;
        let noise_b: u8 = 0xA2;
        let noise_c: u8 = 0xA3;
        let noise_d: u8 = 0xA4;
        let real_subrk_byte: u8 = 0xF5;
        let real_divrk_byte: u8 = 0xD8;
        let add_byte: u8 = 0x21;

        // Proto 0: 4 distinct candidates, each 1 hit with b=0 (Number → RK valid).
        let proto0_code = vec![
            insn_abc(noise_a, 1, 0, 0),
            insn_abc(noise_b, 2, 0, 0),
            insn_abc(noise_c, 3, 0, 0),
            insn_abc(noise_d, 4, 0, 0),
            insn_abc(add_byte, 5, 0, 1), // terminator
        ];
        // Proto 1: exactly 2 candidates — the real reverse-k-arith pair.
        let proto1_code = vec![
            insn_abc(real_subrk_byte, 2, 0, 0), // pc 0 — SubRK (earlier)
            insn_abc(real_divrk_byte, 3, 0, 0), // pc 1 — DivRK (later)
            insn_abc(add_byte, 4, 2, 3),        // pc 2 — ADD R4 = R2 + R3
        ];

        let mut chunk = chunk_from_code(proto0_code, 8);
        // Add proto 1 with its own constants (RK idx 0 must resolve to Number).
        chunk.protos.push(Proto {
            max_stack_size: 8,
            num_params: 0,
            num_upvalues: 0,
            is_vararg: false,
            flags: 0,
            typeinfo: None,
            code: proto1_code,
            constants: vec![Constant::Number(100.0)],
            child_protos: Vec::new(),
            line_defined: 0,
            debug_name: None,
            line_info: None,
            debug_info: None,
        });
        // Proto 0 also needs a Number at K[0] for its noise candidates' RK check.
        chunk.protos[0].constants = vec![Constant::Number(50.0)];

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        // Pre-seed ADD + all six arith ops (detector's gate).
        ctx.map[add_byte as usize] = LuauOpcode::Add as u8;
        ctx.assigned[LuauOpcode::Add as usize] = true;
        ctx.map[0xE0] = LuauOpcode::Sub as u8; ctx.assigned[LuauOpcode::Sub as usize] = true;
        ctx.map[0xE1] = LuauOpcode::Mul as u8; ctx.assigned[LuauOpcode::Mul as usize] = true;
        ctx.map[0xE2] = LuauOpcode::Div as u8; ctx.assigned[LuauOpcode::Div as usize] = true;
        ctx.map[0xE3] = LuauOpcode::Mod as u8; ctx.assigned[LuauOpcode::Mod as usize] = true;
        ctx.map[0xE4] = LuauOpcode::Pow as u8; ctx.assigned[LuauOpcode::Pow as usize] = true;

        detect_subrk_divrk(&chunk, &mut ctx);

        // Proto 1's pair wins via co-occurrence gate + pc order.
        assert_eq!(
            ctx.map[real_subrk_byte as usize],
            LuauOpcode::SubRK as u8,
            "real SUBRK byte at proto 1 pc 0 must be assigned SUBRK"
        );
        assert_eq!(
            ctx.map[real_divrk_byte as usize],
            LuauOpcode::DivRK as u8,
            "real DIVRK byte at proto 1 pc 1 must be assigned DIVRK"
        );
        // Proto 0's 4 noise candidates must ALL remain unmapped — the
        // exactly-2 gate rejected that proto.
        for (label, byte) in [("noise_a", noise_a), ("noise_b", noise_b),
                               ("noise_c", noise_c), ("noise_d", noise_d)] {
            assert_eq!(
                ctx.map[byte as usize], 255,
                "{} (0x{:02X}) was wrongly assigned — exactly-2 gate should reject noisy proto",
                label, byte
            );
        }
    }

    /// opcode_can_appear_in_chunk: LoadKX requires > 32768 constants for permutation_complete.
    /// detect_loadkx bypasses this check using D=0 purity instead.
    #[test]
    fn opcode_can_appear_loadkx_requires_large_const_table() {
        let code = vec![insn_abc(0x01, 0, 0, 0)];

        // Small const table — LoadKX excluded from permutation_complete
        let mut chunk_small = chunk_from_code(code.clone(), 4);
        chunk_small.protos[0].constants = vec![
            Constant::Number(1.0), Constant::Number(2.0), Constant::String("hi".to_string()),
        ];
        assert!(
            !DetectCtx::opcode_can_appear_in_chunk(&chunk_small, LuauOpcode::LoadKX as u8),
            "permutation_complete must exclude LoadKX when no proto has > 32768 constants"
        );

        // Large const table — LoadKX allowed
        let mut chunk_large = chunk_from_code(code, 4);
        chunk_large.protos[0].constants = (0..40000).map(|_| Constant::Nil).collect();
        assert!(
            DetectCtx::opcode_can_appear_in_chunk(&chunk_large, LuauOpcode::LoadKX as u8),
            "LoadKX must be allowed when at least one proto has > 32768 constants"
        );

        // Other opcodes always allowed
        assert!(DetectCtx::opcode_can_appear_in_chunk(&chunk_small, LuauOpcode::LoadK as u8));
        assert!(DetectCtx::opcode_can_appear_in_chunk(&chunk_small, LuauOpcode::SubRK as u8));
        assert!(DetectCtx::opcode_can_appear_in_chunk(&chunk_small, LuauOpcode::Add as u8));
    }

    #[test]
    fn detect_jumpxeq_assigns_single_string_hit() {
        // Regression test: a single JumpXEqKS occurrence with a String constant
        // in AUX must map. Old threshold (`hits >= 2`) dropped any proto that
        // used `if x == "flag" then ... end` only once — very common in enum
        // checks.
        let jxs_byte: u8 = 0x55;
        let code = vec![
            insn_ad(jxs_byte, 0, 2),       // 0: JumpXEqKS A=r0 D=+2 target=3
            0x00000000,                     // 1: AUX = constant index 0 (String)
            insn_abc(0xAA, 0, 0, 0),       // 2: filler
            insn_abc(0xBB, 0, 0, 0),       // 3: jump target
        ];
        let mut chunk = chunk_from_code(code, 4);
        // Constant 0 must be a String for JumpXEqKS categorisation
        chunk.protos[0].constants = vec![Constant::String("flag".into())];

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_jumpxeq(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[jxs_byte as usize],
            LuauOpcode::JumpXEqKS as u8,
            "detect_jumpxeq failed to map single JumpXEqKS with string AUX"
        );
    }

    /// Build a "module-style" proto where NEWTABLE creates the M table at the
    /// FIRST instruction position, then 50+ instructions of unrelated work happen,
    /// then 5 SETTABLEKS calls fill R0 (the M table) — this exact shape was the
    /// regression that drove the detect_newtable rewrite.
    fn build_module_proto(newtable_byte: u8, settableks_byte: u8) -> Chunk {
        // Pre-fill 50 filler instructions between the NEWTABLE and the first fill
        // to simulate services-import code.
        let mut code: Vec<u32> = Vec::new();
        // 0: NEWTABLE R0, B=9 (hash hint=256), C=0, AUX at 1=0
        code.push(insn_abc(newtable_byte, 0, 9, 0));
        code.push(0x00000000); // AUX
        // 50 filler instructions targeting R3..R7 (no R0 reads)
        for _ in 0..50 {
            code.push(insn_abc(0xAA, 3, 0, 0));
        }
        // 5 SETTABLEKS R(field), R0, "key" — fills R0 (B=0)
        for _ in 0..5 {
            code.push(insn_abc(settableks_byte, 4, 0, 0));
            code.push(0x00000001); // AUX = constant index 1 (string)
        }
        // RETURN-like
        code.push(insn_abc(0xCC, 0, 0, 0));
        let mut chunk = chunk_from_code(code, 8);
        // Provide a string constant so the SETTABLEKS-as-key field validates
        chunk.protos[0].constants = vec![
            Constant::String("module_constant".into()),
            Constant::String("field".into()),
        ];
        chunk
    }

    #[test]
    fn detect_newtable_finds_module_pattern_with_distant_fills() {
        // Regression test for the GETGLOBAL-stealing-NEWTABLE bug.
        // A NEWTABLE at proto position 0 with fills 50+ instructions later
        // (canonical module-table shape) MUST be detected.
        let newtable_byte: u8 = 0xFF;
        let settableks_byte: u8 = 0x30;
        let chunk = build_module_proto(newtable_byte, settableks_byte);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        // Pre-seed SETTABLEKS so detect_newtable can use it for cross-validation
        ctx.map[settableks_byte as usize] = LuauOpcode::SetTableKS as u8;
        ctx.assigned[LuauOpcode::SetTableKS as usize] = true;

        detect_newtable(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[newtable_byte as usize],
            LuauOpcode::NewTable as u8,
            "detect_newtable failed to find NEWTABLE byte 0x{:02X} with distant fills (50+ insns away)",
            newtable_byte
        );
    }

    #[test]
    fn detect_global_ops_does_not_steal_module_newtable_byte() {
        // Companion regression: even though the NEWTABLE byte's "AUX" (at i+1=0)
        // happens to point to a String constant (K[0]="module_constant"), the
        // detector must REJECT it because the proto contains 5 SETTABLEKS that
        // fill R(A) — a strong NEWTABLE signal.
        let newtable_byte: u8 = 0xFF;
        let settableks_byte: u8 = 0x30;
        let chunk = build_module_proto(newtable_byte, settableks_byte);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        // Pre-seed SETTABLEKS for cross-validation
        ctx.map[settableks_byte as usize] = LuauOpcode::SetTableKS as u8;
        ctx.assigned[LuauOpcode::SetTableKS as usize] = true;

        // Run NEWTABLE first (the new Tier 4 order), then GETGLOBAL
        detect_newtable(&chunk, &mut ctx);
        detect_global_ops(&chunk, &mut ctx);

        assert_ne!(
            ctx.map[newtable_byte as usize],
            LuauOpcode::GetGlobal as u8,
            "detect_global_ops wrongly stole NEWTABLE byte 0x{:02X}",
            newtable_byte
        );
        assert_ne!(
            ctx.map[newtable_byte as usize],
            LuauOpcode::SetGlobal as u8,
            "detect_global_ops wrongly stole NEWTABLE byte 0x{:02X} as SETGLOBAL",
            newtable_byte
        );
    }

    #[test]
    fn detect_newtable_rejects_all_zeros_noise() {
        // Regression: 0x00000000 (literally, the zero word) used to win the
        // detect_newtable scoring contest because it appears thousands of times
        // as both AUX data and instruction filler. Some of those positions
        // happened to coincide with later SETTABLEKS R_x R0 by random alignment
        // (R0 is the most-frequent register). The fix requires AT LEAST ONE
        // candidate instance to have a non-empty hint (B>0 OR aux>0). The
        // all-zeros word has B=0 AND aux=0 always, so it must be rejected.
        let real_newtable: u8 = 0xFF;          // op byte for the "real" NEWTABLE
        let settableks_byte: u8 = 0x30;
        // Construct two protos:
        //  Proto A: real module-table — NEWTABLE R0 with B=9 (hash hint=256), then 5 fills
        //  Proto B: lots of 0x00000000 words followed by SETTABLEKS R_x R0
        //           (This simulates the noise that fooled the prior detector.)
        let mut code_a: Vec<u32> = Vec::new();
        code_a.push(insn_abc(real_newtable, 0, 9, 0)); // 0: NEWTABLE R0 B=9 C=0
        code_a.push(0x00000000);                        // 1: AUX = 0
        // 5 fills of R0
        for _ in 0..5 {
            code_a.push(insn_abc(settableks_byte, 4, 0, 0));
            code_a.push(0x00000001);
        }
        code_a.push(insn_abc(0xCC, 0, 0, 0));
        let mut chunk_a = chunk_from_code(code_a, 8);
        chunk_a.protos[0].constants = vec![
            Constant::String("zero".into()),
            Constant::String("one".into()),
        ];

        // Build a single chunk with both protos by injecting Proto B as a child.
        let mut code_b: Vec<u32> = Vec::new();
        // 8 zero words, each followed by a SETTABLEKS Rx R0 (mimicking noise)
        for _ in 0..8 {
            code_b.push(0x00000000);                          // looks like NEWTABLE R0 B=0 C=0
            code_b.push(insn_abc(settableks_byte, 4, 0, 0));  // SETTABLEKS R4 R0
            code_b.push(0x00000001);                          // SETTABLEKS AUX
        }
        code_b.push(insn_abc(0xCC, 0, 0, 0));
        let proto_b = Proto {
            max_stack_size: 8,
            num_params: 0,
            num_upvalues: 0,
            is_vararg: false,
            flags: 0,
            typeinfo: None,
            code: code_b,
            constants: vec![
                Constant::String("zero".into()),
                Constant::String("one".into()),
            ],
            child_protos: Vec::new(),
            line_defined: 0,
            debug_name: None,
            line_info: None,
            debug_info: None,
        };
        // Make sure proto_b is part of the chunk so detect_newtable scans both
        chunk_a.protos.push(proto_b);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk_a);
        ctx.map[settableks_byte as usize] = LuauOpcode::SetTableKS as u8;
        ctx.assigned[LuauOpcode::SetTableKS as usize] = true;

        detect_newtable(&chunk_a, &mut ctx);

        assert_eq!(
            ctx.map[real_newtable as usize],
            LuauOpcode::NewTable as u8,
            "detect_newtable failed to pick the REAL NEWTABLE byte 0x{:02X} \
             over 0x00000000 noise",
            real_newtable
        );
        assert_ne!(
            ctx.map[0x00],
            LuauOpcode::NewTable as u8,
            "detect_newtable wrongly assigned 0x00 (zero word) as NEWTABLE"
        );
    }

    #[test]
    fn detect_newtable_skips_aux_words_of_mapped_ops() {
        // Set up a proto where an AUX word's low byte happens to look like a
        // valid NEWTABLE candidate (C=0, valid A). The detector must NOT count
        // it because the AUX word is preceded by an already-mapped AUX-using op.
        let mapped_namecall_byte: u8 = 0x14; // pretend this is NAMECALL
        let aux_low_byte: u8 = 0x42;          // AUX word's low byte — should NOT be a candidate
        let real_newtable_byte: u8 = 0xCD;
        let settableks_byte: u8 = 0x30;

        // NAMECALL R0 R1:"foo"
        // AUX = (0x42 with C=0) — this would falsely match NEWTABLE if walked
        // NEWTABLE R0
        // AUX = 0
        // SETTABLEKS R3 R0 "k" + AUX
        // SETTABLEKS R4 R0 "k" + AUX
        // SETTABLEKS R5 R0 "k" + AUX
        let aux_disguised = (aux_low_byte as u32) | (0u32 << 24); // C=0
        let code = vec![
            insn_abc(mapped_namecall_byte, 0, 1, 0),  // 0: NAMECALL
            aux_disguised,                              // 1: AUX (looks like a NEWTABLE candidate but is data)
            insn_abc(real_newtable_byte, 0, 9, 0),     // 2: real NEWTABLE
            0x00000000,                                 // 3: NEWTABLE AUX
            insn_abc(settableks_byte, 3, 0, 0),        // 4: SETTABLEKS R3 R0
            0x00000001,                                 // 5: AUX
            insn_abc(settableks_byte, 4, 0, 0),        // 6: SETTABLEKS R4 R0
            0x00000001,                                 // 7: AUX
            insn_abc(settableks_byte, 5, 0, 0),        // 8: SETTABLEKS R5 R0
            0x00000001,                                 // 9: AUX
            insn_abc(0xCC, 0, 0, 0),                    // 10: RETURN
        ];
        let mut chunk = chunk_from_code(code, 8);
        chunk.protos[0].constants = vec![
            Constant::String("zero".into()),
            Constant::String("one".into()),
        ];

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        // Mark NAMECALL and SETTABLEKS as already mapped
        ctx.map[mapped_namecall_byte as usize] = LuauOpcode::NameCall as u8;
        ctx.assigned[LuauOpcode::NameCall as usize] = true;
        ctx.map[settableks_byte as usize] = LuauOpcode::SetTableKS as u8;
        ctx.assigned[LuauOpcode::SetTableKS as usize] = true;

        detect_newtable(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[real_newtable_byte as usize],
            LuauOpcode::NewTable as u8,
            "detect_newtable failed to find the real NEWTABLE byte 0x{:02X}",
            real_newtable_byte
        );
        assert_ne!(
            ctx.map[aux_low_byte as usize],
            LuauOpcode::NewTable as u8,
            "detect_newtable wrongly assigned AUX low byte 0x{:02X} as NEWTABLE",
            aux_low_byte
        );
    }

    // ── Real-bytecode diagnostic tests ──
    //
    // These tests load captured Roblox v6 bytecode from `target/release/bytecode_dumps/`
    // (populated by the server when LUAU_DUMP_BYTECODE is set). They use the committed
    // opmap_cache.json as the known-good mapping and then probe the remaining
    // UNMAPPED bytes to determine which one(s) are actually NEWTABLE / FORGLOOP.
    //
    // Unit tests on synthetic bytecode proved the detectors work in isolation, yet on
    // all 93 decompiled files NEWTABLE (53) and FORGLOOP (59) never get mapped. We
    // need to know: (a) which real byte IS NEWTABLE/FORGLOOP in the cached shuffle,
    // and (b) why the detector rejects it. (a) is cheap — the test below walks every
    // captured .luac with the cache applied and AUX-aware skipping, then looks for
    // unmapped bytes that match NEWTABLE / FORGLOOP structural shape.

    /// Diagnostic: load inspect/ModuleScript.luac, run `OpcodeMap::detect` with an
    /// empty prior (cache cleared), and print where 0xF5/0xD8/0xAD/0xCA end up.
    /// Use this to trace which pass is assigning the real SUBRK/DIVRK bytes vs
    /// stealing them for LoadKX/etc.
    ///
    /// Invoke: `cargo test --release -p luau-core --lib -- --ignored diag_trace_modulescript_loadkx_path --nocapture`
    #[test]
    #[ignore]
    fn diag_trace_modulescript_loadkx_path() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent().unwrap()
            .parent().unwrap()
            .join("inspect/ModuleScript.luac");
        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(e) => { eprintln!("cannot read {}: {}", path.display(), e); return; }
        };
        let chunk = match crate::parser::parse(&data) {
            Ok(c) => c,
            Err(e) => { eprintln!("parse failed: {:?}", e); return; }
        };

        // Run detection with empty prior
        let result = OpcodeMap::detect_with_prior(&chunk, &[255u8; 256]);
        eprintln!("=== Phase 10 diag: ModuleScript.luac detection ===");
        eprintln!("mapped_count = {}", result.mapped_count);
        eprintln!("0xF5 -> {} ({:?})", result.shuffled_to_standard[0xF5],
            LuauOpcode::from_u8(result.shuffled_to_standard[0xF5]));
        eprintln!("0xD8 -> {} ({:?})", result.shuffled_to_standard[0xD8],
            LuauOpcode::from_u8(result.shuffled_to_standard[0xD8]));
        eprintln!("0xAD -> {} ({:?})", result.shuffled_to_standard[0xAD],
            LuauOpcode::from_u8(result.shuffled_to_standard[0xAD]));
        eprintln!("0xCA -> {} ({:?})", result.shuffled_to_standard[0xCA],
            LuauOpcode::from_u8(result.shuffled_to_standard[0xCA]));

        // Check the heuristic map (before permutation_complete)
        eprintln!("--- heuristic_map (before Tier 9 permutation_complete) ---");
        eprintln!("heuristic 0xF5 -> {}", result.heuristic_map[0xF5]);
        eprintln!("heuristic 0xD8 -> {}", result.heuristic_map[0xD8]);
        eprintln!("heuristic 0xAD -> {}", result.heuristic_map[0xAD]);
        eprintln!("heuristic 0xCA -> {}", result.heuristic_map[0xCA]);

        // Find which byte got LoadKX (if any)
        if let Some(kx_byte) = (0..=255u8).find(|&b| result.shuffled_to_standard[b as usize] == LuauOpcode::LoadKX as u8) {
            eprintln!("LoadKX assigned to byte 0x{:02X}", kx_byte);
        } else {
            eprintln!("LoadKX UNMAPPED (good)");
        }

        // Ground truth: 0xF5 should be SubRK (71), 0xD8 should be DivRK (72)
        eprintln!("--- ground truth compliance ---");
        eprintln!("0xF5 correct (=SubRK 71)? {}", result.shuffled_to_standard[0xF5] == 71);
        eprintln!("0xD8 correct (=DivRK 72)? {}", result.shuffled_to_standard[0xD8] == 72);

        // Simulate detect_subrk_divrk's instruction-position walk to see why
        // 0xF5 and 0xAD get different treatment. Uses the FINAL map so AUX skip
        // is accurate.
        eprintln!("--- detect_subrk_divrk simulated metrics (final map) ---");
        let mut pos_hits_final = [0usize; 256];
        let mut rk_hits_final = [0usize; 256];
        let mut raw_freq_final = [0usize; 256];
        for proto in &chunk.protos {
            let code = &proto.code;
            for &w in code {
                raw_freq_final[(w & 0xFF) as usize] += 1;
            }
            let mut i = 0;
            while i < code.len() {
                let insn = code[i];
                let op = (insn & 0xFF) as u8;
                let mapped = result.shuffled_to_standard[op as usize];
                if mapped != 255 {
                    let standard_op = LuauOpcode::from_u8(mapped);
                    if standard_op.has_aux() && i + 1 < code.len() {
                        i += 2;
                    } else {
                        i += 1;
                    }
                    continue;
                }
                pos_hits_final[op as usize] += 1;
                let a = ((insn >> 8) & 0xFF) as usize;
                let b = ((insn >> 16) & 0xFF) as usize;
                let c = ((insn >> 24) & 0xFF) as usize;
                if a < proto.max_stack_size as usize
                    && b < proto.constants.len()
                    && c < proto.max_stack_size as usize
                {
                    if matches!(proto.constants.get(b), Some(Constant::Number(_))) {
                        rk_hits_final[op as usize] += 1;
                    }
                }
                i += 1;
            }
        }
        for &byte in &[0xF5u8, 0xD8, 0xAD, 0xCA] {
            let ph = pos_hits_final[byte as usize];
            let rh = rk_hits_final[byte as usize];
            let raw = raw_freq_final[byte as usize];
            let purity = if ph == 0 { 0 } else { rh * 100 / ph };
            eprintln!("  0x{:02X}: pos_hits={} rk_hits={} raw_freq={} purity={}%",
                byte, ph, rh, raw, purity);
        }
        // Also: 0xF5 stats looking at ONLY proto 1 (where ground truth says
        // SubRK/DivRK live).
        if chunk.protos.len() > 1 {
            let p = &chunk.protos[1];
            eprintln!("--- proto 1 analysis (ground truth SubRK/DivRK location) ---");
            eprintln!("  proto1 code.len = {} constants.len = {} max_stack={}",
                p.code.len(), p.constants.len(), p.max_stack_size);
            for (pc, &w) in p.code.iter().enumerate().take(10) {
                let op = (w & 0xFF) as u8;
                let a = ((w >> 8) & 0xFF) as usize;
                let b = ((w >> 16) & 0xFF) as usize;
                let c = ((w >> 24) & 0xFF) as usize;
                let mapped = result.shuffled_to_standard[op as usize];
                let k_name = if b < p.constants.len() {
                    match p.constants.get(b) {
                        Some(Constant::Number(n)) => format!("Number({})", n),
                        Some(Constant::String(s)) => format!("String({:?})", s),
                        Some(Constant::Nil) => "Nil".to_string(),
                        Some(Constant::Boolean(x)) => format!("Bool({})", x),
                        Some(_) => "<other>".to_string(),
                        None => "<none>".to_string(),
                    }
                } else { "<oob>".to_string() };
                eprintln!("    pc={} word=0x{:08X} op=0x{:02X}({:?}) a={} b={} c={} K[b]={}",
                    pc, w, op, LuauOpcode::from_u8(mapped), a, b, c, k_name);
            }
        }

        // Find all instances of 0xAD in all protos
        eprintln!("--- 0xAD occurrences ---");
        for (pi, p) in chunk.protos.iter().enumerate() {
            for (pc, &w) in p.code.iter().enumerate() {
                if (w & 0xFF) as u8 == 0xAD {
                    let a = ((w >> 8) & 0xFF) as usize;
                    let b = ((w >> 16) & 0xFF) as usize;
                    let c = ((w >> 24) & 0xFF) as usize;
                    let k_name = if b < p.constants.len() {
                        match p.constants.get(b) {
                            Some(Constant::Number(n)) => format!("Number({})", n),
                            Some(Constant::String(s)) => format!("String({:?})", s),
                            _ => "<other>".to_string(),
                        }
                    } else { "<oob>".to_string() };
                    eprintln!("  proto {} pc={} word=0x{:08X} a={} b={} c={} K[b]={}",
                        pi, pc, w, a, b, c, k_name);
                }
            }
        }

        // Also 0xF5 and 0xD8 occurrences for comparison
        eprintln!("--- 0xF5 occurrences ---");
        for (pi, p) in chunk.protos.iter().enumerate() {
            for (pc, &w) in p.code.iter().enumerate() {
                if (w & 0xFF) as u8 == 0xF5 {
                    let a = ((w >> 8) & 0xFF) as usize;
                    let b = ((w >> 16) & 0xFF) as usize;
                    let c = ((w >> 24) & 0xFF) as usize;
                    eprintln!("  proto {} pc={} word=0x{:08X} a={} b={} c={} (K len={})",
                        pi, pc, w, a, b, c, p.constants.len());
                }
            }
        }
        eprintln!("--- 0xD8 occurrences ---");
        for (pi, p) in chunk.protos.iter().enumerate() {
            for (pc, &w) in p.code.iter().enumerate() {
                if (w & 0xFF) as u8 == 0xD8 {
                    let a = ((w >> 8) & 0xFF) as usize;
                    let b = ((w >> 16) & 0xFF) as usize;
                    let c = ((w >> 24) & 0xFF) as usize;
                    eprintln!("  proto {} pc={} word=0x{:08X} a={} b={} c={} (K len={})",
                        pi, pc, w, a, b, c, p.constants.len());
                }
            }
        }
    }

    /// Phase A.5 diagnostic: why does detect_numeric_for NOT fire on
    /// ModuleScript.luac? We run every detector up to (but not including)
    /// detect_numeric_for, then manually inline the pair-candidate scan and
    /// dump:
    ///   - every surviving (prep_byte, loop_byte) candidate with its count
    ///   - which bytes have already been claimed
    ///   - whether any single-sole-candidate decision would fire
    ///   - the real FORNPREP/FORNLOOP bytes in the bytecode (if we can find them
    ///     via the `back-to-body + same-A-reg` shape)
    #[test]
    #[ignore]
    fn diag_phase_a5_numeric_for_on_modulescript() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent().unwrap()
            .parent().unwrap()
            .join("inspect/ModuleScript.luac");
        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(e) => { eprintln!("cannot read {}: {}", path.display(), e); return; }
        };
        let chunk = match crate::parser::parse(&data) {
            Ok(c) => c,
            Err(e) => { eprintln!("parse failed: {:?}", e); return; }
        };
        eprintln!("=== Phase A.5 diag: detect_numeric_for on ModuleScript.luac ===");
        eprintln!("parse ok. protos={} main={}", chunk.protos.len(), chunk.main_proto);

        // Run Tier 1 + Tier 2 up to (but NOT including) detect_numeric_for.
        // This mirrors the order in `detect_with_prior` so the context state
        // is accurate.
        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_return(&chunk, &mut ctx);
        detect_prepvarargs(&chunk, &mut ctx);
        detect_getimport(&chunk, &mut ctx);
        detect_closure_capture(&chunk, &mut ctx);
        detect_dupclosure(&chunk, &mut ctx);
        detect_duptable(&chunk, &mut ctx);
        detect_generic_for(&chunk, &mut ctx);
        detect_forgprep_variants(&chunk, &mut ctx);

        // Snapshot "already claimed" bytes at this point.
        let claimed_before: Vec<(u8, u8, &'static str)> = (0..=255u8)
            .filter(|&b| ctx.map[b as usize] != 255)
            .map(|b| {
                let std = ctx.map[b as usize];
                (b, std, LuauOpcode::from_u8(std).name())
            })
            .collect();
        eprintln!("bytes claimed before detect_numeric_for: {}", claimed_before.len());
        for (b, s, n) in &claimed_before {
            eprintln!("  0x{:02X} -> {:2} {}", b, s, n);
        }

        // Now inline the pair_cand collection loop from detect_numeric_for.
        let forgloop_shuffled = ctx.find_shuffled(LuauOpcode::ForGLoop as u8);
        let forgprep_shuffled = ctx.find_shuffled(LuauOpcode::ForGPrep as u8);
        eprintln!("forgloop_shuffled = {:?}", forgloop_shuffled.map(|b| format!("0x{:02X}", b)));
        eprintln!("forgprep_shuffled = {:?}", forgprep_shuffled.map(|b| format!("0x{:02X}", b)));

        let mut pair_cand: std::collections::HashMap<(u8, u8), usize> =
            std::collections::HashMap::new();
        let mut rejected_reasons: std::collections::HashMap<&'static str, usize> =
            std::collections::HashMap::new();
        // Also capture a few example rejected candidates (proto, pc) for the
        // most common rejection reason.
        let mut sample_already_mapped: Vec<(usize, usize, u8, &'static str)> = Vec::new();

        for (pi, proto) in chunk.protos.iter().enumerate() {
            for i in 0..proto.code.len() {
                let insn = proto.code[i];
                let op = insn_op(insn);
                if ctx.is_mapped(op) {
                    *rejected_reasons.entry("prep_already_mapped").or_insert(0) += 1;
                    if sample_already_mapped.len() < 20 {
                        let std = ctx.map[op as usize];
                        sample_already_mapped.push((pi, i, op, LuauOpcode::from_u8(std).name()));
                    }
                    continue;
                }
                let d = insn_d(insn) as i32;
                let a = insn_a(insn);
                if d <= 0 {
                    *rejected_reasons.entry("d_not_positive").or_insert(0) += 1;
                    continue;
                }
                if a >= proto.max_stack_size {
                    *rejected_reasons.entry("a_oob_stack").or_insert(0) += 1;
                    continue;
                }
                let target = (i as i32 + d + 1) as usize;
                if target >= proto.code.len() {
                    *rejected_reasons.entry("target_oob").or_insert(0) += 1;
                    continue;
                }
                let ti = proto.code[target];
                let target_op = insn_op(ti);
                let td = insn_d(ti) as i32;
                if insn_a(ti) != a {
                    *rejected_reasons.entry("target_a_mismatch").or_insert(0) += 1;
                    continue;
                }
                if td >= 0 {
                    *rejected_reasons.entry("target_not_back_jump").or_insert(0) += 1;
                    continue;
                }
                let back = (target as i32) + td + 1;
                if (back - (i as i32 + 1)).abs() > 1 {
                    *rejected_reasons.entry("back_edge_wrong_target").or_insert(0) += 1;
                    continue;
                }
                if Some(target_op) == forgloop_shuffled {
                    *rejected_reasons.entry("target_is_forgloop").or_insert(0) += 1;
                    continue;
                }
                if Some(op) == forgprep_shuffled {
                    *rejected_reasons.entry("prep_is_forgprep").or_insert(0) += 1;
                    continue;
                }
                // AUX heuristic only fires when FORGLOOP is unmapped.
                if forgloop_shuffled.is_none() {
                    let has_aux_hint = if target + 1 < proto.code.len() {
                        let maybe_aux = proto.code[target + 1];
                        let count = maybe_aux & 0xFF;
                        let mid = maybe_aux & 0x7FFFFF00;
                        count >= 1 && count <= 15 && mid == 0
                    } else {
                        false
                    };
                    if has_aux_hint {
                        *rejected_reasons.entry("aux_shape_is_forgloop").or_insert(0) += 1;
                        continue;
                    }
                }
                if ctx.is_mapped(target_op) {
                    *rejected_reasons.entry("loop_already_mapped").or_insert(0) += 1;
                    continue;
                }
                let key = (op, target_op);
                *pair_cand.entry(key).or_insert(0) += 1;
            }
        }

        eprintln!("rejection histogram:");
        let mut rv: Vec<_> = rejected_reasons.iter().collect();
        rv.sort_by(|a, b| b.1.cmp(a.1));
        for (reason, count) in rv {
            eprintln!("  {:30} = {}", reason, count);
        }

        eprintln!("sample prep_already_mapped candidates (first 20):");
        for (pi, pc, b, name) in &sample_already_mapped {
            eprintln!("  proto {} pc {} byte 0x{:02X} was already -> {}", pi, pc, b, name);
        }

        eprintln!("pair candidates found: {}", pair_cand.len());
        let mut pair_sorted: Vec<((u8, u8), usize)> = pair_cand.into_iter().collect();
        pair_sorted.sort_by(|a, b| b.1.cmp(&a.1)
            .then_with(|| a.0.0.cmp(&b.0.0))
            .then_with(|| a.0.1.cmp(&b.0.1)));
        for (idx, ((prep, lop), count)) in pair_sorted.iter().enumerate() {
            eprintln!(
                "  [{}] prep=0x{:02X} loop=0x{:02X} count={}",
                idx, prep, lop, count
            );
            if idx >= 20 { break; }
        }
        if let Some(&((prep_op, loop_op), pair_count)) = pair_sorted.first() {
            let multi_hit = pair_count >= 2;
            let single_sole_candidate = pair_count >= 1 && pair_sorted.len() == 1;
            let accept = multi_hit || single_sole_candidate;
            eprintln!(
                "decision: winner=(0x{:02X}, 0x{:02X}) count={} multi_hit={} single_sole={} accept={}",
                prep_op, loop_op, pair_count, multi_hit, single_sole_candidate, accept
            );
        } else {
            eprintln!("decision: NO candidates after structural filters");
        }

        // ==== Second sweep: IGNORE "already mapped" filters to surface stolen FORNPREP ====
        eprintln!();
        eprintln!("=== UNRESTRICTED sweep (ignoring is_mapped filters) ===");
        let mut pair_cand_unrestricted: std::collections::HashMap<(u8, u8), usize> =
            std::collections::HashMap::new();
        for proto in chunk.protos.iter() {
            for i in 0..proto.code.len() {
                let insn = proto.code[i];
                let op = insn_op(insn);
                let d = insn_d(insn) as i32;
                let a = insn_a(insn);
                if d <= 0 { continue; }
                if a >= proto.max_stack_size { continue; }
                let target = (i as i32 + d + 1) as usize;
                if target >= proto.code.len() { continue; }
                let ti = proto.code[target];
                let target_op = insn_op(ti);
                let td = insn_d(ti) as i32;
                if insn_a(ti) != a { continue; }
                if td >= 0 { continue; }
                let back = (target as i32) + td + 1;
                if (back - (i as i32 + 1)).abs() > 1 { continue; }
                // AUX heuristic: same as detect_numeric_for would apply.
                let has_aux_hint = if target + 1 < proto.code.len() {
                    let maybe_aux = proto.code[target + 1];
                    let count = maybe_aux & 0xFF;
                    let mid = maybe_aux & 0x7FFFFF00;
                    count >= 1 && count <= 15 && mid == 0
                } else { false };
                if has_aux_hint { continue; }

                // DO NOT filter on is_mapped — this is the whole point.
                let key = (op, target_op);
                *pair_cand_unrestricted.entry(key).or_insert(0) += 1;
            }
        }
        eprintln!("unrestricted pair candidates: {}", pair_cand_unrestricted.len());
        let mut pu: Vec<_> = pair_cand_unrestricted.into_iter().collect();
        pu.sort_by(|a, b| b.1.cmp(&a.1)
            .then_with(|| a.0.0.cmp(&b.0.0))
            .then_with(|| a.0.1.cmp(&b.0.1)));
        for (idx, ((prep, lop), count)) in pu.iter().enumerate().take(20) {
            let prep_mapped = ctx.map[*prep as usize];
            let loop_mapped = ctx.map[*lop as usize];
            let prep_name = if prep_mapped == 255 { "(unmapped)".to_string() }
                else { format!("->{}", LuauOpcode::from_u8(prep_mapped).name()) };
            let loop_name = if loop_mapped == 255 { "(unmapped)".to_string() }
                else { format!("->{}", LuauOpcode::from_u8(loop_mapped).name()) };
            eprintln!(
                "  [{}] prep=0x{:02X} {}  loop=0x{:02X} {}  count={}",
                idx, prep, prep_name, lop, loop_name, count
            );
        }

        // ==== Aux-filter only sweep: add back the back-edge filter, drop the AUX filter ====
        eprintln!();
        eprintln!("=== BACK-EDGE-ONLY sweep (no AUX filter) ===");
        let mut pc_no_aux: std::collections::HashMap<(u8, u8), usize> =
            std::collections::HashMap::new();
        for proto in chunk.protos.iter() {
            for i in 0..proto.code.len() {
                let insn = proto.code[i];
                let op = insn_op(insn);
                let d = insn_d(insn) as i32;
                let a = insn_a(insn);
                if d <= 0 { continue; }
                if a >= proto.max_stack_size { continue; }
                let target = (i as i32 + d + 1) as usize;
                if target >= proto.code.len() { continue; }
                let ti = proto.code[target];
                let target_op = insn_op(ti);
                let td = insn_d(ti) as i32;
                if insn_a(ti) != a { continue; }
                if td >= 0 { continue; }
                let back = (target as i32) + td + 1;
                if (back - (i as i32 + 1)).abs() > 1 { continue; }
                let key = (op, target_op);
                *pc_no_aux.entry(key).or_insert(0) += 1;
            }
        }
        eprintln!("back-edge-only candidates: {}", pc_no_aux.len());
        let mut pn: Vec<_> = pc_no_aux.into_iter().collect();
        pn.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.0.cmp(&b.0.0)).then_with(|| a.0.1.cmp(&b.0.1)));
        for (idx, ((prep, lop), count)) in pn.iter().enumerate().take(20) {
            let pm = ctx.map[*prep as usize];
            let lm = ctx.map[*lop as usize];
            let pn = if pm == 255 { "(unmapped)".to_string() } else { format!("->{}", LuauOpcode::from_u8(pm).name()) };
            let ln = if lm == 255 { "(unmapped)".to_string() } else { format!("->{}", LuauOpcode::from_u8(lm).name()) };
            eprintln!("  [{}] prep=0x{:02X} {}  loop=0x{:02X} {}  count={}", idx, prep, pn, lop, ln, count);
        }

        // ==== Back-edge-off sweep: drop the ±1 back-edge filter, keep the AUX filter ====
        eprintln!();
        eprintln!("=== AUX-ONLY sweep (no back-edge ±1 filter) ===");
        let mut pc_no_be: std::collections::HashMap<(u8, u8), usize> =
            std::collections::HashMap::new();
        for proto in chunk.protos.iter() {
            for i in 0..proto.code.len() {
                let insn = proto.code[i];
                let op = insn_op(insn);
                let d = insn_d(insn) as i32;
                let a = insn_a(insn);
                if d <= 0 { continue; }
                if a >= proto.max_stack_size { continue; }
                let target = (i as i32 + d + 1) as usize;
                if target >= proto.code.len() { continue; }
                let ti = proto.code[target];
                let target_op = insn_op(ti);
                let td = insn_d(ti) as i32;
                if insn_a(ti) != a { continue; }
                if td >= 0 { continue; }
                let has_aux_hint = if target + 1 < proto.code.len() {
                    let maybe_aux = proto.code[target + 1];
                    let count = maybe_aux & 0xFF;
                    let mid = maybe_aux & 0x7FFFFF00;
                    count >= 1 && count <= 15 && mid == 0
                } else { false };
                if has_aux_hint { continue; }
                let key = (op, target_op);
                *pc_no_be.entry(key).or_insert(0) += 1;
            }
        }
        eprintln!("aux-only candidates: {}", pc_no_be.len());
        let mut pe: Vec<_> = pc_no_be.into_iter().collect();
        pe.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.0.cmp(&b.0.0)).then_with(|| a.0.1.cmp(&b.0.1)));
        for (idx, ((prep, lop), count)) in pe.iter().enumerate().take(20) {
            let pm = ctx.map[*prep as usize];
            let lm = ctx.map[*lop as usize];
            let pn = if pm == 255 { "(unmapped)".to_string() } else { format!("->{}", LuauOpcode::from_u8(pm).name()) };
            let ln = if lm == 255 { "(unmapped)".to_string() } else { format!("->{}", LuauOpcode::from_u8(lm).name()) };
            eprintln!("  [{}] prep=0x{:02X} {}  loop=0x{:02X} {}  count={}", idx, prep, pn, lop, ln, count);
        }

        // ==== Unmapped byte frequency in AD backward-jump slots ====
        // Tally which UNMAPPED bytes appear most often as AD backward-jump
        // instructions anywhere in the chunk. The real FORNLOOP byte, if any,
        // should show up here with high frequency.
        eprintln!();
        eprintln!("=== AD backward-jump byte frequency (unmapped only) ===");
        let mut back_jump_freq: std::collections::HashMap<u8, usize> =
            std::collections::HashMap::new();
        for proto in chunk.protos.iter() {
            for i in 0..proto.code.len() {
                let insn = proto.code[i];
                let op = insn_op(insn);
                let d = insn_d(insn) as i32;
                if d >= 0 { continue; }
                if ctx.map[op as usize] != 255 { continue; }
                *back_jump_freq.entry(op).or_insert(0) += 1;
            }
        }
        let mut bjf: Vec<_> = back_jump_freq.into_iter().collect();
        bjf.sort_by(|a, b| b.1.cmp(&a.1));
        for (byte, count) in bjf.iter().take(15) {
            eprintln!("  0x{:02X}: {} occurrences", byte, count);
        }

        // ==== Strict SAME-A forward→back pair, NO back-edge or aux filter ====
        eprintln!();
        eprintln!("=== Same-A forward→back pairs, both unmapped, no back-edge/aux filters ===");
        let mut same_a_pairs: std::collections::HashMap<(u8, u8), (usize, i64, i64)> =
            std::collections::HashMap::new();
        for proto in chunk.protos.iter() {
            for i in 0..proto.code.len() {
                let insn = proto.code[i];
                let op = insn_op(insn);
                let d = insn_d(insn) as i32;
                let a = insn_a(insn);
                if d <= 0 { continue; }
                if a >= proto.max_stack_size { continue; }
                let target = (i as i32 + d + 1) as usize;
                if target >= proto.code.len() { continue; }
                let ti = proto.code[target];
                let target_op = insn_op(ti);
                let ta = insn_a(ti);
                let td = insn_d(ti) as i32;
                if td >= 0 { continue; }
                if a != ta { continue; }
                if ctx.map[op as usize] != 255 { continue; }
                if ctx.map[target_op as usize] != 255 { continue; }
                let back = (target as i32) + td + 1;
                let delta = (back - (i as i32 + 1)) as i64;
                let entry = same_a_pairs.entry((op, target_op)).or_insert((0, i64::MAX, i64::MIN));
                entry.0 += 1;
                entry.1 = entry.1.min(delta);
                entry.2 = entry.2.max(delta);
            }
        }
        let mut sap: Vec<_> = same_a_pairs.into_iter().collect();
        sap.sort_by(|a, b| b.1.0.cmp(&a.1.0));
        eprintln!("same-A unmapped pair count: {}", sap.len());
        for ((prep, lop), (count, dmin, dmax)) in sap.iter().take(20) {
            eprintln!(
                "  (0x{:02X}, 0x{:02X}) count={} delta_range=[{}, {}]",
                prep, lop, count, dmin, dmax
            );
        }

        // ==== RAW forward→back pair dump: drop all filters except jump direction ====
        // For every forward-jumping AD whose target is a backward-jumping AD, dump
        // the pair with its A match status and back-edge delta. This bypasses the
        // same-A and back-edge-±1 filters so we can see whether real FORNPREP/
        // FORNLOOP pairs are being rejected by A mismatch or body-offset drift.
        eprintln!();
        eprintln!("=== RAW forward→back pair dump (small protos only) ===");
        let mut raw_pairs: std::collections::HashMap<(u8, u8), (usize, usize, i64, i64)> =
            std::collections::HashMap::new();
        // key = (prep_byte, loop_byte), value = (count, a_match_count, min_backedge_delta, max_backedge_delta)
        for (pi, proto) in chunk.protos.iter().enumerate() {
            if proto.code.len() > 30 { continue; } // only small protos
            for i in 0..proto.code.len() {
                let insn = proto.code[i];
                let op = insn_op(insn);
                let d = insn_d(insn) as i32;
                let a = insn_a(insn);
                if d <= 0 { continue; }
                let target = (i as i32 + d + 1) as usize;
                if target >= proto.code.len() { continue; }
                let ti = proto.code[target];
                let target_op = insn_op(ti);
                let ta = insn_a(ti);
                let td = insn_d(ti) as i32;
                if td >= 0 { continue; }
                let back = (target as i32) + td + 1;
                let delta = (back - (i as i32 + 1)) as i64;
                let a_match = a == ta;
                // Only list pairs where BOTH bytes are currently unmapped — candidates
                // for real FORNPREP/FORNLOOP identification.
                if ctx.map[op as usize] != 255 { continue; }
                if ctx.map[target_op as usize] != 255 { continue; }
                let entry = raw_pairs.entry((op, target_op)).or_insert((0, 0, i64::MAX, i64::MIN));
                entry.0 += 1;
                if a_match { entry.1 += 1; }
                entry.2 = entry.2.min(delta);
                entry.3 = entry.3.max(delta);
                if entry.0 <= 2 {
                    eprintln!(
                        "  proto {} pc {}: prep=0x{:02X}(a={}) d={} -> loop@{} = 0x{:02X}(a={}) td={} back={} (i+1={}, delta={})",
                        pi, i, op, a, d, target, target_op, ta, td, back, i+1, delta
                    );
                }
            }
        }
        let mut raw_list: Vec<_> = raw_pairs.into_iter().collect();
        raw_list.sort_by(|a, b| b.1.0.cmp(&a.1.0));
        eprintln!("raw pair summary (small protos, both bytes unmapped):");
        for ((prep, lop), (count, a_match, dmin, dmax)) in raw_list.iter().take(30) {
            eprintln!(
                "  (0x{:02X}, 0x{:02X}) count={} a_match={} delta_range=[{}, {}]",
                prep, lop, count, a_match, dmin, dmax
            );
        }

        // ==== Third sweep: strip the AUX-hint filter AND the back-edge-±1 filter ====
        // Some compilers emit FORNPREP→FORNLOOP with a continue/break restructure
        // that may shift the back-edge target off by more than 1. Show how many
        // candidates survive the loose pass.
        eprintln!();
        eprintln!("=== LOOSE sweep (no aux filter, no back-edge ±1 filter) ===");
        let mut pair_cand_loose: std::collections::HashMap<(u8, u8), usize> =
            std::collections::HashMap::new();
        for proto in chunk.protos.iter() {
            for i in 0..proto.code.len() {
                let insn = proto.code[i];
                let op = insn_op(insn);
                let d = insn_d(insn) as i32;
                let a = insn_a(insn);
                if d <= 0 { continue; }
                if a >= proto.max_stack_size { continue; }
                let target = (i as i32 + d + 1) as usize;
                if target >= proto.code.len() { continue; }
                let ti = proto.code[target];
                let target_op = insn_op(ti);
                let td = insn_d(ti) as i32;
                if insn_a(ti) != a { continue; }
                if td >= 0 { continue; }
                let key = (op, target_op);
                *pair_cand_loose.entry(key).or_insert(0) += 1;
            }
        }
        eprintln!("loose pair candidates: {}", pair_cand_loose.len());
        let mut pl: Vec<_> = pair_cand_loose.into_iter().collect();
        pl.sort_by(|a, b| b.1.cmp(&a.1)
            .then_with(|| a.0.0.cmp(&b.0.0))
            .then_with(|| a.0.1.cmp(&b.0.1)));
        for (idx, ((prep, lop), count)) in pl.iter().enumerate().take(20) {
            let prep_mapped = ctx.map[*prep as usize];
            let loop_mapped = ctx.map[*lop as usize];
            let prep_name = if prep_mapped == 255 { "(unmapped)".to_string() }
                else { format!("->{}", LuauOpcode::from_u8(prep_mapped).name()) };
            let loop_name = if loop_mapped == 255 { "(unmapped)".to_string() }
                else { format!("->{}", LuauOpcode::from_u8(loop_mapped).name()) };
            eprintln!(
                "  [{}] prep=0x{:02X} {}  loop=0x{:02X} {}  count={}",
                idx, prep, prep_name, lop, loop_name, count
            );
        }
    }

    /// Phase A.5 diagnostic: scan every bytecode_dumps file for protos that
    /// contain a canonical FORNPREP/FORNLOOP pair. "Canonical" here means:
    ///   prep.D >= 1 && loop.D < 0 && same A register
    ///   && (loop_target == prep_pc + 1)  // back-edge lands at body start
    ///
    /// This is the strict shape detect_numeric_for assumes. Any file that has
    /// such a pair is a "real-bytecode fixture" we can use to verify Phase A
    /// Patch 1 structurally (not via known_shuffles augmenter unanimity).
    ///
    /// For each hit, print: file, proto index, pc, prep_byte, loop_byte, A, body_len.
    /// Then run the FULL `OpcodeMap::detect_with_prior(chunk, &[255; 256])`
    /// (no cache seed) on that file and report whether FORNPREP and FORNLOOP
    /// ended up mapped. This isolates whether Patch 1 + Patch 2 actually fire
    /// on a real structural match.
    #[test]
    #[ignore]
    fn diag_phase_a5_find_real_numeric_for_dumps() {
        let files = load_dumped_bytecode_files();
        if files.is_empty() {
            eprintln!("no bytecode dumps available");
            return;
        }
        eprintln!("=== Phase A.5: scan {} dump files for canonical FORNPREP/FORNLOOP pairs ===", files.len());

        #[derive(Debug)]
        struct Hit {
            file: String,
            size: usize,
            proto: usize,
            pc: usize,
            prep_byte: u8,
            loop_byte: u8,
            reg_a: u8,
            body_len: i32,
        }
        let mut hits: Vec<Hit> = Vec::new();

        for (fname, data) in &files {
            let chunk = match crate::parser::parse(data) {
                Ok(c) => c,
                Err(_) => continue,
            };
            for (pi, proto) in chunk.protos.iter().enumerate() {
                for i in 0..proto.code.len() {
                    let insn = proto.code[i];
                    let op = insn_op(insn);
                    let d = insn_d(insn) as i32;
                    let a = insn_a(insn);
                    if d < 1 { continue; }
                    if a >= proto.max_stack_size { continue; }
                    let target = (i as i32 + d + 1) as usize;
                    if target >= proto.code.len() { continue; }
                    let ti = proto.code[target];
                    let target_op = insn_op(ti);
                    let td = insn_d(ti) as i32;
                    if td >= 0 { continue; }
                    if insn_a(ti) != a { continue; }
                    // Canonical back-edge: loop jumps to prep_pc + 1 (body start)
                    let back_target = (target as i32 + td + 1) as usize;
                    if back_target != i + 1 { continue; }
                    // Require bytes to be distinct (FORNPREP != FORNLOOP)
                    if op == target_op { continue; }
                    hits.push(Hit {
                        file: fname.clone(),
                        size: data.len(),
                        proto: pi,
                        pc: i,
                        prep_byte: op,
                        loop_byte: target_op,
                        reg_a: a,
                        body_len: d,
                    });
                }
            }
        }

        eprintln!("found {} canonical FORNPREP/FORNLOOP hits across all dumps", hits.len());
        // Count occurrences per (file, prep_byte, loop_byte) tuple
        let mut by_file: HashMap<String, usize> = HashMap::new();
        let mut by_pair: HashMap<(u8, u8), usize> = HashMap::new();
        for h in &hits {
            *by_file.entry(h.file.clone()).or_insert(0) += 1;
            *by_pair.entry((h.prep_byte, h.loop_byte)).or_insert(0) += 1;
        }
        let mut file_rank: Vec<(String, usize)> = by_file.into_iter().collect();
        file_rank.sort_by(|a, b| b.1.cmp(&a.1));
        eprintln!("\ntop files by hit count:");
        for (f, c) in file_rank.iter().take(20) {
            eprintln!("  {} hits: {}", c, f);
        }
        let mut pair_rank: Vec<((u8, u8), usize)> = by_pair.into_iter().collect();
        pair_rank.sort_by(|a, b| b.1.cmp(&a.1));
        eprintln!("\ntop (prep_byte, loop_byte) pairs:");
        for ((p, l), c) in pair_rank.iter().take(20) {
            eprintln!("  (0x{:02X}, 0x{:02X}) count={}", p, l, c);
        }

        // First 20 hits with full details
        eprintln!("\nfirst 20 hit details:");
        for h in hits.iter().take(20) {
            eprintln!(
                "  {} proto={} pc={} prep=0x{:02X} loop=0x{:02X} A={} body_len={}",
                h.file, h.proto, h.pc, h.prep_byte, h.loop_byte, h.reg_a, h.body_len
            );
        }

        // Run full detect_with_prior([255;256]) on EVERY file and report FORNPREP/FORNLOOP status
        eprintln!("\n=== FRESH detect_with_prior([255;256]) on all files ===");
        let prior = [255u8; 256];
        let mut fornprep_hits = 0usize;
        let mut fornloop_hits = 0usize;
        let mut both_hits: Vec<(String, usize, u8, u8)> = Vec::new();
        for (fname, data) in &files {
            let chunk = match crate::parser::parse(data) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let map = OpcodeMap::detect_with_prior(&chunk, &prior);
            let fp_byte = map.heuristic_map.iter().position(|&v| v == LuauOpcode::ForNPrep as u8);
            let fl_byte = map.heuristic_map.iter().position(|&v| v == LuauOpcode::ForNLoop as u8);
            if fp_byte.is_some() { fornprep_hits += 1; }
            if fl_byte.is_some() { fornloop_hits += 1; }
            if let (Some(fp), Some(fl)) = (fp_byte, fl_byte) {
                both_hits.push((fname.clone(), data.len(), fp as u8, fl as u8));
            }
        }
        eprintln!("FORNPREP mapped in {}/{} files, FORNLOOP in {}/{}",
                  fornprep_hits, files.len(), fornloop_hits, files.len());
        both_hits.sort_by_key(|h| h.1);
        eprintln!("{} files have BOTH FORNPREP and FORNLOOP mapped:", both_hits.len());
        for (f, sz, fp, fl) in both_hits.iter().take(20) {
            eprintln!("  {} ({} bytes): FORNPREP=0x{:02X}, FORNLOOP=0x{:02X}", f, sz, fp, fl);
        }

        // Run second pass: DEEP sweep for FORNPREP/FORNLOOP using relaxed back-edge
        // Include pairs where back_target is ANY reasonable position (not just body_start)
        // AND exclude known FORG family bytes. Report pairs that are plausible.
        eprintln!("\n=== RELAXED pair scan (exclude FORG bytes, any back-edge direction) ===");
        let forg_bytes: std::collections::HashSet<u8> = vec![0x17, 0x51, 0x64, 0x6E].into_iter().collect();
        let mut relaxed_pairs: HashMap<(u8, u8), (usize, std::collections::HashSet<String>)> = HashMap::new();
        for (fname, data) in &files {
            let chunk = match crate::parser::parse(data) {
                Ok(c) => c,
                Err(_) => continue,
            };
            for proto in chunk.protos.iter() {
                for i in 0..proto.code.len() {
                    let insn = proto.code[i];
                    let op = insn_op(insn);
                    if forg_bytes.contains(&op) { continue; }
                    let d = insn_d(insn) as i32;
                    let a = insn_a(insn);
                    if d < 1 { continue; }
                    if a >= proto.max_stack_size { continue; }
                    let target = (i as i32 + d + 1) as usize;
                    if target >= proto.code.len() { continue; }
                    let ti = proto.code[target];
                    let target_op = insn_op(ti);
                    if forg_bytes.contains(&target_op) { continue; }
                    let td = insn_d(ti) as i32;
                    if td >= 0 { continue; }
                    if insn_a(ti) != a { continue; }
                    if op == target_op { continue; }
                    // Check back-edge is in-range (lands within the proto)
                    let back_target = (target as i32 + td + 1) as i64;
                    if back_target < 0 || back_target as usize >= proto.code.len() { continue; }
                    // Back-edge should roughly land at body_start (prep_pc+1) ± small skew
                    let delta = back_target - (i as i64 + 1);
                    if delta.abs() > 16 { continue; } // allow small prologue
                    let entry = relaxed_pairs.entry((op, target_op)).or_insert((0, std::collections::HashSet::new()));
                    entry.0 += 1;
                    entry.1.insert(fname.clone());
                }
            }
        }
        let mut rp: Vec<((u8, u8), (usize, std::collections::HashSet<String>))> = relaxed_pairs.into_iter().collect();
        rp.sort_by(|a, b| b.1.0.cmp(&a.1.0));
        eprintln!("relaxed pair count: {}", rp.len());
        for ((p, l), (c, fs)) in rp.iter().take(15) {
            eprintln!("  (0x{:02X}, 0x{:02X}) count={} in {} files", p, l, c, fs.len());
        }
    }

    fn load_cached_opmap() -> Option<[u8; 256]> {
        let cache_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()?
            .parent()?
            .join("target/release/opmap_cache.json");
        let data = std::fs::read_to_string(&cache_path).ok()?;
        // Manual tiny JSON parser for `[[N, N, ..., N]]` form
        // (avoids serde_json dependency in luau-core).
        let mut nums: Vec<u8> = Vec::with_capacity(256);
        let mut cur: Option<u32> = None;
        let mut nested = 0u32;
        for ch in data.chars() {
            match ch {
                '[' => nested += 1,
                ']' => {
                    if let Some(n) = cur.take() { nums.push(n.min(255) as u8); }
                    if nested > 0 { nested -= 1; }
                    if nested == 1 { break; } // first variant done
                }
                ',' => {
                    if let Some(n) = cur.take() { nums.push(n.min(255) as u8); }
                }
                d if d.is_ascii_digit() => {
                    let v = d as u32 - '0' as u32;
                    cur = Some(cur.map(|c| c * 10 + v).unwrap_or(v));
                }
                _ => {}
            }
        }
        if nums.len() != 256 { return None; }
        let mut map = [255u8; 256];
        map.copy_from_slice(&nums);
        Some(map)
    }

    fn load_dumped_bytecode_files() -> Vec<(String, Vec<u8>)> {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent().unwrap()
            .parent().unwrap()
            .join("target/release/bytecode_dumps");
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("luac") { continue; }
                if let Ok(data) = std::fs::read(&path) {
                    let name = path.file_name().unwrap().to_string_lossy().to_string();
                    out.push((name, data));
                }
            }
        }
        out
    }

    /// Diagnostic: with the cached opmap applied, walk every captured .luac file
    /// AUX-aware and find unmapped bytes at true instruction positions. For each,
    /// check NEWTABLE-shape (C=0, B hash-hint, followed by SETTABLEKS with B=A)
    /// and FORGLOOP-shape (AD format backward jump, preceded by a forward jumper).
    /// Report per-byte evidence so we can identify which bytes SHOULD be mapped.
    ///
    /// This test is `#[ignore]` so it doesn't run on normal `cargo test` — invoke
    /// with `cargo test --release -p luau-core --lib -- --ignored diag_find_real_newtable_forgloop_bytes --nocapture`
    #[test]
    #[ignore]
    fn diag_find_real_newtable_forgloop_bytes() {
        let cache = match load_cached_opmap() {
            Some(m) => m,
            None => { eprintln!("no cache available — skipping diagnostic"); return; }
        };
        let files = load_dumped_bytecode_files();
        if files.is_empty() {
            eprintln!("no bytecode dumps available — skipping diagnostic");
            return;
        }
        eprintln!("loaded {} real bytecode files", files.len());
        eprintln!("cache has {} mapped bytes", cache.iter().filter(|&&v| v != 255).count());

        // byte -> (newtable_evidence_score, forgloop_evidence_score, total_appearances)
        let mut per_byte: HashMap<u8, (usize, usize, usize)> = HashMap::new();

        // Pre-compute cached shuffled bytes for ops we need
        let settableks_byte = cache.iter().position(|&v| v == LuauOpcode::SetTableKS as u8).map(|i| i as u8);
        let setlist_byte = cache.iter().position(|&v| v == LuauOpcode::SetList as u8).map(|i| i as u8);
        let settablen_byte = cache.iter().position(|&v| v == LuauOpcode::SetTableN as u8).map(|i| i as u8);

        let mut parsed_count = 0usize;
        for (_fname, data) in &files {
            let chunk = match crate::parser::parse(data) {
                Ok(c) => c,
                Err(_) => continue,
            };
            parsed_count += 1;
            for proto in &chunk.protos {
                let code = &proto.code;
                // AUX-aware walk using the cached opmap to know which ops have AUX
                let mut i = 0usize;
                while i < code.len() {
                    let insn = code[i];
                    let op = insn_op(insn);
                    let mapped = cache[op as usize];
                    if mapped != 255 {
                        let std_op = LuauOpcode::from_u8(mapped);
                        let step = if std_op.has_aux() && i + 1 < code.len() { 2 } else { 1 };
                        i += step;
                        continue;
                    }
                    // Unmapped byte at a true instruction position — probe for shapes
                    let a = insn_a(insn);
                    let _b = insn_b(insn);
                    let c = insn_c(insn);
                    let d = insn_d(insn) as i32;
                    let entry = per_byte.entry(op).or_default();
                    entry.2 += 1;

                    // NEWTABLE shape: C=0, A<stack, has fill with B=A later
                    if c == 0 && a < proto.max_stack_size {
                        let mut fills = 0usize;
                        // Same AUX-aware walk scanning forward for fills
                        let mut j = i + 2;
                        while j < code.len() && fills < 32 {
                            let fop = insn_op(code[j]);
                            let fmapped = cache[fop as usize];
                            if fmapped != 255 {
                                let f_std = LuauOpcode::from_u8(fmapped);
                                if (Some(fop) == settableks_byte
                                    || Some(fop) == setlist_byte
                                    || Some(fop) == settablen_byte)
                                    && insn_b(code[j]) == a
                                {
                                    fills += 1;
                                }
                                if f_std.has_aux() && j + 1 < code.len() { j += 2; } else { j += 1; }
                            } else {
                                j += 1;
                            }
                        }
                        if fills > 0 {
                            // Score: fills count heavily (x5), plus proto-start bonus
                            let score = fills * 5 + if i <= 1 { 100 } else { 0 };
                            entry.0 += score;
                        }
                    }

                    // FORGLOOP shape: AD backward jump (d < 0), AUX has count 1-15 with mid=0,
                    // followed by an instruction outside the loop body
                    if d < 0 && a < proto.max_stack_size && i + 1 < code.len() {
                        let aux = code[i + 1];
                        let count = aux & 0xFF;
                        let mid = aux & 0x7FFFFF00;
                        if count >= 1 && count <= 15 && mid == 0 {
                            // Back-edge target: i + d + 1. Must be a valid, earlier position.
                            let back = i as i32 + d + 1;
                            if back >= 0 && (back as usize) < i {
                                entry.1 += 10;
                            }
                        }
                    }

                    i += 1;
                }
            }
        }

        eprintln!("parsed {} of {} files", parsed_count, files.len());
        // Sort candidates by NEWTABLE score
        let mut nt_ranked: Vec<(u8, usize, usize, usize)> = per_byte.iter()
            .map(|(&b, &(nt, fg, tot))| (b, nt, fg, tot))
            .filter(|(_, nt, _, _)| *nt > 0)
            .collect();
        nt_ranked.sort_by_key(|&(_, nt, _, _)| std::cmp::Reverse(nt));
        eprintln!("\n=== TOP NEWTABLE CANDIDATES (unmapped bytes with SETTABLEKS fills after them) ===");
        for (b, nt, fg, tot) in nt_ranked.iter().take(10) {
            eprintln!("  byte 0x{:02X}: nt_score={}, fg_score={}, total_insn_positions={}", b, nt, fg, tot);
        }

        // Sort candidates by FORGLOOP score
        let mut fg_ranked: Vec<(u8, usize, usize, usize)> = per_byte.iter()
            .map(|(&b, &(nt, fg, tot))| (b, nt, fg, tot))
            .filter(|(_, _, fg, _)| *fg > 0)
            .collect();
        fg_ranked.sort_by_key(|&(_, _, fg, _)| std::cmp::Reverse(fg));
        eprintln!("\n=== TOP FORGLOOP CANDIDATES (unmapped bytes that are AD backward jumps with AUX count hint) ===");
        for (b, nt, fg, tot) in fg_ranked.iter().take(10) {
            eprintln!("  byte 0x{:02X}: nt_score={}, fg_score={}, total_insn_positions={}", b, nt, fg, tot);
        }

        // Also dump the top 5 unmapped bytes by total appearances
        let mut by_total: Vec<(u8, usize, usize, usize)> = per_byte.iter()
            .map(|(&b, &(nt, fg, tot))| (b, nt, fg, tot))
            .collect();
        by_total.sort_by_key(|&(_, _, _, tot)| std::cmp::Reverse(tot));
        eprintln!("\n=== TOP UNMAPPED BYTES BY INSTRUCTION-POSITION COUNT ===");
        for (b, nt, fg, tot) in by_total.iter().take(15) {
            eprintln!("  byte 0x{:02X}: total={}, nt={}, fg={}", b, tot, nt, fg);
        }
    }

    /// Run full `OpcodeMap::detect` on each real captured file and report what
    /// standard opcode byte 0xC6 ends up mapped to. If detect_newtable works,
    /// the answer should be `53 (NEWTABLE)` for large files. If another detector
    /// is stealing the byte, the answer tells us which one.
    #[test]
    #[ignore]
    fn diag_trace_c6_assignment_on_real_files() {
        let files = load_dumped_bytecode_files();
        if files.is_empty() {
            eprintln!("no bytecode dumps available — skipping diagnostic");
            return;
        }
        // Sort by descending size — larger files are more likely to have enough
        // NEWTABLE usage for the detector to work.
        let mut sized: Vec<(String, Vec<u8>)> = files;
        sized.sort_by_key(|(_, d)| std::cmp::Reverse(d.len()));

        // Seed with cache if available — this mirrors the production flow.
        let prior = load_cached_opmap().unwrap_or([255u8; 256]);
        eprintln!("== Running full OpcodeMap::detect_with_prior on {} files, reporting byte 0xC6 fate ==", sized.len());
        eprintln!("   (prior seeded from cache: {} entries)",
                  prior.iter().filter(|&&v| v != 255).count());
        let mut newtable_count = 0usize;
        let mut unmapped_count = 0usize;
        let mut by_assignment: std::collections::HashMap<u8, usize> = std::collections::HashMap::new();
        for (fname, data) in sized.iter().take(49) {
            let chunk = match crate::parser::parse(data) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let map = OpcodeMap::detect_with_prior(&chunk, &prior);
            let mapping_0xc6 = map.heuristic_map[0xC6];
            *by_assignment.entry(mapping_0xc6).or_insert(0) += 1;
            if mapping_0xc6 == LuauOpcode::NewTable as u8 {
                newtable_count += 1;
            } else if mapping_0xc6 == 255 {
                unmapped_count += 1;
            }
            // Print first 10 for human inspection
            if by_assignment.values().sum::<usize>() <= 10 {
                let name = if mapping_0xc6 == 255 {
                    "UNMAPPED".to_string()
                } else {
                    format!("{:?}", LuauOpcode::from_u8(mapping_0xc6))
                };
                eprintln!("  {:<40} size={:6}  0xC6 -> {} ({})", fname, data.len(), mapping_0xc6, name);
            }
        }
        eprintln!("\n=== byte 0xC6 assignment distribution across all files ===");
        let mut ranked: Vec<(u8, usize)> = by_assignment.into_iter().collect();
        ranked.sort_by_key(|&(_, c)| std::cmp::Reverse(c));
        for (op, count) in ranked {
            let name = if op == 255 {
                "UNMAPPED".to_string()
            } else {
                format!("{:?}", LuauOpcode::from_u8(op))
            };
            eprintln!("  {} files: op {} ({})", count, op, name);
        }
        eprintln!("\nSUMMARY: NEWTABLE assigned in {} files, UNMAPPED in {} files",
                  newtable_count, unmapped_count);

        // Also trace what byte NEWTABLE ends up assigned to in the heuristic map
        let mut sized2 = load_dumped_bytecode_files();
        sized2.sort_by_key(|(_, d)| std::cmp::Reverse(d.len()));
        eprintln!("\n=== Where does NEWTABLE (std op 53) end up across all files? ===");
        let mut newtable_byte_distribution: std::collections::HashMap<u8, usize> = std::collections::HashMap::new();
        // Also track per-file assignment for spotting patterns.
        let mut per_file: Vec<(String, usize, u8, usize)> = Vec::new(); // (name, size, nt_byte, insn_count)
        for (fname, data) in sized2.iter().take(49) {
            let chunk = match crate::parser::parse(data) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let insn_count: usize = chunk.protos.iter().map(|p| p.code.len()).sum();
            let map = OpcodeMap::detect_with_prior(&chunk, &prior);
            let nt_byte = map.heuristic_map.iter().position(|&v| v == LuauOpcode::NewTable as u8);
            let key = nt_byte.map(|b| b as u8).unwrap_or(255);
            *newtable_byte_distribution.entry(key).or_insert(0) += 1;
            per_file.push((fname.clone(), data.len(), key, insn_count));
        }
        let mut ranked: Vec<(u8, usize)> = newtable_byte_distribution.into_iter().collect();
        ranked.sort_by_key(|&(_, c)| std::cmp::Reverse(c));
        for (b, c) in ranked {
            if b == 255 {
                eprintln!("  {} files: NEWTABLE NOT ASSIGNED", c);
            } else {
                eprintln!("  {} files: NEWTABLE -> byte 0x{:02X}", c, b);
            }
        }

        eprintln!("\n=== Per-file details (wrong bytes) ===");
        per_file.sort_by_key(|f| std::cmp::Reverse(f.1));
        for (fname, size, nt_byte, insns) in per_file.iter().take(49) {
            if *nt_byte != 0xC6 && *nt_byte != 255 {
                eprintln!("  {:<44} size={:6} insns={:5} NT=0x{:02X}", fname, size, insns, nt_byte);
            }
        }
    }

    /// Run `detect_newtable` on the largest real file after ONLY seeding the
    /// context with the things it depends on (SETTABLEKS, SETLIST, SETTABLEN
    /// from a cache lookup). Report the full candidate table to see why 0xC6
    /// is not selected.
    #[test]
    #[ignore]
    fn diag_newtable_detector_scoring_on_largest_file() {
        let cache = match load_cached_opmap() {
            Some(m) => m,
            None => { eprintln!("no cache"); return; }
        };
        let mut files = load_dumped_bytecode_files();
        files.sort_by_key(|(_, d)| std::cmp::Reverse(d.len()));
        // Find the largest file that parses successfully
        let (fname, chunk, data_len) = {
            let mut out = None;
            for (fname, data) in &files {
                if let Ok(c) = crate::parser::parse(data) {
                    out = Some((fname.clone(), c, data.len()));
                    break;
                }
            }
            out.expect("no parseable file")
        };
        eprintln!("== Running detect_newtable on {} ({} bytes, {} protos) ==",
                  fname, data_len, chunk.protos.len());
        eprintln!("  cache[14] (0x0E) = {} (expect 26=SETTABLEKS)", cache[14]);
        let cached_settableks = cache.iter().position(|&v| v == LuauOpcode::SetTableKS as u8);
        eprintln!("  cached SETTABLEKS byte = {:?}", cached_settableks);
        let cached_newtable = cache.iter().position(|&v| v == LuauOpcode::NewTable as u8);
        eprintln!("  cached NEWTABLE byte   = {:?}", cached_newtable);

        // Run detect_with_prior using the cache, then show the state just
        // before detect_newtable would run (i.e., the final map minus NewTable).
        // This lets us see what's stealing 0xC6 in the cache-seeded flow.
        let full_map = OpcodeMap::detect_with_prior(&chunk, &cache);
        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        // Seed ctx with EVERYTHING from the heuristic_map EXCEPT NewTable.
        for (shuffled, &std_op) in full_map.heuristic_map.iter().enumerate() {
            if std_op == 255 { continue; }
            if std_op == LuauOpcode::NewTable as u8 { continue; }
            ctx.map[shuffled] = std_op;
            ctx.assigned[std_op as usize] = true;
        }
        let seeded = ctx.map.iter().filter(|&&v| v != 255).count();
        eprintln!("  seeded {} ops from cache-seeded per-file detect (excluding NewTable)", seeded);
        // Show what NEWTABLE got assigned to (full map including any completion)
        let nt_byte = full_map.shuffled_to_standard.iter().position(|&v| v == LuauOpcode::NewTable as u8);
        eprintln!("  per-file detect FULL NewTable -> {:?}", nt_byte);
        // Also heuristic (pre-completion)
        let nt_heur = full_map.heuristic_map.iter().position(|&v| v == LuauOpcode::NewTable as u8);
        eprintln!("  per-file detect HEURISTIC NewTable -> {:?}", nt_heur);
        // And what 0xC6 got assigned to
        let c6_heur = full_map.heuristic_map[0xC6];
        let c6_full = full_map.shuffled_to_standard[0xC6];
        eprintln!("  0xC6 heuristic -> {} (full -> {})",
                  if c6_heur == 255 { "UNMAPPED".to_string() } else { format!("{} ({:?})", c6_heur, LuauOpcode::from_u8(c6_heur)) },
                  if c6_full == 255 { "UNMAPPED".to_string() } else { format!("{} ({:?})", c6_full, LuauOpcode::from_u8(c6_full)) });

        // Now reimplement the scoring loop INLINE with instrumentation
        let settableks_op = ctx.find_shuffled(LuauOpcode::SetTableKS as u8);
        let setlist_op = ctx.find_shuffled(LuauOpcode::SetList as u8);
        let settablen_op = ctx.find_shuffled(LuauOpcode::SetTableN as u8);
        eprintln!("  fill ops: settableks={:?} setlist={:?} settablen={:?}",
                  settableks_op, setlist_op, settablen_op);
        #[derive(Default, Debug)]
        struct Cand {
            total: usize,
            weighted_fill: usize,
            nonempty_hint_hits: usize,
            nonempty_with_fills: usize,
            proto_start: usize,
            distinct_protos: std::collections::HashSet<usize>,
        }
        let mut candidates: HashMap<u8, Cand> = HashMap::new();

        for (pi, proto) in chunk.protos.iter().enumerate() {
            let code = &proto.code;
            let mut i = 0usize;
            while i < code.len() {
                let insn = code[i];
                let op = insn_op(insn);
                let mapped = ctx.map[op as usize];
                if mapped != 255 {
                    let s = LuauOpcode::from_u8(mapped);
                    if s.has_aux() && i + 1 < code.len() { i += 2; } else { i += 1; }
                    continue;
                }
                let a = insn_a(insn);
                let b = insn_b(insn);
                let c = insn_c(insn);
                if c != 0 || a >= proto.max_stack_size {
                    i += 1;
                    continue;
                }
                let aux = if i + 1 < code.len() { code[i + 1] } else { 0 };
                let aux_looks_like_hint = aux <= 65535;
                let has_hint = (b > 0 || aux > 0) && aux_looks_like_hint;
                let strict_hint = b > 0 && aux_looks_like_hint;

                let entry = candidates.entry(op).or_default();
                entry.total += 1;
                entry.distinct_protos.insert(pi);
                if has_hint { entry.nonempty_hint_hits += 1; }

                let mut fill_count = 0usize;
                for j in (i + 2)..code.len() {
                    let fop = insn_op(code[j]);
                    let fb = insn_b(code[j]);
                    if (Some(fop) == settableks_op
                        || Some(fop) == setlist_op
                        || Some(fop) == settablen_op)
                        && fb == a
                    {
                        fill_count += 1;
                        if fill_count >= 32 { break; }
                    }
                }
                if fill_count > 0 {
                    entry.weighted_fill += fill_count;
                    if has_hint {
                        entry.nonempty_with_fills += 1;
                        if i <= 1 && fill_count >= 3 && strict_hint {
                            entry.proto_start += 1000;
                        }
                    }
                }
                i += 1;
            }
        }

        // Rank and print
        let score_fn = |c: &Cand| -> usize {
            c.nonempty_with_fills * 5 + c.proto_start + c.distinct_protos.len() * 2 + c.nonempty_hint_hits
        };
        let mut ranked: Vec<(u8, usize, &Cand)> = candidates.iter()
            .map(|(&b, c)| (b, score_fn(c), c))
            .collect();
        ranked.sort_by_key(|&(_, s, _)| std::cmp::Reverse(s));
        eprintln!("\n=== Top 20 NEWTABLE candidates from detect_newtable logic ===");
        for (b, s, c) in ranked.iter().take(20) {
            eprintln!(
                "  0x{:02X}: score={} total={} nonempty_hints={} nonempty_fills={} proto_start={} weighted_fill={} protos={}",
                b, s, c.total, c.nonempty_hint_hits, c.nonempty_with_fills, c.proto_start, c.weighted_fill, c.distinct_protos.len()
            );
        }
        // Specifically find 0xC6
        if let Some(c6) = candidates.get(&0xC6) {
            eprintln!("\n=== 0xC6 detailed ===");
            eprintln!("  score: {}", score_fn(c6));
            eprintln!("  {:#?}", c6);
        } else {
            eprintln!("\n0xC6 has NO candidate entry — it's being filtered out by `c != 0 || a >= max_stack`");
        }

        // Re-run with STRICT b <= 15 filter (NEWTABLE's hash hint is log2 0-15)
        eprintln!("\n=== Re-running with STRICT b <= 15 filter ===");
        let mut candidates2: HashMap<u8, Cand> = HashMap::new();
        for (pi, proto) in chunk.protos.iter().enumerate() {
            let code = &proto.code;
            let mut i = 0usize;
            while i < code.len() {
                let insn = code[i];
                let op = insn_op(insn);
                let mapped = ctx.map[op as usize];
                if mapped != 255 {
                    let s = LuauOpcode::from_u8(mapped);
                    if s.has_aux() && i + 1 < code.len() { i += 2; } else { i += 1; }
                    continue;
                }
                let a = insn_a(insn);
                let b = insn_b(insn);
                let c = insn_c(insn);
                // STRICT: b <= 15 (real NEWTABLE hash hint limit)
                if c != 0 || a >= proto.max_stack_size || b > 15 {
                    i += 1;
                    continue;
                }
                let aux = if i + 1 < code.len() { code[i + 1] } else { 0 };
                let aux_looks_like_hint = aux <= 65535;
                let has_hint = (b > 0 || aux > 0) && aux_looks_like_hint;
                let strict_hint = b > 0 && aux_looks_like_hint;

                let entry = candidates2.entry(op).or_default();
                entry.total += 1;
                entry.distinct_protos.insert(pi);
                if has_hint { entry.nonempty_hint_hits += 1; }

                let mut fill_count = 0usize;
                for j in (i + 2)..code.len() {
                    let fop = insn_op(code[j]);
                    let fb = insn_b(code[j]);
                    if (Some(fop) == settableks_op
                        || Some(fop) == setlist_op
                        || Some(fop) == settablen_op)
                        && fb == a
                    {
                        fill_count += 1;
                        if fill_count >= 32 { break; }
                    }
                }
                if fill_count > 0 {
                    entry.weighted_fill += fill_count;
                    if has_hint {
                        entry.nonempty_with_fills += 1;
                        if i <= 1 && fill_count >= 3 && strict_hint {
                            entry.proto_start += 1000;
                        }
                    }
                }
                i += 1;
            }
        }
        let mut ranked2: Vec<(u8, usize, &Cand)> = candidates2.iter()
            .map(|(&b, c)| (b, score_fn(c), c))
            .collect();
        ranked2.sort_by_key(|&(_, s, _)| std::cmp::Reverse(s));
        eprintln!("Top 20 NEWTABLE candidates WITH b <= 15 filter:");
        for (b, s, c) in ranked2.iter().take(20) {
            eprintln!(
                "  0x{:02X}: score={} total={} nonempty_hints={} nonempty_fills={} proto_start={} weighted_fill={} protos={}",
                b, s, c.total, c.nonempty_hint_hits, c.nonempty_with_fills, c.proto_start, c.weighted_fill, c.distinct_protos.len()
            );
        }
        if let Some(c6) = candidates2.get(&0xC6) {
            eprintln!("\n0xC6 with strict filter: score={}, {:#?}", score_fn(c6), c6);
        }

        // Final validation: run the REAL detect_newtable on the cache-seeded ctx
        // and assert it picks 0xC6 for NEWTABLE (the known-correct answer for the
        // largest bytecode file).
        detect_newtable(&chunk, &mut ctx);
        let nt_byte = ctx.map.iter().position(|&v| v == LuauOpcode::NewTable as u8);
        eprintln!("\ndetect_newtable assigned NewTable -> {:?}", nt_byte);
        assert_eq!(nt_byte, Some(0xC6), "detect_newtable should select 0xC6 for NEWTABLE in the largest file");
    }

    /// Sanity check: count RAW occurrences of specific bytes in the largest file,
    /// AUX-aware with the cached opmap, and show the shape histogram.
    /// This tells us definitively whether a byte is actually present and what
    /// shape it usually has.
    #[test]
    #[ignore]
    fn diag_raw_shape_histogram_for_candidates() {
        let cache = match load_cached_opmap() {
            Some(m) => m,
            None => { eprintln!("no cache"); return; }
        };
        let mut files = load_dumped_bytecode_files();
        files.sort_by_key(|(_, d)| std::cmp::Reverse(d.len()));
        let (fname, chunk, _) = {
            let mut out = None;
            for (fname, data) in &files {
                if let Ok(c) = crate::parser::parse(data) {
                    out = Some((fname.clone(), c, data.len()));
                    break;
                }
            }
            out.expect("no parseable file")
        };
        eprintln!("== RAW shape histogram on {} ({} protos) ==", fname, chunk.protos.len());

        // Compute frequency of EVERY byte at TRUE instruction positions (AUX-aware with cache)
        let mut byte_freq: [u32; 256] = [0; 256];
        let mut byte_shape_c_zero: [u32; 256] = [0; 256];
        let mut byte_shape_b_le15: [u32; 256] = [0; 256];
        let mut byte_shape_ad_backward: [u32; 256] = [0; 256];
        let mut byte_shape_ad_forward: [u32; 256] = [0; 256];
        let mut byte_pos_zero: [u32; 256] = [0; 256]; // at proto position 0 or 1
        let mut total_insns = 0u32;

        for proto in &chunk.protos {
            let code = &proto.code;
            let mut i = 0usize;
            while i < code.len() {
                let insn = code[i];
                let op = insn_op(insn) as usize;
                byte_freq[op] += 1;
                total_insns += 1;

                let a = insn_a(insn);
                let b = insn_b(insn);
                let c = insn_c(insn);
                let d = insn_d(insn) as i32;
                if c == 0 && a < proto.max_stack_size { byte_shape_c_zero[op] += 1; }
                if b <= 15 && c == 0 && a < proto.max_stack_size { byte_shape_b_le15[op] += 1; }
                if d < 0 && a < proto.max_stack_size { byte_shape_ad_backward[op] += 1; }
                if d > 0 && a < proto.max_stack_size { byte_shape_ad_forward[op] += 1; }
                if i <= 1 { byte_pos_zero[op] += 1; }

                // AUX step
                let mapped = cache[op];
                if mapped != 255 {
                    let s = LuauOpcode::from_u8(mapped);
                    if s.has_aux() && i + 1 < code.len() { i += 2; } else { i += 1; }
                } else {
                    i += 1;
                }
            }
        }

        eprintln!("total true instruction positions: {}", total_insns);
        eprintln!("\n=== Top 30 UNMAPPED bytes by frequency ===");
        let mut unmapped: Vec<(u8, u32)> = (0..256usize)
            .filter(|&i| cache[i] == 255 && byte_freq[i] > 0)
            .map(|i| (i as u8, byte_freq[i]))
            .collect();
        unmapped.sort_by_key(|&(_, f)| std::cmp::Reverse(f));
        for (b, f) in unmapped.iter().take(30) {
            let i = *b as usize;
            eprintln!(
                "  0x{:02X}: freq={} c0_shape={} newtable_shape={} ad_back={} ad_fwd={} pos_zero={}",
                b, f, byte_shape_c_zero[i], byte_shape_b_le15[i],
                byte_shape_ad_backward[i], byte_shape_ad_forward[i], byte_pos_zero[i]
            );
        }

        eprintln!("\n=== Focus bytes (suspected NewTable / ForGLoop candidates) ===");
        for &b in &[0xC6u8, 0xCF, 0x56, 0x39, 0x13, 0x23, 0x2A, 0x16, 0x28, 0xE7] {
            let i = b as usize;
            let name = if cache[i] == 255 { "UNMAPPED".to_string() }
                       else { format!("{:?}", LuauOpcode::from_u8(cache[i])) };
            eprintln!(
                "  0x{:02X} (cache={}): freq={} c0_shape={} newtable_shape={} ad_back={} ad_fwd={} pos_zero={}",
                b, name, byte_freq[i], byte_shape_c_zero[i], byte_shape_b_le15[i],
                byte_shape_ad_backward[i], byte_shape_ad_forward[i], byte_pos_zero[i]
            );
        }
    }

    // ─────────────────────────────────────────────────────────────────
    // Phase 4 regression tests: FORNPREP/FORNLOOP atomic pair constraint
    // ─────────────────────────────────────────────────────────────────

    /// Build a single-proto chunk with exactly ONE numeric-for loop.
    /// This is the Phase A Patch 1 regression fixture: the old
    /// `pair_count >= 2` gate refused to map this case; the relaxed gate
    /// accepts it as long as the winning pair is the unique candidate.
    ///
    /// Layout (matches real Luau VM semantics — FORNPREP's D jumps PAST
    /// FORNLOOP, not TO it, so skip_target = prep_pc + 1 + D = PC 4 here
    /// and FORNLOOP lives at skip_target - 1 = prep_pc + D = PC 3):
    ///   PC 0  : FORNPREP A=0 D=+3   (skip target PC 4, loop_pc = 0+3 = 3)
    ///   PC 1,2: body filler
    ///   PC 3  : FORNLOOP A=0 D=-3   (back to 3+1-3 = 1 — body start)
    ///   PC 4  : return-ish          (this is the skip target; past the loop)
    fn build_single_numeric_for_proto(prep_byte: u8, loop_byte: u8) -> Chunk {
        let code = vec![
            insn_ad(prep_byte, 0, 3),       // 0: FORNPREP (D=3 → loop at 3)
            insn_abc(0xAA, 1, 0, 0),        // 1: body
            insn_abc(0xAB, 1, 1, 0),        // 2: body
            insn_ad(loop_byte, 0, -3),      // 3: FORNLOOP back to 1
            insn_abc(0xCC, 0, 0, 0),        // 4: return-ish (skip target)
        ];
        chunk_from_code(code, 4)
    }

    /// Build a two-loop numeric-for proto using the specified shuffled bytes.
    /// Layout (real Luau VM semantics — see build_single_numeric_for_proto):
    ///   PC 0  : FORNPREP A=0 D=+3   (loop_pc = 0+3 = 3)
    ///   PC 1,2: body filler
    ///   PC 3  : FORNLOOP A=0 D=-3   (back to 1)
    ///   PC 4  : FORNPREP A=0 D=+3   (loop_pc = 4+3 = 7)
    ///   PC 5,6: body filler
    ///   PC 7  : FORNLOOP A=0 D=-3   (back to 5)
    ///   PC 8  : return-ish          (also the skip target of the second prep)
    fn build_two_numeric_for_proto(prep_byte: u8, loop_byte: u8) -> Chunk {
        let code = vec![
            insn_ad(prep_byte, 0, 3),       // 0: FORNPREP (D=3 → loop at 3)
            insn_abc(0xAA, 1, 0, 0),        // 1: body
            insn_abc(0xAB, 1, 1, 0),        // 2: body
            insn_ad(loop_byte, 0, -3),      // 3: FORNLOOP back to 1
            insn_ad(prep_byte, 0, 3),       // 4: FORNPREP (D=3 → loop at 7)
            insn_abc(0xAA, 1, 0, 0),        // 5: body
            insn_abc(0xAB, 1, 1, 0),        // 6: body
            insn_ad(loop_byte, 0, -3),      // 7: FORNLOOP back to 5
            insn_abc(0xCC, 0, 0, 0),        // 8: return-ish
        ];
        chunk_from_code(code, 4)
    }

    #[test]
    fn detect_numeric_for_maps_both_prep_and_loop_atomically() {
        // Happy path: a well-formed FORNPREP/FORNLOOP pair must be mapped together.
        let prep_byte: u8 = 0x39;
        let loop_byte: u8 = 0xA8;
        let chunk = build_two_numeric_for_proto(prep_byte, loop_byte);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_numeric_for(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[prep_byte as usize],
            LuauOpcode::ForNPrep as u8,
            "expected ForNPrep to be mapped to 0x{:02X}", prep_byte
        );
        assert_eq!(
            ctx.map[loop_byte as usize],
            LuauOpcode::ForNLoop as u8,
            "expected ForNLoop to be mapped to 0x{:02X}", loop_byte
        );
    }

    #[test]
    fn detect_numeric_for_never_assigns_half_pair_when_loop_is_missing() {
        // Regression: a bytecode stream with what LOOKS like FORNPREP but no
        // matching FORNLOOP (e.g. the target is something else entirely) must
        // NOT cause ForNPrep to be assigned alone. Before the atomic-pair fix,
        // detect_numeric_for's separate max_by paths could leak one half into
        // the cache, poisoning every downstream script's loop reconstruction.
        //
        // Here we build a proto with a prep-shaped insn whose "loop target"
        // doesn't look like a backward-jumping FORxLOOP at all. The pair
        // counter should never tick, so no assignment happens.
        let prep_byte: u8 = 0x39;
        let code = vec![
            insn_ad(prep_byte, 0, 2),        // 0: looks like FORNPREP forward jump
            insn_abc(0xAA, 1, 0, 0),         // 1: body
            insn_abc(0xAB, 1, 1, 0),         // 2: body
            insn_abc(0xBB, 2, 0, 0),         // 3: target is ABC (not a backward jump)
            insn_abc(0xCC, 0, 0, 0),         // 4: return-ish
        ];
        let chunk = chunk_from_code(code, 4);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_numeric_for(&chunk, &mut ctx);

        assert!(
            !ctx.assigned[LuauOpcode::ForNPrep as usize],
            "ForNPrep must not be assigned when no matching FORNLOOP target exists"
        );
        assert!(
            !ctx.assigned[LuauOpcode::ForNLoop as usize],
            "ForNLoop must not be assigned when no prep-loop pair exists"
        );
        assert_eq!(
            ctx.map[prep_byte as usize], 255,
            "prep byte 0x{:02X} must remain unmapped — no half-pair allowed", prep_byte
        );
    }

    #[test]
    fn detect_numeric_for_accepts_single_unique_pair() {
        // Phase A Patch 1: a single FORNPREP/FORNLOOP pair must be mapped
        // when it is the only candidate in the proto set. Before the patch,
        // `pair_count >= 2` blocked single-loop scripts from contributing
        // FORNPREP/FORNLOOP evidence, leaving the cache perpetually 80/84.
        let prep_byte: u8 = 0xA8;
        let loop_byte: u8 = 0x8B;
        let chunk = build_single_numeric_for_proto(prep_byte, loop_byte);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_numeric_for(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[prep_byte as usize],
            LuauOpcode::ForNPrep as u8,
            "single unique pair must map ForNPrep (0x{:02X})", prep_byte
        );
        assert_eq!(
            ctx.map[loop_byte as usize],
            LuauOpcode::ForNLoop as u8,
            "single unique pair must map ForNLoop (0x{:02X})", loop_byte
        );
    }

    #[test]
    fn detect_numeric_for_rejects_ambiguous_single_pairs() {
        // Phase A Patch 1: when two DIFFERENT pair candidates each have count
        // 1 (two distinct prep_byte/loop_byte combos, neither repeated), the
        // detector must not commit — the structural match is not unique
        // enough to be safe. Half-mapping here would poison the cache.
        //
        // Build a proto with TWO single-loop patterns using DIFFERENT bytes:
        //   prep A=0xA8, loop A=0x8B  (candidate 1)
        //   prep B=0x39, loop B=0x42  (candidate 2)
        // Both pairs have count 1, both pass structural filters.
        let code = vec![
            insn_ad(0xA8, 0, 3),            // 0: FORNPREP-A (D=3 → loop at 3)
            insn_abc(0xAA, 1, 0, 0),        // 1: body
            insn_abc(0xAB, 1, 1, 0),        // 2: body
            insn_ad(0x8B, 0, -3),           // 3: FORNLOOP-A back to 1
            insn_ad(0x39, 0, 3),            // 4: FORNPREP-B (D=3 → loop at 7)
            insn_abc(0xAA, 1, 0, 0),        // 5: body
            insn_abc(0xAB, 1, 1, 0),        // 6: body
            insn_ad(0x42, 0, -3),           // 7: FORNLOOP-B (different byte)
            insn_abc(0xCC, 0, 0, 0),        // 8: return-ish
        ];
        let chunk = chunk_from_code(code, 4);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_numeric_for(&chunk, &mut ctx);

        assert!(
            !ctx.assigned[LuauOpcode::ForNPrep as usize],
            "ambiguous two-candidate pairs must NOT commit ForNPrep"
        );
        assert!(
            !ctx.assigned[LuauOpcode::ForNLoop as usize],
            "ambiguous two-candidate pairs must NOT commit ForNLoop"
        );
        assert_eq!(ctx.map[0xA8], 255);
        assert_eq!(ctx.map[0x8B], 255);
        assert_eq!(ctx.map[0x39], 255);
        assert_eq!(ctx.map[0x42], 255);
    }

    #[test]
    fn detect_numeric_for_is_deterministic_across_runs() {
        // Determinism regression: run detect_numeric_for on identical input 5
        // times. The output must be bit-identical. Before Phase 1's tiebreak
        // fix, HashMap iteration order could pick a different byte on each run.
        let prep_byte: u8 = 0x39;
        let loop_byte: u8 = 0xA8;
        let chunk = build_two_numeric_for_proto(prep_byte, loop_byte);

        let mut first_map: Option<[u8; 256]> = None;
        for _ in 0..5 {
            let mut ctx = DetectCtx::new();
            ctx.compute_frequencies(&chunk);
            detect_numeric_for(&chunk, &mut ctx);
            if let Some(prev) = first_map.as_ref() {
                assert_eq!(
                    prev, &ctx.map,
                    "detect_numeric_for output differed between runs — non-determinism regression"
                );
            } else {
                first_map = Some(ctx.map);
            }
        }
    }

    // ─────────────────────────────────────────────────────────────────
    // Phase 3 regression tests: NOT/MINUS/LENGTH structural protection
    // ─────────────────────────────────────────────────────────────────

    #[test]
    fn is_structural_required_covers_unary_ops() {
        // Phase 3b contract: NOT, MINUS, LENGTH must be treated as structural-
        // required so greedy/format-match passes never guess them. Future
        // refactors must not drop them from this list — if they do, the
        // `0xF6 → MINUS` / `0x1C → NOT` regression from ground-truth
        // ModuleScript.luac will come back.
        assert!(
            DetectCtx::is_structural_required_standard_opcode(LuauOpcode::Not as u8),
            "NOT must be structural-required to block blind greedy assignment"
        );
        assert!(
            DetectCtx::is_structural_required_standard_opcode(LuauOpcode::Minus as u8),
            "MINUS must be structural-required to block blind greedy assignment"
        );
        assert!(
            DetectCtx::is_structural_required_standard_opcode(LuauOpcode::Length as u8),
            "LENGTH must be structural-required to block blind greedy assignment"
        );
    }

    // ─────────────────────────────────────────────────────────────────
    // Phase B0.19 tests: augmenter format-consistency override for unary ops
    // ─────────────────────────────────────────────────────────────────

    /// Build a prior map that matches ALL of known_shuffles variant 1 EXCEPT the
    /// unary bytes 0xD4 (Minus) and 0x56 (Length), so that:
    ///   - find_best_known_shuffle selects variant 1 with a high score (≥50 mappings,
    ///     0 conflicts from the subset we provide)
    ///   - The augmenter fills in 0xD4 → Minus and 0x56 → Length
    ///   - No other detector can steal 0xD4 because every other variant-1 byte is
    ///     already assigned in the prior, leaving no "And/Or/Move-shaped" candidates
    fn make_variant1_prior() -> [u8; 256] {
        // ALL variant 1 mappings from make_variant_1() in known_shuffles.rs,
        // EXCEPT 0xD4 (Minus=51) and 0x56 (Length=52) which are intentionally omitted.
        let mut prior = [255u8; 256];
        prior[0xFE] = 0;   // Nop
        prior[0xFD] = 1;   // Break
        prior[0x08] = 2;   // LoadNil
        prior[0x04] = 3;   // LoadB
        // LoadN (4) omitted — variant 1 doesn't map it
        prior[0x52] = 5;   // LoadK
        prior[0x6F] = 6;   // Move
        prior[0xFF] = 7;   // GetGlobal
        prior[0x7D] = 8;   // SetGlobal
        prior[0x02] = 9;   // GetUpval
        prior[0xC6] = 10;  // SetUpval
        prior[0x05] = 11;  // CloseUpvals
        prior[0xA4] = 12;  // GetImport
        prior[0x12] = 13;  // GetTable
        prior[0xA9] = 14;  // SetTable
        prior[0x4D] = 15;  // GetTableKS
        prior[0x30] = 16;  // SetTableKS
        // GetTableN(17), SetTableN(18), NewClosure(19) omitted — not in variant 1
        prior[0xBC] = 20;  // NameCall
        prior[0x9F] = 21;  // Call
        prior[0x82] = 22;  // Return
        prior[0x65] = 23;  // Jump
        prior[0x6E] = 24;  // JumpBack
        prior[0xFB] = 25;  // JumpIf
        prior[0x0E] = 26;  // JumpIfNot
        prior[0xF1] = 27;  // JumpIfEq
        // JumpIfLE(28) omitted
        prior[0x47] = 29;  // JumpIfLT
        prior[0x9A] = 30;  // JumpIfNotEq
        // JumpIfNotLE(31) omitted
        prior[0xB7] = 32;  // JumpIfNotLT
        prior[0x87] = 33;  // Add
        // Sub(34)..Pow(38) omitted — not in variant 1
        prior[0x11] = 39;  // AddK
        prior[0x8C] = 40;  // SubK
        // MulK(41) omitted — not in variant 1
        // DivK(42) omitted
        prior[0x1C] = 43;  // ModK
        prior[0x78] = 44;  // PowK
        prior[0x03] = 45;  // And
        prior[0x09] = 46;  // Or
        prior[0x01] = 47;  // AndK
        // OrK(48) omitted
        prior[0x73] = 49;  // Concat
        prior[0x13] = 50;  // Not
        // 0xD4 → Minus (51) intentionally omitted — augmenter should fill it
        // 0x56 → Length (52) intentionally omitted — augmenter should fill it
        prior[0x10] = 53;  // NewTable
        prior[0xE2] = 54;  // DupTable
        prior[0xD9] = 55;  // SetList
        prior[0xA8] = 56;  // ForNPrep
        prior[0x8B] = 57;  // ForNLoop
        // ForGPrep(58) omitted
        prior[0xC5] = 59;  // ForGLoop
        // ForGPrepINext(60), Deprecated61(61), ForGPrepNext(62) omitted
        prior[0xFA] = 63;  // NativeCall
        prior[0x15] = 64;  // GetVarargs
        prior[0xA3] = 65;  // PrepVarargs
        // LoadKX(66), JumpX(67), FastCall(68) omitted
        prior[0xFC] = 69;  // Coverage
        prior[0x00] = 70;  // Capture
        // SubRK(71), DivRK(72) omitted
        prior[0x9E] = 73;  // FastCall1
        prior[0x34] = 74;  // FastCall2
        // FastCall2K(75)..JumpXEqKN(80) omitted
        prior[0x60] = 81;  // JumpXEqKS
        prior[0xC0] = 82;  // DupClosure
        prior
    }

    #[test]
    fn augmenter_keeps_unary_byte_when_format_consistent() {
        // Phase B0.19 regression: when the known-shuffles augmenter proposes a
        // byte for Minus/Length/Not and that byte appears ONLY in unary ABC format
        // (C=0, A!=B, A<stack, B<stack) within the chunk, the format-consistency
        // override must keep the assignment instead of reverting it.
        //
        // Scenario: a chunk where:
        //   - Most bytes match variant 1 (so augmenter picks variant 1)
        //   - 0xD4 (variant 1's Minus byte) appears 3 times in pure unary format
        //   - detect_unary_not_minus DID NOT fire (no numeric consumers nearby)
        //
        // After the augmenter's format-consistency override: 0xD4 → Minus (51).

        // Prior: variant 1's structural bytes (no 0xD4 yet).
        let prior = make_variant1_prior();

        // Chunk: a few unary-format 0xD4 instructions mixed with structural filler.
        // We must include enough variant-1-matching bytes to make the augmenter
        // select variant 1 (prior handles that). The chunk itself just needs some
        // 0xD4 instructions in pure unary format.
        let unary_byte: u8 = 0xD4;  // variant 1 maps this to Minus (51)
        let code = vec![
            // Unary: A=1, B=2, C=0 (valid for maxstack=8)
            insn_abc(unary_byte, 1, 2, 0),
            // Unary: A=3, B=0, C=0 (A!=B, both valid)
            insn_abc(unary_byte, 3, 0, 0),
            // Unary: A=0, B=1, C=0
            insn_abc(unary_byte, 0, 1, 0),
            // Filler (a byte that doesn't match any pattern)
            insn_abc(0xCC, 0, 0, 0),
        ];
        let chunk = chunk_from_code(code, 8);

        let result = OpcodeMap::detect_with_prior(&chunk, &prior);

        assert_eq!(
            result.shuffled_to_standard[unary_byte as usize],
            LuauOpcode::Minus as u8,
            "Phase B0.19: augmenter should keep 0x{:02X} → Minus when all instances \
             have pure unary format (C=0, A!=B, valid registers)",
            unary_byte
        );
    }

    #[test]
    fn augmenter_rejects_unary_byte_when_format_inconsistent() {
        // Phase B0.19 regression: if the proposed byte does NOT appear exclusively
        // in unary format (e.g., some instance has C!=0), the format-consistency
        // override must NOT keep it. The structural-required revert applies.
        //
        // Scenario: 0xD4 appears mostly in non-unary format (C!=0).
        // The augmenter proposes 0xD4 → Minus (variant 1), but the format check
        // fails because one instance has C=3 (not unary). Revert applies.

        let prior = make_variant1_prior();

        let unary_byte: u8 = 0xD4;
        let code = vec![
            // Two instances with C=0 (unary-looking)
            insn_abc(unary_byte, 1, 2, 0),
            insn_abc(unary_byte, 3, 0, 0),
            // One instance with C=3 (NOT unary format — breaks the "all C=0" rule)
            insn_abc(unary_byte, 0, 1, 3),
            insn_abc(0xCC, 0, 0, 0),
        ];
        let chunk = chunk_from_code(code, 8);

        let result = OpcodeMap::detect_with_prior(&chunk, &prior);

        // Format check fails: NOT all instances have C=0. Augmenter assignment reverts.
        assert_ne!(
            result.shuffled_to_standard[unary_byte as usize],
            LuauOpcode::Minus as u8,
            "Phase B0.19: augmenter must NOT keep 0x{:02X} → Minus when some \
             instances have C!=0 (non-unary format)",
            unary_byte
        );
    }

    // ─────────────────────────────────────────────────────────────────
    // Phase 6 regression tests: per-detector determinism with tied candidates
    // ─────────────────────────────────────────────────────────────────

    /// Build a chunk with THREE shuffled bytes that all match the LOADNIL
    /// pattern (A<maxstack, B=C=0) with identical instance counts. Before the
    /// Phase 6 tiebreak fix, HashMap iteration order picked an arbitrary winner.
    fn build_loadnil_tied_chunk(b0: u8, b1: u8, b2: u8) -> Chunk {
        let code = vec![
            insn_abc(b0, 0, 0, 0),
            insn_abc(b0, 1, 0, 0),
            insn_abc(b1, 0, 0, 0),
            insn_abc(b1, 1, 0, 0),
            insn_abc(b2, 0, 0, 0),
            insn_abc(b2, 1, 0, 0),
        ];
        chunk_from_code(code, 4)
    }

    #[test]
    fn detect_loadnil_is_deterministic_under_tied_candidates() {
        // Three bytes, each with 2 clean instances — LOADNIL detector MUST
        // pick the lowest byte (ascending tiebreak) every time.
        let chunk = build_loadnil_tied_chunk(0x03, 0x04, 0x05);

        let expected_winner: u8 = 0x03;
        for run in 0..50 {
            let mut ctx = DetectCtx::new();
            ctx.compute_frequencies(&chunk);
            detect_loadnil(&chunk, &mut ctx);
            assert_eq!(
                ctx.map[expected_winner as usize],
                LuauOpcode::LoadNil as u8,
                "run {}: detect_loadnil picked {:02X} instead of {:02X} — HashMap iteration leaked non-determinism",
                run, expected_winner, expected_winner
            );
            // The other two tied bytes must NOT have been assigned.
            assert_eq!(ctx.map[0x04], 255, "run {}: 0x04 wrongly assigned", run);
            assert_eq!(ctx.map[0x05], 255, "run {}: 0x05 wrongly assigned", run);
        }
    }

    /// Build a chunk where detect_concat has two candidates with identical
    /// stats: both have 3 valid CONCAT-shaped instances. Tiebreak must pick
    /// the lower byte every time.
    fn build_concat_tied_chunk(b0: u8, b1: u8) -> Chunk {
        // CONCAT requires B < C, both valid registers, A valid register.
        let code = vec![
            insn_abc(b0, 0, 1, 2),
            insn_abc(b0, 0, 1, 3),
            insn_abc(b0, 0, 2, 3),
            insn_abc(b1, 0, 1, 2),
            insn_abc(b1, 0, 1, 3),
            insn_abc(b1, 0, 2, 3),
        ];
        chunk_from_code(code, 4)
    }

    #[test]
    fn detect_concat_is_deterministic_under_tied_candidates() {
        // 0xBB and 0x73 both have 3 valid CONCAT hits — matches the
        // previously-observed 9670b flake where CONCAT flipped between bytes.
        let chunk = build_concat_tied_chunk(0x73, 0xBB);

        let expected_winner: u8 = 0x73; // lower byte wins under ascending tiebreak
        for run in 0..50 {
            let mut ctx = DetectCtx::new();
            ctx.compute_frequencies(&chunk);
            detect_concat(&chunk, &mut ctx);
            assert_eq!(
                ctx.map[expected_winner as usize],
                LuauOpcode::Concat as u8,
                "run {}: detect_concat picked the wrong winner under ties",
                run
            );
            assert_eq!(ctx.map[0xBB], 255, "run {}: 0xBB wrongly won", run);
        }
    }

    // ─────────────────────────────────────────────────────────────────
    // Phase B0 regression tests: detect_jump JumpX sublogic frequency cap
    // ─────────────────────────────────────────────────────────────────

    /// Phase B0 regression: LOADN-shaped instructions at a single shuffled byte
    /// must NOT be claimed by detect_jump's JumpX sublogic.
    ///
    /// Before the Phase B0 fix, detect_jump accepted any byte with >= 2
    /// "long-jump-in-range" hits. LOADN bit layout is `op(8) a(8) d(16)`, so
    /// `insn >> 8 = a | (d << 8)` >= 256 for any D>=1, which trivially passes
    /// the sublogic's `|e_signed| > 127` filter. A handful of accidentally
    /// in-range LOADNs was enough to steal the byte from the real LoadN opcode.
    ///
    /// Root cause investigation: see memory file project_phase_b0_jumpx_diagnosis.md.
    /// On ModuleScript.luac this manifested as `0x8C -> JUMPX` while real
    /// 0x8C is LOADN, breaking Proto 9 `numeric_for_simple`.
    #[test]
    fn detect_jump_jumpx_sublogic_rejects_loadn_shaped_byte() {
        // Build a synthetic proto with 200 LOADN-shaped instructions at
        // shuffled byte 0xAB. Values of D are small positive ints typical of
        // real LOADNs (for loops, array indices, small numeric literals).
        // With D in 1..20, (insn >> 8) in [256, 20*256+a] which easily passes
        // the JumpX `|e|>127` filter. With pc in 0..200, many of these will
        // have e % 200 in-range, exercising the buggy code path.
        let mut code = Vec::new();
        for i in 0..200u32 {
            // a = i % 8, d = (i % 5) + 1 (1..5) — small positive LOADN values
            let a = (i % 8) as u8;
            let d = ((i % 5) + 1) as i16;
            code.push(insn_ad(0xAB, a, d));
        }
        let chunk = chunk_from_code(code, 16);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        // Before the fix, freq[0xAB] = 200 + `count >= 2` passes.
        // After the fix, the sublogic's `ctx.freq[op] <= jx_max` cap rejects
        // 0xAB because 200 > max(20, 200/200+1=2) = 20.
        detect_jump(&chunk, &mut ctx);
        assert_ne!(
            ctx.map[0xAB], LuauOpcode::JumpX as u8,
            "detect_jump JumpX sublogic wrongly claimed LOADN-shaped byte 0xAB \
             (freq=200). JumpX is a rare long-jump escape hatch and must never \
             be assigned to high-frequency AD-format bytes. See \
             project_phase_b0_jumpx_diagnosis.md."
        );

        // Defense in depth: try_assign's is_rare_standard_opcode gate also
        // rejects JumpX on high-frequency bytes. Verify by calling try_assign
        // directly — this exercises Patch 4b in isolation.
        let mut ctx2 = DetectCtx::new();
        ctx2.compute_frequencies(&chunk);
        let assigned = ctx2.try_assign(0xAB, LuauOpcode::JumpX as u8);
        assert!(
            !assigned,
            "try_assign should have rejected JumpX on 0xAB (freq=200 > 2% of \
             total_insns=200), but it succeeded. Patch 4b (is_rare_standard_opcode \
             += JumpX) is not in effect."
        );
    }

    /// Phase B0 counter-test: a real JumpX (rare, `|e| > 127`, low chunk freq)
    /// must still be detected after the Phase B0 fix. Guards against
    /// over-correction of `detect_jump_jumpx_sublogic_rejects_loadn_shaped_byte`.
    ///
    /// Builds a chunk with 400 innocuous filler instructions at various bytes
    /// plus exactly 2 JumpX-shaped hits at byte 0x5A:
    ///   - 0x5A at pc=0 with e = +200 (target=200 in-range, |e|=200>127)
    ///   - 0x5A at pc=398 with e = -200 (target=198 in-range, |e|=200>127)
    /// byte 0x5A's total chunk frequency is exactly 2, well below jx_max.
    #[test]
    fn detect_jump_jumpx_sublogic_still_detects_real_jumpx() {
        // Pack an E-format instruction: op(8) | e(24)
        fn insn_e(op: u8, e: i32) -> u32 {
            let e_u = (e as u32) & 0x00FF_FFFF;
            (op as u32) | (e_u << 8)
        }

        let mut code: Vec<u32> = Vec::with_capacity(400);
        // pc=0: real JumpX with e=+200 (target=200, in-range, |e|>127)
        code.push(insn_e(0x5A, 200));
        // pc=1..397: filler. Use a variety of bytes distinct from 0x5A so
        // none of them happen to pass the JumpX sublogic. insn_abc with
        // c=0 makes the top byte 0, so (insn>>8) is small — fails |e|>127.
        for i in 1..398u32 {
            // Cycle over 6 filler bytes, none equal to 0x5A
            let fb = [0x10u8, 0x20, 0x30, 0x40, 0x11, 0x21][(i as usize) % 6];
            code.push(insn_abc(fb, (i % 8) as u8, (i % 4) as u8, 0));
        }
        // pc=398: real JumpX with e=-200 (target=198, in-range, |e|>127)
        code.push(insn_e(0x5A, -200));
        // pc=399: filler to make the target valid
        code.push(insn_abc(0x10, 0, 0, 0));

        let chunk = chunk_from_code(code, 16);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        // Sanity: 0x5A freq must be exactly 2 (well under jx_max).
        assert_eq!(
            ctx.freq[0x5A] as usize, 2,
            "fixture setup bug: expected exactly 2 instances of 0x5A, got {}",
            ctx.freq[0x5A]
        );
        detect_jump(&chunk, &mut ctx);
        assert_eq!(
            ctx.map[0x5A], LuauOpcode::JumpX as u8,
            "detect_jump should still detect real JumpX on 0x5A (freq=2, 2 \
             long-jump hits, well under jx_max). If this fails, Patch 4a's \
             cap is too aggressive and has over-corrected the fix."
        );
    }

    /// Phase B0.1 regression: `detect_conditional_jumps` must refuse to claim
    /// a LOADN-shaped byte as JumpIfNot. LOADN instructions are AD-format with
    /// A = destination register (a > 0 trivially satisfied, a < max_stack always
    /// true) and D = signed literal number (d > 0 for positive literals; pc + d
    /// frequently in-range for late pc and small d). Without a raw-frequency
    /// cap, the candidate filter's structure test is satisfied by a large
    /// fraction of LOADN instances, letting LOADN-shape steal JumpIfNot's slot.
    ///
    /// Phase B0 fixed the JumpX-sublogic twin of this bug in `detect_jump`.
    /// After Phase B0 landed, 0x8C on ModuleScript.luac immediately shifted
    /// from `JumpX` to `JumpIfNot` via this sibling detector — same structural
    /// class, different detector. Phase B0.1 caps the raw frequency inside
    /// `detect_conditional_jumps` to reject LOADN-shape candidates.
    ///
    /// Root cause investigation: see memory file project_phase_b0_jumpx_diagnosis.md.
    #[test]
    fn detect_conditional_jumps_rejects_loadn_shaped_byte() {
        // Build a synthetic chunk with 500 LOADN-shaped words at byte 0xAB.
        // a is cycled across registers [1..8] (a > 0 always), d is small
        // positive literals in [1..12] — all instances trivially pass the
        // (a > 0, a < max_stack, d > 0, target < code.len()) filter used by
        // `detect_conditional_jumps`.
        //
        // Without the Phase B0.1 fix, candidates[0xAB] >= 5 ⇒ the detector
        // force-assigns `0xAB -> JumpIfNot`. With the fix,
        //     cj_freq_cap = max(20, 500/20) = 25
        //     freq[0xAB] = 500 > 25 ⇒ candidate rejected.
        let mut code: Vec<u32> = Vec::with_capacity(500);
        for i in 0..500u32 {
            // a in 1..8 (strictly > 0), d in 1..12 (strictly > 0)
            let a = ((i % 7) + 1) as u8;
            let d = ((i % 11) + 1) as i16;
            code.push(insn_ad(0xAB, a, d));
        }
        let chunk = chunk_from_code(code, 16);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        assert_eq!(
            ctx.freq[0xAB] as usize, 500,
            "fixture setup bug: expected 500 instances of 0xAB, got {}",
            ctx.freq[0xAB]
        );
        detect_conditional_jumps(&chunk, &mut ctx);
        assert_ne!(
            ctx.map[0xAB], LuauOpcode::JumpIfNot as u8,
            "detect_conditional_jumps wrongly claimed LOADN-shaped byte 0xAB \
             (freq=500, 7% of chunk) as JumpIfNot. JumpIfNot raw frequency is \
             empirically 0.3-2% of total instructions in Roblox bytecode; a \
             candidate whose raw frequency exceeds `max(20, total/20)` cannot \
             be a real JumpIfNot. See project_phase_b0_jumpx_diagnosis.md."
        );
        assert_ne!(
            ctx.map[0xAB], LuauOpcode::JumpIf as u8,
            "detect_conditional_jumps wrongly claimed LOADN-shaped byte 0xAB \
             (freq=500) as JumpIf via the single-candidate fallback. The \
             Phase B0.1 freq cap must reject the byte from BOTH JumpIfNot \
             AND JumpIf assignment paths."
        );
    }

    /// Phase B0.1 counter-test: a real JumpIfNot (raw frequency well under
    /// the 5%-of-total cap) must still be detected after the Phase B0.1 fix.
    /// Guards against over-correction of
    /// `detect_conditional_jumps_rejects_loadn_shaped_byte`.
    ///
    /// Builds a 1000-instruction chunk with:
    ///   - 20 JumpIfNot-shaped hits at byte 0xC1 (2% of total, within cap)
    ///   - 10 JumpIf-shaped hits at byte 0xC2 (1% of total, within cap)
    ///   - 970 filler instructions at bytes that don't pass the conditional
    ///     filter (c != 0 kills the a > 0 check via insn_a being the high bits)
    #[test]
    fn detect_conditional_jumps_still_detects_real_jumpifnot() {
        let mut code: Vec<u32> = Vec::with_capacity(1000);
        // First 20 positions: real JumpIfNot at 0xC1 with a=3, d=5 (forward jump)
        for _ in 0..20u32 {
            code.push(insn_ad(0xC1, 3, 5));
        }
        // Next 10 positions: real JumpIf at 0xC2 with a=2, d=7 (forward jump)
        for _ in 0..10u32 {
            code.push(insn_ad(0xC2, 2, 7));
        }
        // Remaining 970: innocuous filler at byte 0x10 with a=0 (fails a > 0)
        // and d=0 (fails d > 0) so they don't accumulate as candidates.
        for _ in 0..970u32 {
            code.push(insn_ad(0x10, 0, 0));
        }
        let chunk = chunk_from_code(code, 16);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        // Sanity: 0xC1 freq=20, 0xC2 freq=10, total=1000; cap = max(20, 50) = 50.
        // Both 20 and 10 are under 50 → should be KEPT by the Phase B0.1 cap.
        assert_eq!(ctx.freq[0xC1] as usize, 20);
        assert_eq!(ctx.freq[0xC2] as usize, 10);
        let expected_cap: u32 = std::cmp::max(20u32, 1000 / 20);
        assert_eq!(expected_cap, 50, "cap formula changed; update this test");
        assert!(ctx.freq[0xC1] <= expected_cap);
        assert!(ctx.freq[0xC2] <= expected_cap);

        detect_conditional_jumps(&chunk, &mut ctx);
        assert_eq!(
            ctx.map[0xC1], LuauOpcode::JumpIfNot as u8,
            "detect_conditional_jumps should still detect real JumpIfNot on \
             0xC1 (freq=20 ≤ cap=50, 20 conditional-shape hits). If this \
             fails, Phase B0.1's cap is too aggressive and has over-corrected \
             the fix."
        );
        assert_eq!(
            ctx.map[0xC2], LuauOpcode::JumpIf as u8,
            "detect_conditional_jumps should still detect real JumpIf on \
             0xC2 (freq=10 ≤ cap=50, 10 conditional-shape hits)."
        );
    }

    /// Phase B0.2: detect_jumpback must NOT claim a high-frequency LOADN-shaped
    /// byte just because a minority of its signed-D literals happen to be
    /// negative.
    ///
    /// Regression for the ModuleScript.luac 0x8C case:
    /// 0x8C is LOADN with 790 total instances. 17 of them happen to have
    /// negative literal D (i.e., `local v = -3`) and 10 of those land at
    /// valid in-bounds backward targets. That beat the real JumpBack byte
    /// 0x48's 5 backward hits. Without the Phase B0.2 raw-frequency cap,
    /// `detect_jumpback` assigned 0x8C → JumpBack, which cascaded into
    /// detect_loadn picking the wrong byte, which cascaded into numeric-for
    /// loops being mis-recognized as while-true in the lifter.
    ///
    /// Synthetic repro: 800 LOADN-shaped instructions at byte 0xAB, about
    /// 2% of which have d<0. The max_by count (~16) would win under the old
    /// code with no cap. With the Phase B0.2 cap of max(20, 800/20)=40, 0xAB's
    /// raw frequency 800 far exceeds 40 so it's rejected as a JumpBack
    /// candidate.
    #[test]
    fn detect_jumpback_rejects_loadn_shaped_byte() {
        // 800 LOADN-shaped at 0xAB: mostly small positive D, a ~2% minority
        // with d<0 (negative-literal `local v = -N`).
        let mut code: Vec<u32> = Vec::with_capacity(800);
        for i in 0..800u32 {
            let a = ((i % 9) + 1) as u8; // a = 1..=9 (register)
            // ~2% of instances have d<0 (matches real LOADN literal distribution)
            let d: i16 = if i % 50 == 0 { -((i % 17) as i16 + 1) } else {
                ((i % 100) + 1) as i16
            };
            code.push(insn_ad(0xAB, a, d));
        }
        let chunk = chunk_from_code(code, 16);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        assert_eq!(ctx.freq[0xAB] as usize, 800);
        // cap = max(20, 800/20) = max(20, 40) = 40. 0xAB freq 800 > 40 → rejected.
        let expected_cap: u32 = std::cmp::max(20u32, 800 / 20);
        assert_eq!(expected_cap, 40, "cap formula changed; update this test");
        assert!(ctx.freq[0xAB] > expected_cap,
            "test precondition: LoadN-shaped byte freq must exceed cap");

        detect_jumpback(&chunk, &mut ctx);
        assert_ne!(
            ctx.map[0xAB], LuauOpcode::JumpBack as u8,
            "detect_jumpback must NOT claim LoadN-shaped byte 0xAB. Phase B0.2 \
             Patch 6a's raw-frequency cap rejects any candidate whose raw \
             chunk frequency exceeds max(20, total_insns/20). If this fails, \
             the cap is missing or has been reverted."
        );
        // Also verify direct try_assign path is blocked when the shuffled byte
        // has already been excluded. Here, since 0xAB is unmapped we test the
        // detector-internal cap specifically — the try_assign 2%-cap does not
        // fire because JumpBack is not in `is_rare_standard_opcode`.
    }

    /// Phase B0.2: detect_jumpback must still detect a real JumpBack byte on
    /// a synthetic chunk where LoadN-shape noise is present at high frequency.
    ///
    /// This test mirrors the real shape of ModuleScript.luac: a high-frequency
    /// LOADN byte (mostly positive D, a few negatives) coexists with a
    /// low-frequency real JumpBack byte (mostly backward jumps).
    #[test]
    fn detect_jumpback_still_detects_real_jumpback() {
        // 20 real JumpBack at 0xE3: all backward d, in-range target.
        // Place them at pc >= 20 so `target = pc + d + 1 >= 0`.
        let mut code: Vec<u32> = Vec::with_capacity(2000);
        // Fill first 100 with innocuous non-candidates (a=0 fails most filters,
        // d=0 makes JumpBack filter fail regardless, also ensures pc for JumpBacks
        // is large enough that they have valid targets).
        for _ in 0..100u32 {
            code.push(insn_ad(0x10, 0, 0));
        }
        // Add 20 real JumpBack at 0xE3 (d=-5 → target = pc+1-5 = pc-4, well in bounds)
        for _ in 0..20u32 {
            code.push(insn_ad(0xE3, 0, -5));
        }
        // Fill remaining 1880 with a LoadN-shape noise byte at 0xAB.
        // 2% are d<0 (matches real LOADN literal distribution).
        for i in 0..1880u32 {
            let a = ((i % 9) + 1) as u8;
            let d: i16 = if i % 50 == 0 { -((i % 17) as i16 + 1) } else {
                ((i % 100) + 1) as i16
            };
            code.push(insn_ad(0xAB, a, d));
        }
        let chunk = chunk_from_code(code, 16);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        let expected_cap: u32 = std::cmp::max(20u32, 2000 / 20);
        assert_eq!(expected_cap, 100, "cap formula changed; update this test");
        // 0xE3 freq 20 < cap 100 ✓ (passes)
        // 0xAB freq 1880 > cap 100 ✓ (rejected)
        assert_eq!(ctx.freq[0xE3] as usize, 20);
        assert_eq!(ctx.freq[0xAB] as usize, 1880);
        assert!(ctx.freq[0xE3] <= expected_cap);
        assert!(ctx.freq[0xAB] > expected_cap);

        detect_jumpback(&chunk, &mut ctx);
        assert_eq!(
            ctx.map[0xE3], LuauOpcode::JumpBack as u8,
            "detect_jumpback should still detect real JumpBack at 0xE3 \
             (freq=20 ≤ cap=100, 20 backward-D hits) even when LoadN-shaped \
             noise at 0xAB is present at high frequency. If this fails, \
             Phase B0.2's cap is too aggressive and has over-corrected the fix."
        );
        assert_ne!(
            ctx.map[0xAB], LuauOpcode::JumpBack as u8,
            "detect_jumpback must NOT claim LoadN-shaped noise byte 0xAB."
        );
    }

    /// Phase B0 diagnostic: trace exactly which detector/phase first assigns
    /// 0x8C on ModuleScript.luac. Runs the same tier order as
    /// `detect_with_prior([255;256])` but takes a snapshot of `ctx.map[0x8C]`
    /// after every phase. Prints:
    ///   - 0x8C's frequency in the chunk
    ///   - the first phase to claim 0x8C, and what standard opcode it was assigned to
    ///   - a per-proto count of 0x8C occurrences and whether they look like LOADN
    ///     (A in-range, D small positive)
    ///   - whether the heuristic-map vs permutation-complete map differ for 0x8C
    ///
    /// Invoke:
    ///   cargo test --release -p luau-core --lib -- --ignored diag_phase_b0_trace_0x8c --nocapture
    #[test]
    #[ignore]
    fn diag_phase_b0_trace_0x8c() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent().unwrap()
            .parent().unwrap()
            .join("inspect/ModuleScript.luac");
        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(e) => { eprintln!("cannot read {}: {}", path.display(), e); return; }
        };
        let chunk = match crate::parser::parse(&data) {
            Ok(c) => c,
            Err(e) => { eprintln!("parse failed: {:?}", e); return; }
        };
        eprintln!("=== Phase B0 diag: trace 0x8C assignment on ModuleScript.luac ===");
        eprintln!("protos={} main={}", chunk.protos.len(), chunk.main_proto);

        // Frequency of 0x8C across the chunk, and per-proto breakdown
        let mut total_8c = 0usize;
        let mut proto_8c: Vec<(usize, usize)> = Vec::new();
        for (pi, p) in chunk.protos.iter().enumerate() {
            let n = p.code.iter().filter(|&&w| insn_op(w) == 0x8C).count();
            total_8c += n;
            if n > 0 { proto_8c.push((pi, n)); }
        }
        eprintln!("total 0x8C occurrences = {}", total_8c);
        eprintln!("top 10 protos by 0x8C count:");
        proto_8c.sort_by(|a, b| b.1.cmp(&a.1));
        for (pi, n) in proto_8c.iter().take(10) {
            let p = &chunk.protos[*pi];
            eprintln!("  proto {} count={} stack={} K={}", pi, n, p.max_stack_size, p.constants.len());
        }

        // LOADN-shape check: for every 0x8C instruction, is A in-range and D a small int?
        let mut loadn_shape = 0usize;
        let mut not_loadn_shape = 0usize;
        for p in &chunk.protos {
            for &w in &p.code {
                if insn_op(w) == 0x8C {
                    let a = insn_a(w);
                    let d = insn_d(w) as i32;
                    if a < p.max_stack_size && d >= -1000 && d <= 10000 {
                        loadn_shape += 1;
                    } else {
                        not_loadn_shape += 1;
                    }
                }
            }
        }
        eprintln!("0x8C LOADN-shape: {} loadn-shaped / {} non-loadn-shaped", loadn_shape, not_loadn_shape);

        // E-format check: does 0x8C look like JUMPX?
        let mut jumpx_valid = 0usize;
        let mut jumpx_total = 0usize;
        let mut jumpx_bigjump = 0usize;
        for p in &chunk.protos {
            for (i, &w) in p.code.iter().enumerate() {
                if insn_op(w) == 0x8C {
                    jumpx_total += 1;
                    let e = insn_e(w);
                    let target = i as i32 + e;
                    if target >= 0 && (target as usize) < p.code.len() {
                        jumpx_valid += 1;
                        if e.abs() > 127 { jumpx_bigjump += 1; }
                    }
                }
            }
        }
        eprintln!("0x8C JUMPX-shape: {} in-range / {} total, {} |E|>127",
            jumpx_valid, jumpx_total, jumpx_bigjump);

        // Now replay detection phase-by-phase and snapshot ctx.map[0x8C] after each.
        // This is an inline copy of detect_with_prior's tier order.
        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        eprintln!("0x8C raw frequency in compute_frequencies = {}", ctx.freq[0x8C]);

        // Seed empty prior
        let mut phase_log: Vec<(&'static str, u8)> = Vec::new();
        let snap = |ctx: &DetectCtx| ctx.map[0x8C];
        let mut last = snap(&ctx);

        let check = |ctx: &DetectCtx, phase: &'static str, last: &mut u8, log: &mut Vec<(&'static str, u8)>| {
            let cur = snap(ctx);
            if cur != *last {
                eprintln!(">>> 0x8C FIRST ASSIGNED in {} -> std={} ({:?})",
                    phase, cur, LuauOpcode::from_u8(cur));
                *last = cur;
                log.push((phase, cur));
            }
        };

        detect_return(&chunk, &mut ctx);          check(&ctx, "detect_return", &mut last, &mut phase_log);
        detect_prepvarargs(&chunk, &mut ctx);     check(&ctx, "detect_prepvarargs", &mut last, &mut phase_log);
        detect_getimport(&chunk, &mut ctx);       check(&ctx, "detect_getimport", &mut last, &mut phase_log);
        detect_closure_capture(&chunk, &mut ctx); check(&ctx, "detect_closure_capture", &mut last, &mut phase_log);
        detect_dupclosure(&chunk, &mut ctx);      check(&ctx, "detect_dupclosure", &mut last, &mut phase_log);
        detect_duptable(&chunk, &mut ctx);        check(&ctx, "detect_duptable", &mut last, &mut phase_log);
        detect_generic_for(&chunk, &mut ctx);     check(&ctx, "detect_generic_for", &mut last, &mut phase_log);
        detect_forgprep_variants(&chunk, &mut ctx); check(&ctx, "detect_forgprep_variants", &mut last, &mut phase_log);
        detect_numeric_for(&chunk, &mut ctx);     check(&ctx, "detect_numeric_for", &mut last, &mut phase_log);
        detect_call(&chunk, &mut ctx);            check(&ctx, "detect_call", &mut last, &mut phase_log);
        detect_namecall(&chunk, &mut ctx);        check(&ctx, "detect_namecall", &mut last, &mut phase_log);
        detect_loadk(&chunk, &mut ctx);           check(&ctx, "detect_loadk", &mut last, &mut phase_log);
        detect_move(&chunk, &mut ctx);            check(&ctx, "detect_move", &mut last, &mut phase_log);
        detect_jump(&chunk, &mut ctx);            check(&ctx, "detect_jump (T3)", &mut last, &mut phase_log);
        detect_table_ops(&chunk, &mut ctx);       check(&ctx, "detect_table_ops", &mut last, &mut phase_log);
        detect_conditional_jumps(&chunk, &mut ctx); check(&ctx, "detect_conditional_jumps", &mut last, &mut phase_log);
        detect_upvalue_ops(&chunk, &mut ctx);     check(&ctx, "detect_upvalue_ops", &mut last, &mut phase_log);
        detect_comparison_jumps_aux(&chunk, &mut ctx); check(&ctx, "detect_comparison_jumps_aux", &mut last, &mut phase_log);
        detect_jumpxeq(&chunk, &mut ctx);         check(&ctx, "detect_jumpxeq", &mut last, &mut phase_log);
        detect_jumpback(&chunk, &mut ctx);        check(&ctx, "detect_jumpback", &mut last, &mut phase_log);
        detect_newtable(&chunk, &mut ctx);        check(&ctx, "detect_newtable", &mut last, &mut phase_log);
        detect_global_ops(&chunk, &mut ctx);      check(&ctx, "detect_global_ops", &mut last, &mut phase_log);
        detect_fastcall(&chunk, &mut ctx);        check(&ctx, "detect_fastcall", &mut last, &mut phase_log);
        detect_fastcall1(&chunk, &mut ctx);       check(&ctx, "detect_fastcall1", &mut last, &mut phase_log);
        detect_fastcall2(&chunk, &mut ctx);       check(&ctx, "detect_fastcall2", &mut last, &mut phase_log);
        detect_fastcall2k(&chunk, &mut ctx);      check(&ctx, "detect_fastcall2k", &mut last, &mut phase_log);
        detect_setlist(&chunk, &mut ctx);         check(&ctx, "detect_setlist", &mut last, &mut phase_log);
        detect_gettablen_settablen(&chunk, &mut ctx); check(&ctx, "detect_gettablen_settablen", &mut last, &mut phase_log);
        detect_gettable_settable(&chunk, &mut ctx); check(&ctx, "detect_gettable_settable", &mut last, &mut phase_log);
        detect_forgprep_variants(&chunk, &mut ctx); check(&ctx, "detect_forgprep_variants (re-run)", &mut last, &mut phase_log);
        detect_loadb(&chunk, &mut ctx);           check(&ctx, "detect_loadb", &mut last, &mut phase_log);
        detect_loadn(&chunk, &mut ctx);           check(&ctx, "detect_loadn", &mut last, &mut phase_log);
        detect_loadnil(&chunk, &mut ctx);         check(&ctx, "detect_loadnil", &mut last, &mut phase_log);
        detect_move(&chunk, &mut ctx);            check(&ctx, "detect_move (re-run)", &mut last, &mut phase_log);
        detect_arith_sequence(&chunk, &mut ctx);  check(&ctx, "detect_arith_sequence", &mut last, &mut phase_log);
        detect_arithmetic(&chunk, &mut ctx);      check(&ctx, "detect_arithmetic", &mut last, &mut phase_log);
        detect_arithmetic_k(&chunk, &mut ctx);    check(&ctx, "detect_arithmetic_k", &mut last, &mut phase_log);
        detect_register_arithmetic(&chunk, &mut ctx); check(&ctx, "detect_register_arithmetic", &mut last, &mut phase_log);
        detect_unary_not_minus(&chunk, &mut ctx); check(&ctx, "detect_unary_not_minus", &mut last, &mut phase_log);
        detect_unary_ops(&chunk, &mut ctx);       check(&ctx, "detect_unary_ops", &mut last, &mut phase_log);
        detect_concat(&chunk, &mut ctx);          check(&ctx, "detect_concat", &mut last, &mut phase_log);
        detect_getvarargs(&chunk, &mut ctx);      check(&ctx, "detect_getvarargs", &mut last, &mut phase_log);
        detect_closeupvals(&chunk, &mut ctx);     check(&ctx, "detect_closeupvals", &mut last, &mut phase_log);
        detect_and_or(&chunk, &mut ctx);          check(&ctx, "detect_and_or", &mut last, &mut phase_log);
        detect_fastcall(&chunk, &mut ctx);        check(&ctx, "detect_fastcall (T6)", &mut last, &mut phase_log);
        detect_fastcall1(&chunk, &mut ctx);       check(&ctx, "detect_fastcall1 (T6)", &mut last, &mut phase_log);
        detect_fastcall2(&chunk, &mut ctx);       check(&ctx, "detect_fastcall2 (T6)", &mut last, &mut phase_log);
        detect_fastcall2k(&chunk, &mut ctx);      check(&ctx, "detect_fastcall2k (T6)", &mut last, &mut phase_log);
        detect_fastcall3(&chunk, &mut ctx);       check(&ctx, "detect_fastcall3", &mut last, &mut phase_log);
        detect_idiv(&chunk, &mut ctx);            check(&ctx, "detect_idiv", &mut last, &mut phase_log);
        detect_idivk(&chunk, &mut ctx);           check(&ctx, "detect_idivk", &mut last, &mut phase_log);
        detect_subrk_divrk(&chunk, &mut ctx);     check(&ctx, "detect_subrk_divrk", &mut last, &mut phase_log);
        detect_loadkx(&chunk, &mut ctx);          check(&ctx, "detect_loadkx", &mut last, &mut phase_log);
        detect_elimination_pass(&chunk, &mut ctx); check(&ctx, "detect_elimination_pass", &mut last, &mut phase_log);

        eprintln!("--- phase 0x8C state after Tier 6 ---");
        eprintln!("ctx.map[0x8C] = {} ({:?})", ctx.map[0x8C], LuauOpcode::from_u8(ctx.map[0x8C]));

        validate_frequency_plausibility(&chunk, &mut ctx); check(&ctx, "validate_frequency_plausibility", &mut last, &mut phase_log);
        validate_aux_alignment(&chunk, &mut ctx); check(&ctx, "validate_aux_alignment", &mut last, &mut phase_log);

        // Don't re-run all the second/third passes; just call detect_frequency_rank_matching and
        // permutation_complete which are the final fallbacks.
        // Simulate the SECOND pass from detect_with_prior (line ~399-456)
        if ctx.find_shuffled(LuauOpcode::Return as u8).is_none() { detect_return(&chunk, &mut ctx); }
        if ctx.find_shuffled(LuauOpcode::Call as u8).is_none() { detect_call(&chunk, &mut ctx); }
        if ctx.find_shuffled(LuauOpcode::NameCall as u8).is_none() { detect_namecall(&chunk, &mut ctx); }
        if ctx.find_shuffled(LuauOpcode::Move as u8).is_none() { detect_move(&chunk, &mut ctx); }
        if ctx.find_shuffled(LuauOpcode::GetTableKS as u8).is_none() || ctx.find_shuffled(LuauOpcode::SetTableKS as u8).is_none() { detect_table_ops(&chunk, &mut ctx); }
        if ctx.find_shuffled(LuauOpcode::GetUpval as u8).is_none() { detect_upvalue_ops(&chunk, &mut ctx); }
        if ctx.find_shuffled(LuauOpcode::Jump as u8).is_none() { detect_jump(&chunk, &mut ctx); }
        check(&ctx, "2nd pass (gated re-runs)", &mut last, &mut phase_log);

        // THIRD PASS from detect_with_prior (line ~458-514): unconditionally re-run
        detect_return(&chunk, &mut ctx);
        detect_prepvarargs(&chunk, &mut ctx);
        detect_getimport(&chunk, &mut ctx);
        detect_closure_capture(&chunk, &mut ctx);
        detect_dupclosure(&chunk, &mut ctx);
        detect_duptable(&chunk, &mut ctx);
        detect_generic_for(&chunk, &mut ctx);
        detect_forgprep_variants(&chunk, &mut ctx);
        detect_numeric_for(&chunk, &mut ctx);
        detect_call(&chunk, &mut ctx);
        detect_namecall(&chunk, &mut ctx);
        detect_loadk(&chunk, &mut ctx);
        detect_jump(&chunk, &mut ctx);            check(&ctx, "3rd pass: detect_jump (unconditional)", &mut last, &mut phase_log);
        detect_table_ops(&chunk, &mut ctx);
        detect_conditional_jumps(&chunk, &mut ctx);
        detect_upvalue_ops(&chunk, &mut ctx);
        detect_newtable(&chunk, &mut ctx);
        detect_global_ops(&chunk, &mut ctx);
        detect_fastcall(&chunk, &mut ctx);
        detect_fastcall1(&chunk, &mut ctx);
        detect_fastcall2(&chunk, &mut ctx);
        detect_fastcall2k(&chunk, &mut ctx);
        detect_setlist(&chunk, &mut ctx);
        detect_gettablen_settablen(&chunk, &mut ctx);
        detect_gettable_settable(&chunk, &mut ctx);
        detect_comparison_jumps_aux(&chunk, &mut ctx);
        detect_jumpxeq(&chunk, &mut ctx);
        detect_jumpback(&chunk, &mut ctx);
        detect_forgprep_variants(&chunk, &mut ctx);
        detect_loadb(&chunk, &mut ctx);
        detect_loadn(&chunk, &mut ctx);           check(&ctx, "3rd pass: detect_loadn", &mut last, &mut phase_log);
        detect_loadnil(&chunk, &mut ctx);
        detect_move(&chunk, &mut ctx);
        detect_arith_sequence(&chunk, &mut ctx);
        detect_arithmetic(&chunk, &mut ctx);
        detect_arithmetic_k(&chunk, &mut ctx);
        detect_register_arithmetic(&chunk, &mut ctx);
        detect_unary_not_minus(&chunk, &mut ctx);
        detect_unary_ops(&chunk, &mut ctx);
        detect_concat(&chunk, &mut ctx);
        detect_getvarargs(&chunk, &mut ctx);
        detect_closeupvals(&chunk, &mut ctx);
        detect_and_or(&chunk, &mut ctx);
        detect_fastcall(&chunk, &mut ctx);
        detect_fastcall1(&chunk, &mut ctx);
        detect_fastcall2(&chunk, &mut ctx);
        detect_fastcall2k(&chunk, &mut ctx);
        detect_fastcall3(&chunk, &mut ctx);
        detect_idiv(&chunk, &mut ctx);
        detect_idivk(&chunk, &mut ctx);
        detect_subrk_divrk(&chunk, &mut ctx);
        detect_loadkx(&chunk, &mut ctx);

        detect_frequency_rank_matching(&chunk, &mut ctx); check(&ctx, "detect_frequency_rank_matching", &mut last, &mut phase_log);
        // Snapshot heuristic map BEFORE permutation_complete
        let heuristic_8c = ctx.map[0x8C];
        permutation_complete(&chunk, &mut ctx);   check(&ctx, "permutation_complete", &mut last, &mut phase_log);
        let final_8c = ctx.map[0x8C];

        eprintln!("--- summary (partial trace, 1st pass only) ---");
        eprintln!("heuristic_map[0x8C] = {} ({:?})", heuristic_8c, LuauOpcode::from_u8(heuristic_8c));
        eprintln!("final_map[0x8C]     = {} ({:?})", final_8c, LuauOpcode::from_u8(final_8c));
        eprintln!("phase log (changes only, 1st pass): {:?}", phase_log);

        // --- Now run the REAL detect_with_prior(chunk, [255;256]) for the true final state ---
        let real = OpcodeMap::detect_with_prior(&chunk, &[255u8; 256]);
        eprintln!("--- full detect_with_prior result ---");
        eprintln!("heuristic_map[0x8C] = {} ({:?})", real.heuristic_map[0x8C], LuauOpcode::from_u8(real.heuristic_map[0x8C]));
        eprintln!("final  map  [0x8C] = {} ({:?})", real.shuffled_to_standard[0x8C], LuauOpcode::from_u8(real.shuffled_to_standard[0x8C]));
        let loadn_byte = (0..=255u8).find(|&b| real.shuffled_to_standard[b as usize] == LuauOpcode::LoadN as u8);
        let jumpx_byte = (0..=255u8).find(|&b| real.shuffled_to_standard[b as usize] == LuauOpcode::JumpX as u8);
        eprintln!("LoadN mapped to byte = {:?}", loadn_byte.map(|b| format!("0x{:02X}", b)));
        eprintln!("JumpX mapped to byte = {:?}", jumpx_byte.map(|b| format!("0x{:02X}", b)));
        if let Some(b) = loadn_byte {
            eprintln!("raw freq of LoadN-byte 0x{:02X} = {}", b, ctx.freq[b as usize]);
        }
        if let Some(b) = jumpx_byte {
            eprintln!("raw freq of JumpX-byte 0x{:02X} = {}", b, ctx.freq[b as usize]);
        }

        // Replay detect_jump's JumpX sublogic on the SAME ctx state that existed
        // right after validate_frequency_plausibility unassigned 0x8C — confirms
        // the sublogic is what re-claims 0x8C for JumpX in the 3rd pass.
        eprintln!("--- JumpX sublogic replay at the 3rd-pass state ---");
        let mut jx_hits: Vec<(u8, usize, usize)> = Vec::new(); // byte, valid_big_E, total_big_E
        for proto in &chunk.protos {
            for (i, &insn) in proto.code.iter().enumerate() {
                let op = insn_op(insn);
                if real.shuffled_to_standard[op as usize] != 255 { continue; }
                let e = (insn >> 8) as i32;
                let e_signed = if e >= (1 << 23) { e - (1 << 24) } else { e };
                let target = i as i32 + e_signed;
                let in_range = target >= 0 && (target as usize) < proto.code.len();
                let big = e_signed.abs() > 127;
                if in_range && big {
                    if let Some(entry) = jx_hits.iter_mut().find(|(b, _, _)| *b == op) {
                        entry.1 += 1;
                        entry.2 += 1;
                    } else {
                        jx_hits.push((op, 1, 1));
                    }
                }
            }
        }
        jx_hits.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        eprintln!("unmapped bytes ranked by JumpX-sublogic hits (post-3rd-pass unmapped set):");
        for (b, valid, _total) in jx_hits.iter().take(10) {
            eprintln!("  0x{:02X}: valid_JumpX_hits={} raw_freq={}", b, valid, ctx.freq[*b as usize]);
        }
    }

    /// Phase B0.1 diag: trace `detect_conditional_jumps` candidate list at the
    /// moment it runs on ModuleScript.luac (after Phase B0 Patch 4a + 4b stopped
    /// the JumpX claim). For each unmapped candidate byte, print:
    ///   - raw chunk frequency
    ///   - "conditional-shape" hit count (a>0, a<max_stack, d>0, target in range)
    ///   - distinct D-value count (LOADN literals cluster, jumps spread)
    ///   - backward-D count (jumps have backward branches, LOADN literals have
    ///     negative numbers but are shaped differently)
    ///   - fraction of targets landing on known-AUX positions (real jumps = 0%)
    ///
    /// Invoke:
    ///   cargo test --release -p luau-core --lib -- --ignored diag_phase_b01_trace_condjump_0x8c --nocapture
    #[test]
    #[ignore]
    fn diag_phase_b01_trace_condjump_0x8c() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent().unwrap()
            .parent().unwrap()
            .join("inspect/ModuleScript.luac");
        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(e) => { eprintln!("cannot read {}: {}", path.display(), e); return; }
        };
        let chunk = match crate::parser::parse(&data) {
            Ok(c) => c,
            Err(e) => { eprintln!("parse failed: {:?}", e); return; }
        };
        eprintln!("=== Phase B0.1 diag: detect_conditional_jumps candidate list on ModuleScript.luac ===");
        eprintln!("protos={} total_insns={}", chunk.protos.len(),
            chunk.protos.iter().map(|p| p.code.len()).sum::<usize>());

        // Run every detector that fires BEFORE detect_conditional_jumps in Tier 3.
        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_return(&chunk, &mut ctx);
        detect_prepvarargs(&chunk, &mut ctx);
        detect_getimport(&chunk, &mut ctx);
        detect_closure_capture(&chunk, &mut ctx);
        detect_dupclosure(&chunk, &mut ctx);
        detect_duptable(&chunk, &mut ctx);
        detect_generic_for(&chunk, &mut ctx);
        detect_forgprep_variants(&chunk, &mut ctx);
        detect_numeric_for(&chunk, &mut ctx);
        detect_call(&chunk, &mut ctx);
        detect_namecall(&chunk, &mut ctx);
        detect_loadk(&chunk, &mut ctx);
        detect_move(&chunk, &mut ctx);
        detect_jump(&chunk, &mut ctx);
        detect_table_ops(&chunk, &mut ctx);

        let total_insns: u32 = ctx.total_insns;
        eprintln!("total_insns (from ctx) = {}", total_insns);
        eprintln!("ctx.map[0x8C] BEFORE detect_conditional_jumps = {} ({:?})",
            ctx.map[0x8C], LuauOpcode::from_u8(ctx.map[0x8C]));

        // Build an "aux positions" grid for known-AUX mapped ops.
        let mut aux_positions: Vec<Vec<bool>> = Vec::with_capacity(chunk.protos.len());
        for proto in &chunk.protos {
            let mut aux = vec![false; proto.code.len()];
            let mut i = 0usize;
            while i < proto.code.len() {
                let op = insn_op(proto.code[i]);
                let mapped = ctx.map[op as usize];
                if mapped != 255 {
                    let std_op = LuauOpcode::from_u8(mapped);
                    if std_op.has_aux() && i + 1 < proto.code.len() {
                        aux[i + 1] = true;
                        i += 2;
                        continue;
                    }
                }
                i += 1;
            }
            aux_positions.push(aux);
        }

        // For each unmapped byte, compute: (conditional_hits, raw_freq,
        // distinct_D, backward_D_count, aux_landings)
        #[derive(Default, Clone)]
        struct CandStats {
            cond_hits: usize,
            raw_freq: u32,
            distinct_d: std::collections::HashSet<i32>,
            backward_d: usize,
            aux_landings: usize,
            forward_d_min: i32,
            forward_d_max: i32,
        }
        let mut stats: std::collections::HashMap<u8, CandStats> = std::collections::HashMap::new();
        for (proto_idx, proto) in chunk.protos.iter().enumerate() {
            let aux = &aux_positions[proto_idx];
            for (i, &insn) in proto.code.iter().enumerate() {
                let op = insn_op(insn);
                if ctx.is_mapped(op) { continue; }
                let a = insn_a(insn);
                let d = insn_d(insn) as i32;
                let target = i as i32 + d;
                let e = stats.entry(op).or_default();
                e.raw_freq = ctx.freq[op as usize];
                // Conditional-jump filter from detect_conditional_jumps
                if a > 0 && a < proto.max_stack_size && d > 0
                    && target >= 0 && (target as usize) < proto.code.len()
                {
                    e.cond_hits += 1;
                    e.distinct_d.insert(d);
                    if e.forward_d_max == 0 || d > e.forward_d_max { e.forward_d_max = d; }
                    if e.forward_d_min == 0 || d < e.forward_d_min { e.forward_d_min = d; }
                    if (target as usize) < aux.len() && aux[target as usize] {
                        e.aux_landings += 1;
                    }
                }
                if a > 0 && a < proto.max_stack_size && d < 0
                    && (i as i32 + d) >= 0
                {
                    e.backward_d += 1;
                }
            }
        }
        // Sort by conditional_hits desc, byte asc
        let mut sorted: Vec<(u8, CandStats)> = stats.into_iter()
            .filter(|(_, s)| s.cond_hits >= 3)
            .collect();
        sorted.sort_by(|a, b| b.1.cond_hits.cmp(&a.1.cond_hits).then_with(|| a.0.cmp(&b.0)));
        eprintln!("top 15 candidates for detect_conditional_jumps (by cond_hits):");
        eprintln!("  byte | cond_hits | raw_freq | raw_freq_% | distinct_D | backward_D | aux_lands | fwd_D_range | distinct_D_%");
        for (op, s) in sorted.iter().take(15) {
            let pct = (s.raw_freq as f64 / total_insns as f64) * 100.0;
            let distinct_pct = if s.cond_hits == 0 { 0.0 } else {
                (s.distinct_d.len() as f64 / s.cond_hits as f64) * 100.0
            };
            eprintln!("  0x{:02X} | {:9} | {:8} | {:9.2}% | {:10} | {:10} | {:9} | [{}, {}] | {:.1}%",
                op, s.cond_hits, s.raw_freq, pct,
                s.distinct_d.len(), s.backward_d, s.aux_landings,
                s.forward_d_min, s.forward_d_max, distinct_pct);
        }
    }

    /// Phase B0.2 diag: trace detect_jumpback candidates on ModuleScript.luac.
    /// Expected finding: 0x8C (which should be LoadN) wins detect_jumpback because
    /// its ~15-40 negative-D LOADN instances out of 735 trivially pass the
    /// `d<0 && target_in_bounds` filter. The REAL JumpBack byte should have ~10-30
    /// negative-D hits. Without a raw-frequency cap, the max-count win is 0x8C.
    #[test]
    #[ignore]
    fn diag_phase_b02_trace_jumpback_0x8c() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent().unwrap()
            .parent().unwrap()
            .join("inspect/ModuleScript.luac");
        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(e) => { eprintln!("cannot read {}: {}", path.display(), e); return; }
        };
        let chunk = match crate::parser::parse(&data) {
            Ok(c) => c,
            Err(e) => { eprintln!("parse failed: {:?}", e); return; }
        };
        eprintln!("=== Phase B0.2 diag: detect_jumpback candidate list on ModuleScript.luac ===");
        let total_insns: usize = chunk.protos.iter().map(|p| p.code.len()).sum();
        eprintln!("protos={} total_insns={}", chunk.protos.len(), total_insns);

        // Run every detector that fires BEFORE detect_jumpback in Tier 4.
        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_return(&chunk, &mut ctx);
        detect_prepvarargs(&chunk, &mut ctx);
        detect_getimport(&chunk, &mut ctx);
        detect_closure_capture(&chunk, &mut ctx);
        detect_dupclosure(&chunk, &mut ctx);
        detect_duptable(&chunk, &mut ctx);
        detect_generic_for(&chunk, &mut ctx);
        detect_forgprep_variants(&chunk, &mut ctx);
        detect_numeric_for(&chunk, &mut ctx);
        detect_call(&chunk, &mut ctx);
        detect_namecall(&chunk, &mut ctx);
        detect_loadk(&chunk, &mut ctx);
        detect_move(&chunk, &mut ctx);
        detect_jump(&chunk, &mut ctx);
        detect_table_ops(&chunk, &mut ctx);
        detect_conditional_jumps(&chunk, &mut ctx);
        detect_upvalue_ops(&chunk, &mut ctx);
        detect_comparison_jumps_aux(&chunk, &mut ctx);
        detect_jumpxeq(&chunk, &mut ctx);

        eprintln!("ctx.map[0x8C] BEFORE detect_jumpback = {} ({:?})",
            ctx.map[0x8C], LuauOpcode::from_u8(ctx.map[0x8C]));
        eprintln!("ctx.freq[0x8C] = {}", ctx.freq[0x8C]);

        // Mirror detect_jumpback's candidate-build loop: d < 0 && target in bounds.
        #[derive(Default, Clone)]
        struct JbCandStats {
            backward_hits: usize,  // matches detect_jumpback filter
            raw_freq: u32,
            total_d_neg: usize,    // count of any d<0 regardless of target check
            total_d_pos: usize,    // positive d (LoadN literals)
            total_d_zero: usize,   // d=0
        }
        let mut stats: std::collections::HashMap<u8, JbCandStats> = std::collections::HashMap::new();
        for proto in &chunk.protos {
            let code_len = proto.code.len() as i32;
            for (i, &insn) in proto.code.iter().enumerate() {
                let op = insn_op(insn);
                if ctx.is_mapped(op) { continue; }
                let d = insn_d(insn) as i32;
                let target = i as i32 + d + 1;
                let e = stats.entry(op).or_default();
                e.raw_freq = ctx.freq[op as usize];
                match d.cmp(&0) {
                    std::cmp::Ordering::Less => e.total_d_neg += 1,
                    std::cmp::Ordering::Equal => e.total_d_zero += 1,
                    std::cmp::Ordering::Greater => e.total_d_pos += 1,
                }
                if d < 0 && target >= 0 && target < code_len {
                    e.backward_hits += 1;
                }
            }
        }
        // Sort by backward_hits desc, byte asc
        let mut sorted: Vec<(u8, JbCandStats)> = stats.into_iter()
            .filter(|(_, s)| s.backward_hits >= 1)
            .collect();
        sorted.sort_by(|a, b| b.1.backward_hits.cmp(&a.1.backward_hits).then_with(|| a.0.cmp(&b.0)));
        eprintln!("top 15 candidates for detect_jumpback (by backward_hits):");
        eprintln!("  byte | bwd_hits | raw_freq | raw_freq_% | d_neg | d_zero | d_pos | neg_pct_of_freq");
        for (op, s) in sorted.iter().take(15) {
            let pct = (s.raw_freq as f64 / total_insns as f64) * 100.0;
            let neg_pct = if s.raw_freq == 0 { 0.0 } else {
                (s.total_d_neg as f64 / s.raw_freq as f64) * 100.0
            };
            eprintln!("  0x{:02X} | {:8} | {:8} | {:9.2}% | {:5} | {:6} | {:5} | {:.2}%",
                op, s.backward_hits, s.raw_freq, pct,
                s.total_d_neg, s.total_d_zero, s.total_d_pos, neg_pct);
        }
    }

    // ── Phase B0.12: detect_loadkx regression tests ──────────────────────────

    /// Build a chunk whose first proto has > 32768 constants and N LoadKX instructions.
    /// LOADK must already be known (seeded with loadk_shuffled byte → canonical 5).
    /// All instructions are padded so freq[loadkx_shuffled] ≤ N / (total_words / 50)
    /// unless `inflate_freq` is set.
    fn build_loadkx_chunk(
        loadk_shuffled: u8,
        loadkx_shuffled: u8,
        loadkx_count: usize,
        extra_non_loadkx_words: usize,
    ) -> Chunk {
        use crate::parser::types::Constant;
        // Phase B0.16: use a modest constant table (500 entries).
        // detect_loadkx no longer requires > 32768 constants — D=0 purity is the discriminator.
        let const_count = 500usize;
        let mut constants: Vec<Constant> = Vec::with_capacity(const_count);
        for _ in 0..const_count {
            constants.push(Constant::Nil);
        }

        let mut code: Vec<u32> = Vec::new();

        // Seed LOADK instructions to allow detect_loadkx prereq check
        for _ in 0..5 {
            code.push((loadk_shuffled as u32) | (0u32 << 8));
        }

        // Add LoadKX instructions: op=loadkx_shuffled, A=1, D=0, AUX=valid const index.
        // The AUX word's low byte (bits 0-7) must be an already-mapped op byte (loadk_shuffled)
        // so that the detect_loadkx loop skips it when scanning forward. This avoids the
        // false-positive where AUX words at position i+1 create spurious D=0 candidates.
        // The full AUX u32 is treated as a constant index: loadk_shuffled | (idx << 8).
        // As long as this value < const_count (500) it is a valid index.
        // loadk_shuffled = 0x6F = 111. For idx=0: aux = 111. For idx=1: aux = 111 + 256 = 367 < 500.
        for i in 0..loadkx_count {
            code.push((loadkx_shuffled as u32) | (1u32 << 8)); // A=1, D=0
            let const_idx = i % 2; // 0 or 1, keeps aux_u = 0x6F or 0x16F = 111 or 367 (< 500)
            let aux_word = (loadk_shuffled as u32) | ((const_idx as u32) << 8);
            code.push(aux_word); // aux_u = aux_word < 500 (valid); op=loadk_shuffled (mapped → skip)
        }

        // Pad with filler words that have D≠0 to demonstrate they fail purity check.
        for i in 0..extra_non_loadkx_words {
            let filler_op: u8 = if loadkx_shuffled != 0xEE { 0xEE } else { 0xEF };
            // Filler instructions: alternate D=0 and D=1 to NOT be pure D=0
            let d_field: u32 = if i % 2 == 0 { 0 } else { 1u32 << 16 };
            code.push((filler_op as u32) | d_field);
        }

        Chunk {
            version: 6,
            types_version: 0,
            strings: Vec::new(),
            protos: vec![crate::parser::types::Proto {
                max_stack_size: 16,
                num_params: 0,
                num_upvalues: 0,
                is_vararg: false,
                flags: 0,
                typeinfo: None,
                code,
                constants,
                child_protos: Vec::new(),
                line_defined: 0,
                debug_name: None,
                line_info: None,
                debug_info: None,
            }],
            main_proto: 0,
        }
    }

    /// B0.12 fix 1: count >= 1 threshold — single LoadKX instruction is detected.
    #[test]
    fn b12_detect_loadkx_single_occurrence_detected() {
        let loadk_byte: u8 = 0x6F; // arbitrary LOADK shuffled byte
        let loadkx_byte: u8 = 0xA7; // arbitrary unmapped byte for LOADKX
        let chunk = build_loadkx_chunk(loadk_byte, loadkx_byte, 1, 200);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        // Seed LOADK as prerequisite
        ctx.try_assign_force(loadk_byte, LuauOpcode::LoadK as u8);

        detect_loadkx(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[loadkx_byte as usize],
            LuauOpcode::LoadKX as u8,
            "detect_loadkx failed to map single-occurrence LoadKX byte 0x{:02X}", loadkx_byte
        );
    }

    /// B0.12 fix 2: try_assign_force bypasses frequency guard.
    /// Construct scenario where freq[loadkx_byte] >> 2% of total_insns.
    #[test]
    fn b12_detect_loadkx_high_frequency_still_detected() {
        let loadk_byte: u8 = 0x6F;
        let loadkx_byte: u8 = 0xA7;
        // 50 LoadKX = 100 code words (insn + AUX each).
        // Only 10 extra filler words → total = 5 (loadk) + 100 (loadkx+aux) + 10 = 115
        // freq[loadkx_byte] = 50. total_insns = 115. threshold = 115/50 = 2.
        // 50 > 2 → try_assign would BLOCK, but try_assign_force bypasses it.
        let chunk = build_loadkx_chunk(loadk_byte, loadkx_byte, 50, 10);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        ctx.try_assign_force(loadk_byte, LuauOpcode::LoadK as u8);

        detect_loadkx(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[loadkx_byte as usize],
            LuauOpcode::LoadKX as u8,
            "detect_loadkx blocked by frequency guard (should use try_assign_force)"
        );
    }

    /// B0.16 purity check: a candidate byte with MIXED D values (some D=0, some D≠0)
    /// must NOT be detected as LoadKX. LOADKX always has D=0; any byte that uses D
    /// even once is not LOADKX.
    #[test]
    fn b12_detect_loadkx_impure_d_not_detected() {
        use crate::parser::types::Constant;
        let loadk_byte: u8 = 0x6F;
        let impure_byte: u8 = 0xA7; // will have mixed D values → not pure → not LoadKX
        let real_kx_byte: u8 = 0xB3; // pure D=0 → real LoadKX
        let constants: Vec<Constant> = (0..200).map(|_| Constant::Nil).collect();
        let mut code: Vec<u32> = Vec::new();
        // Seed LOADK
        for _ in 0..5 { code.push(loadk_byte as u32); }
        // Impure byte: 3 with D=0 (valid AUX), 2 with D≠0 → purity check fails
        for j in 0..5 {
            let d_bits: u32 = if j < 3 { 0 } else { 1u32 << 16 }; // D=0 for first 3, D=1 for last 2
            code.push((impure_byte as u32) | (1u32 << 8) | d_bits);
            code.push(50u32); // AUX = valid index 50 < 200
        }
        // Real LoadKX byte: all D=0, valid AUX
        for k in 0..3 {
            code.push((real_kx_byte as u32) | (2u32 << 8)); // A=2, D=0
            code.push((80 + k) as u32); // AUX valid
        }
        let chunk = Chunk {
            version: 6, types_version: 0, strings: Vec::new(),
            protos: vec![crate::parser::types::Proto {
                max_stack_size: 16, num_params: 0, num_upvalues: 0,
                is_vararg: false, flags: 0, typeinfo: None,
                code, constants, child_protos: Vec::new(),
                line_defined: 0, debug_name: None, line_info: None, debug_info: None,
            }],
            main_proto: 0,
        };
        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        ctx.try_assign_force(loadk_byte, LuauOpcode::LoadK as u8);
        detect_loadkx(&chunk, &mut ctx);
        assert_ne!(
            ctx.map[impure_byte as usize],
            LuauOpcode::LoadKX as u8,
            "detect_loadkx must NOT map impure-D byte 0x{:02X} to LoadKX", impure_byte
        );
        assert_eq!(
            ctx.map[real_kx_byte as usize],
            LuauOpcode::LoadKX as u8,
            "detect_loadkx must map pure-D=0 byte 0x{:02X} to LoadKX", real_kx_byte
        );
    }

    /// B0.16 gate: a candidate byte with D=0 but AUX values ALL out-of-range
    /// (>= constants.len()) must NOT be detected as LoadKX. The AUX validity check
    /// (aux < constants.len()) is required even when D=0 purity holds.
    #[test]
    fn b12_detect_loadkx_out_of_range_aux_not_detected() {
        use crate::parser::types::Constant;
        let loadk_byte: u8 = 0x6F;
        let candidate_byte: u8 = 0xA7;
        // Proto with 100 constants; candidate byte has D=0 but AUX = 200 (>= 100 = out of range)
        let constants: Vec<Constant> = (0..100).map(|_| Constant::Nil).collect();
        let mut code: Vec<u32> = Vec::new();
        for _ in 0..5 { code.push(loadk_byte as u32); }
        for _ in 0..5 {
            code.push((candidate_byte as u32) | (1u32 << 8)); // A=1, D=0
            code.push(200u32); // AUX = 200 >= 100 = out of range → NOT a valid LOADKX hit
        }
        let chunk = Chunk {
            version: 6, types_version: 0, strings: Vec::new(),
            protos: vec![crate::parser::types::Proto {
                max_stack_size: 16, num_params: 0, num_upvalues: 0,
                is_vararg: false, flags: 0, typeinfo: None,
                code, constants, child_protos: Vec::new(),
                line_defined: 0, debug_name: None, line_info: None, debug_info: None,
            }],
            main_proto: 0,
        };
        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        ctx.try_assign_force(loadk_byte, LuauOpcode::LoadK as u8);
        detect_loadkx(&chunk, &mut ctx);
        assert_ne!(
            ctx.map[candidate_byte as usize],
            LuauOpcode::LoadKX as u8,
            "detect_loadkx must not fire when all AUX values are out-of-range (>= constants.len())"
        );
    }

    /// B0.17: AUX-aware scan prevents GetGlobal AUX words from poisoning LoadKX purity.
    ///
    /// Scenario:
    ///   - GetGlobal (mapped, has AUX) at position i; AUX string index = 193 = 0x000000C1
    ///     → AUX word's low byte = 0xC1 = loadkx_byte. D=0 (bits 16-31 = 0). A=0.
    ///     → WITHOUT the AUX-aware fix: this AUX word appears at i+1 as an "instruction"
    ///       with op=0xC1. The next word (i+2) is a filler instruction with D≠0, so
    ///       d0_valid[0xC1] is NOT incremented but total[0xC1] IS → purity breaks → not detected.
    ///     → WITH the AUX-aware fix: GetGlobal is mapped+has_aux → skip_next=true → i+1 skipped.
    ///   - Real LoadKX (0xC1) occurrences: D=0, A valid, AUX valid const index → pure.
    ///   - Expected: detect_loadkx correctly assigns 0xC1 to LoadKX.
    #[test]
    fn b17_detect_loadkx_aux_aware_scan_not_poisoned() {
        use crate::parser::types::Constant;
        let loadk_byte: u8 = 0x6F;   // LOADK shuffled byte
        let getglobal_byte: u8 = 0x22; // GetGlobal shuffled byte (will be mapped)
        let loadkx_byte: u8 = 0xC1;  // LoadKX shuffled byte (the target we want to detect)

        let const_count = 300usize;
        let constants: Vec<Constant> = (0..const_count).map(|_| Constant::Nil).collect();
        let mut code: Vec<u32> = Vec::new();

        // Seed LOADK (required prereq for detect_loadkx)
        for _ in 0..5 {
            code.push(loadk_byte as u32);
        }

        // Filler instruction (unmapped, D≠0): acts as the word AFTER the GetGlobal AUX.
        // When the AUX word (string_idx=193, op=0xC1) is processed WITHOUT the fix, the
        // "AUX" it reads is code[i+2] = this filler. Filler has D=1 (bits 16-31 ≠ 0)
        // and A=10 (< max_stack_size=32). aux_u = code[i+3] = large → out of range.
        // So: D≠0 → the `d0_valid` check fails → total[0xC1] increases but not d0_valid[0xC1].
        // Purity breaks WITHOUT the fix.
        let filler_d1: u32 = 0xDD | (1u32 << 16); // op=0xDD, D=1

        // Place GetGlobal+AUX pairs where the AUX has string_index = 193 = 0xC1.
        // GetGlobal AUX format: just a string index (1-based), stored as u32.
        // string_index = 193 → AUX word = 0x000000C1 → low byte = 0xC1, D=0.
        for _ in 0..10 {
            let getglobal_insn = getglobal_byte as u32 | (5u32 << 8); // A=5, D=0
            let aux_string_idx: u32 = 193; // 0x000000C1 — low byte = loadkx_byte!
            code.push(getglobal_insn);  // mapped, has_aux → next word is AUX data
            code.push(aux_string_idx);  // AUX: string index 193
            code.push(filler_d1);       // word after AUX — D≠0, will be seen as instruction
        }

        // Real LoadKX instructions: 9 occurrences with D=0, A=2, valid const index.
        for k in 0..9usize {
            code.push((loadkx_byte as u32) | (2u32 << 8)); // op=0xC1, A=2, D=0
            let const_idx = 50 + k; // valid: 50..58 < 300
            code.push(const_idx as u32); // AUX = const_idx (low byte ≠ 0xC1 for k<199)
        }

        let chunk = Chunk {
            version: 6,
            types_version: 0,
            strings: Vec::new(),
            protos: vec![crate::parser::types::Proto {
                max_stack_size: 32,
                num_params: 0,
                num_upvalues: 0,
                is_vararg: false,
                flags: 0,
                typeinfo: None,
                code,
                constants,
                child_protos: Vec::new(),
                line_defined: 0,
                debug_name: None,
                line_info: None,
                debug_info: None,
            }],
            main_proto: 0,
        };

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        ctx.try_assign_force(loadk_byte, LuauOpcode::LoadK as u8);
        ctx.try_assign_force(getglobal_byte, LuauOpcode::GetGlobal as u8);

        detect_loadkx(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[loadkx_byte as usize],
            LuauOpcode::LoadKX as u8,
            "detect_loadkx must assign 0x{:02X} to LoadKX even when GetGlobal AUX words \
             have low byte = 0xC1 — AUX-aware scan should skip them",
            loadkx_byte
        );
    }

    /// Keystone proof for Phase B0.13: given ForGPrepINext (0x64) already mapped,
    /// detect_forgloopinext must identify the paired ForGLoopINext byte (0x35)
    /// and assign it to canonical 61 (Deprecated61).
    ///
    /// Synthetic chunk layout — ipairs-style generic-for loop (no AUX word on loop-back):
    ///   PC 0: FORGPREP_INEXT (0x64) A=0 D=+2  → target = 0+2+1 = 3
    ///   PC 1: body filler (0xAA)
    ///   PC 2: body filler (0xBB)
    ///   PC 3: FORGLOOP_INEXT (0x35) A=0 D=-3  → back = 3+(-3)+1 = 1 (body start)
    ///
    /// Verification:
    ///   target_a (0) == prep_a (0)          ✓
    /// Phase B0.15 model: ForGLoopINext sits at the TOP of the loop body (the jump
    /// target of ForGPrepINext). D is a FORWARD EXIT offset (unsigned 16-bit) pointing
    /// to the instruction AFTER the loop. The body falls through from ForGLoopINext
    /// when the iterator is valid, and exits by D_u16 forward when exhausted.
    ///
    /// Layout used in this test:
    ///   PC 0: ForGPrepINext A=0, D=+2  →  jumps to ForGLoopINext at PC=3
    ///   PC 1: (body instruction 1 — only reached by loop-back from end of body)
    ///   PC 2: (body instruction 2)
    ///   PC 3: ForGLoopINext A=0, D=+3  →  exit to PC=3+3+1=7 when done; fall through to PC=4 when valid
    ///   PC 4: (body instruction 3 — only reached by falling through from ForGLoopINext)
    ///   PC 5: (body instruction 4)
    ///   PC 6: JUMPBACK to PC=3 (ForGLoopINext) at end of body
    ///   PC 7: post-loop (exit target: PC = 3 + 3 + 1 = 7)
    ///
    /// Real Animate.lua data: D_signed=-4352 at PC=2, max_stack=7
    ///   D_u16 = 61184  →  exit at PC=2+61184+1=61187 (huge function ✓)
    #[test]
    fn detect_forgloopinext_keystone_proof() {
        // These two bytes are the actual shuffled bytes observed in Roblox game scripts
        // (Animate.lua diagnostic: 0x64→canonical 60 confirmed, 0x35 unresolved).
        let forgprep_inext_byte: u8 = 0x64;
        let forgloopinext_byte: u8 = 0x35;

        // Build the synthetic ipairs loop using the FORWARD-EXIT model (Phase B0.15).
        // ForGLoopINext is at the TOP of the body; D is an unsigned forward exit offset.
        let code = vec![
            insn_ad(forgprep_inext_byte, 0, 2),  // PC 0: ForGPrepINext, A=0, D=+2 → jumps to PC=3
            insn_abc(0xAA, 1, 0, 0),              // PC 1: body filler (reached on loop-back)
            insn_abc(0xBB, 1, 1, 0),              // PC 2: body filler
            insn_ad(forgloopinext_byte, 0, 3),    // PC 3: ForGLoopINext, A=0, D=+3 (exit to PC=7)
            insn_abc(0xAA, 1, 0, 0),              // PC 4: body (fall-through from ForGLoopINext)
            insn_abc(0xBB, 1, 1, 0),              // PC 5: body
            insn_abc(0xDD, 0, 0, 0),              // PC 6: JUMPBACK to PC=3
            insn_abc(0xCC, 0, 0, 0),              // PC 7: post-loop (exit target = 3+3+1=7)
        ];
        let chunk = chunk_from_code(code, 4);

        // Pre-seed ForGPrepINext (canonical 60) as already mapped.
        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        ctx.try_assign_force(forgprep_inext_byte, LuauOpcode::ForGPrepINext as u8);

        // The keystone call.
        detect_forgloopinext(&chunk, &mut ctx);

        // Keystone assertion: 0x35 must now be mapped to canonical 61 (Deprecated61).
        assert_eq!(
            ctx.map[forgloopinext_byte as usize],
            LuauOpcode::Deprecated61 as u8,
            "KEYSTONE FAILURE: detect_forgloopinext did not map 0x{:02X} to canonical 61 (Deprecated61); \
             got canonical {} instead",
            forgloopinext_byte,
            ctx.map[forgloopinext_byte as usize]
        );
        assert!(
            ctx.assigned[LuauOpcode::Deprecated61 as usize],
            "KEYSTONE FAILURE: Deprecated61 not marked as assigned after detect_forgloopinext"
        );
        // ForGPrepINext must remain stable.
        assert_eq!(
            ctx.map[forgprep_inext_byte as usize],
            LuauOpcode::ForGPrepINext as u8,
            "detect_forgloopinext must not disturb the pre-mapped ForGPrepINext byte"
        );
    }

    /// Additional keystone: real-corpus D_signed=-4352 (D_unsigned=61184) pattern.
    /// Animate.lua has ForGLoopINext at PC=2 in a ~65K-instruction proto.
    /// D_u16=61184, exit_target=2+61184+1=61187.
    #[test]
    fn detect_forgloopinext_large_unsigned_d() {
        let forgprep_inext_byte: u8 = 0x64;
        let forgloopinext_byte: u8 = 0x35;

        // Simulate a large proto: ForGPrepINext at PC=0, D=1 → ForGLoopINext at PC=2.
        // ForGLoopINext has D_signed=-4352, which means D_unsigned=61184.
        // Proto needs at least 2+61184+1=61187 instructions.
        // We create a proto of exactly 61187+1 instructions (smaller would reject).
        let total = 61187 + 1; // exit_target=61187 ≤ total=61188 ✓
        let mut code = vec![0u32; total];
        code[0] = insn_ad(forgprep_inext_byte, 2, 1); // ForGPrepINext, A=2, D=1 → target=2
        // PC=1: body filler
        code[1] = insn_abc(0xAA, 1, 0, 0);
        // PC=2: ForGLoopINext, A=2, D_signed=-4352 (= D_unsigned=61184, exit=61187)
        code[2] = insn_ad(forgloopinext_byte, 2, -4352i16);
        // rest: body filler

        let chunk = chunk_from_code(code, 4);
        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        ctx.try_assign_force(forgprep_inext_byte, LuauOpcode::ForGPrepINext as u8);

        detect_forgloopinext(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[forgloopinext_byte as usize],
            LuauOpcode::Deprecated61 as u8,
            "detect_forgloopinext failed for large D_unsigned corpus pattern"
        );
    }

    /// Robustness: detect_forgloopinext correctly rejects bytes that do NOT form
    /// a valid FORGPREP_INEXT → FORGLOOP_INEXT structural pair (wrong A register).
    #[test]
    fn detect_forgloopinext_rejects_mismatched_byte() {
        let forgprep_inext_byte: u8 = 0x64;
        let bad_byte: u8 = 0x77; // A different byte that does NOT form a valid pair

        // Layout: FORGPREP_INEXT at PC=0 points to PC=3.
        // The instruction at PC=3 has a DIFFERENT A register (A=1 ≠ prep A=0).
        let code = vec![
            insn_ad(forgprep_inext_byte, 0, 2),  // PC 0: ForGPrepINext A=0 D=+2
            insn_abc(0xAA, 1, 0, 0),              // PC 1: body filler
            insn_abc(0xBB, 1, 1, 0),              // PC 2: body filler
            insn_ad(bad_byte, 1, 3),              // PC 3: A=1 ≠ 0, should be rejected (exit=PC7)
            insn_abc(0xAA, 1, 0, 0),              // PC 4: body
            insn_abc(0xBB, 1, 1, 0),              // PC 5: body
            insn_abc(0xDD, 0, 0, 0),              // PC 6: loop-back
            insn_abc(0xCC, 0, 0, 0),              // PC 7: post-loop
        ];
        let chunk = chunk_from_code(code, 4);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        ctx.try_assign_force(forgprep_inext_byte, LuauOpcode::ForGPrepINext as u8);

        detect_forgloopinext(&chunk, &mut ctx);

        // bad_byte must NOT be mapped to Deprecated61 (A mismatch).
        assert_ne!(
            ctx.map[bad_byte as usize],
            LuauOpcode::Deprecated61 as u8,
            "detect_forgloopinext wrongly mapped 0x{:02X} despite A-register mismatch", bad_byte
        );
        assert!(
            !ctx.assigned[LuauOpcode::Deprecated61 as usize],
            "Deprecated61 must not be marked assigned after rejecting mismatched candidate"
        );
    }

    /// Phase B0.18 diagnostic: trace why detect_forgloopinext and detect_loadkx
    /// fail to assign 0x35→61 and 0xC1→66 when processing Animate.lua with the
    /// 81-mapping prior.
    ///
    /// For detect_forgloopinext: walks every proto looking for ForGPrepINext (0x64)
    /// instructions and prints their D field, target PC, and target opcode byte.
    ///
    /// For detect_loadkx: traces total_appearances and d0_valid for 0xC1,
    /// printing each occurrence with d, a, aux_u, const_len.
    ///
    /// Invoke:
    ///   cargo test --release -p luau-core --lib -- --ignored diag_b18_animate_detection_trace --nocapture
    #[test]
    #[ignore]
    fn diag_b18_animate_detection_trace() {
        // 81-entry prior from opmap_cache.json (256 entries, positions 0-255)
        let prior256: [u8; 256] = [
            11, 76, 3, 2, 45, 46, 64, 4, 255, 10, 47, 9, 18, 80, 52, 255,
            42, 255, 70, 39, 255, 41, 255, 58, 55, 255, 255, 255, 73, 255, 255, 255,
            255, 78, 48, 255, 255, 255, 49, 255, 255, 74, 255, 23, 255, 255, 255, 255,
            15, 255, 255, 255, 68, 255, 255, 255, 255, 13, 255, 255, 255, 255, 44, 255,
            255, 255, 255, 36, 255, 255, 255, 30, 24, 255, 255, 255, 79, 7, 255, 255,
            255, 14, 26, 255, 255, 255, 255, 255, 255, 255, 255, 37, 255, 255, 255, 255,
            32, 255, 255, 255, 60, 25, 255, 255, 255, 255, 34, 255, 255, 255, 59, 5,
            255, 255, 255, 255, 255, 255, 255, 255, 40, 255, 255, 255, 255, 8, 255, 255,
            255, 20, 22, 255, 255, 255, 255, 17, 255, 255, 255, 57, 51, 255, 255, 255,
            255, 255, 255, 255, 255, 38, 255, 255, 255, 255, 28, 255, 255, 255, 81, 33,
            255, 255, 255, 65, 12, 255, 255, 255, 56, 6, 255, 255, 255, 255, 255, 255,
            255, 255, 77, 255, 255, 255, 255, 31, 255, 255, 255, 62, 16, 255, 255, 255,
            82, 255, 255, 255, 255, 75, 35, 255, 255, 255, 255, 255, 255, 255, 255, 255,
            255, 255, 255, 255, 53, 255, 255, 255, 72, 19, 255, 255, 255, 255, 50, 255,
            255, 255, 54, 255, 255, 255, 255, 255, 255, 255, 255, 255, 43, 255, 255, 255,
            83, 29, 255, 255, 255, 71, 255, 255, 255, 255, 63, 21, 69, 1, 0, 27,
        ];

        // Path to Animate.lua bytecode dump
        let dump_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent().unwrap()
            .parent().unwrap()
            .join("target/release/bytecode_dumps/0603c30108476574_17957b.luac");

        let data = match std::fs::read(&dump_path) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("cannot read {}: {}", dump_path.display(), e);
                eprintln!("Run an executor first to generate the bytecode dump.");
                return;
            }
        };

        let chunk = match crate::parser::parse(&data) {
            Ok(c) => c,
            Err(e) => { eprintln!("parse failed: {:?}", e); return; }
        };

        eprintln!("=== B0.18 Animate.lua detection trace ===");
        eprintln!("protos={} total_words={}",
            chunk.protos.len(),
            chunk.protos.iter().map(|p| p.code.len()).sum::<usize>());

        // ── 1. ForGPrepINext (0x64) → target analysis ──────────────────────────
        let forgprep_inext_byte: u8 = 0x64;
        eprintln!("\n--- ForGPrepINext (0x64) instructions ---");
        let mut fgpi_count = 0usize;
        let mut fgpi_d_ok = 0usize;
        let mut fgpi_target_0x35 = 0usize;
        for (pi, proto) in chunk.protos.iter().enumerate() {
            for i in 0..proto.code.len() {
                let insn = proto.code[i];
                if insn_op(insn) != forgprep_inext_byte { continue; }
                fgpi_count += 1;
                let a = insn_a(insn);
                let d = insn_d(insn) as i32;
                let a_ok = a < proto.max_stack_size;
                let d_ok = d > 0;
                if d_ok { fgpi_d_ok += 1; }

                let target = (i as i32 + d + 1) as usize;
                let target_in_range = target < proto.code.len();
                let target_op = if target_in_range { insn_op(proto.code[target]) } else { 255 };
                let target_a = if target_in_range { insn_a(proto.code[target]) } else { 255 };
                let target_d = if target_in_range { insn_d(proto.code[target]) } else { 0 };
                let target_d_u = target_d as u16 as usize;

                if target_op == 0x35 { fgpi_target_0x35 += 1; }

                // Show first 20 ForGPrepINext instructions in detail
                if fgpi_count <= 20 {
                    eprintln!("  proto={} pc={} A={} D={} a_ok={} d_ok={} target={}/{} target_op=0x{:02X} target_a={} target_d_u={} target_exit={}",
                        pi, i, a, d, a_ok, d_ok,
                        target, proto.code.len(), target_op, target_a, target_d_u,
                        target + target_d_u + 1);
                }
            }
        }
        eprintln!("Total ForGPrepINext(0x64) = {}, d>0 = {}, target=0x35 = {}",
            fgpi_count, fgpi_d_ok, fgpi_target_0x35);

        // ── 2. Raw frequency of 0x35, 0xC1, 0x2A ──────────────────────────────
        eprintln!("\n--- Raw frequency (all words including AUX) ---");
        let mut freq = [0u32; 256];
        let mut total_words = 0u32;
        for proto in &chunk.protos {
            for &w in &proto.code {
                freq[insn_op(w) as usize] += 1;
                total_words += 1;
            }
        }
        eprintln!("total_words={} freq[0x35]={} freq[0xC1]={} freq[0x2A]={}",
            total_words, freq[0x35], freq[0xC1], freq[0x2A]);

        // ── 3. Instruction-position frequency (AUX-aware) for 0x35, 0xC1 ──────
        eprintln!("\n--- AUX-aware instruction-position scan ---");
        let mut insn_pos_freq = [0u32; 256];
        for proto in &chunk.protos {
            let code = &proto.code;
            let mut i = 0;
            while i < code.len() {
                let op = insn_op(code[i]);
                let canonical = prior256[op as usize];
                if canonical != 255 {
                    let luau_op = crate::parser::opcodes::LuauOpcode::from_u8(canonical);
                    if luau_op.has_aux() { i += 2; } else { i += 1; }
                } else {
                    insn_pos_freq[op as usize] += 1;
                    i += 1;
                }
            }
        }
        eprintln!("insn_pos_freq[0x35]={} insn_pos_freq[0xC1]={} insn_pos_freq[0x2A]={}",
            insn_pos_freq[0x35], insn_pos_freq[0xC1], insn_pos_freq[0x2A]);

        // ── 4. LoadKX (0xC1) purity trace ─────────────────────────────────────
        eprintln!("\n--- LoadKX (0xC1) purity trace ---");
        let mut lkx_total = 0u32;
        let mut lkx_d0_valid = 0u32;
        for (pi, proto) in chunk.protos.iter().enumerate() {
            let const_len = proto.constants.len();
            let code_len = proto.code.len();
            let mut skip_next = false;
            for i in 0..code_len {
                let insn = proto.code[i];
                let op = insn_op(insn);
                if skip_next { skip_next = false; continue; }
                let canonical = prior256[op as usize];
                if canonical != 255 {
                    let luau_op = crate::parser::opcodes::LuauOpcode::from_u8(canonical);
                    if luau_op.has_aux() { skip_next = true; }
                    continue;
                }
                if op == 0xC1 {
                    lkx_total += 1;
                    if i + 1 < code_len {
                        let a = insn_a(insn) as usize;
                        let d = insn_d(insn);
                        let aux_u = proto.code[i + 1] as usize;
                        let d_ok = d == 0;
                        let a_ok = a < proto.max_stack_size as usize;
                        let aux_ok = aux_u < const_len;
                        eprintln!("  proto={} pc={} A={} D={} aux=0x{:08X}({}) const_len={} d_ok={} a_ok={} aux_ok={}",
                            pi, i, a, d, proto.code[i+1], aux_u, const_len, d_ok, a_ok, aux_ok);
                        if d_ok && a_ok && aux_ok {
                            lkx_d0_valid += 1;
                            skip_next = true;
                        }
                    }
                }
            }
        }
        eprintln!("LoadKX(0xC1): total={} d0_valid={} purity={}",
            lkx_total, lkx_d0_valid,
            if lkx_total > 0 { lkx_d0_valid == lkx_total } else { false });

        // ── 5. Run detect_with_prior and report results ──────────────────────
        eprintln!("\n--- detect_with_prior(prior256) result ---");
        let result = OpcodeMap::detect_with_prior(&chunk, &prior256);
        let h_count = result.heuristic_map.iter().filter(|&&v| v != 255).count();
        let f_count = result.shuffled_to_standard.iter().filter(|&&v| v != 255).count();
        eprintln!("heuristic_map: {} opcodes detected", h_count);
        eprintln!("full_map: {} opcodes detected", f_count);
        eprintln!("heuristic_map[0x35]={} ({:?})", result.heuristic_map[0x35],
            crate::parser::opcodes::LuauOpcode::from_u8(result.heuristic_map[0x35]));
        eprintln!("heuristic_map[0xC1]={} ({:?})", result.heuristic_map[0xC1],
            crate::parser::opcodes::LuauOpcode::from_u8(result.heuristic_map[0xC1]));
        eprintln!("heuristic_map[0x2A]={} ({:?})", result.heuristic_map[0x2A],
            crate::parser::opcodes::LuauOpcode::from_u8(result.heuristic_map[0x2A]));
        eprintln!("full_map[0x35]={} ({:?})", result.shuffled_to_standard[0x35],
            crate::parser::opcodes::LuauOpcode::from_u8(result.shuffled_to_standard[0x35]));
        eprintln!("full_map[0xC1]={} ({:?})", result.shuffled_to_standard[0xC1],
            crate::parser::opcodes::LuauOpcode::from_u8(result.shuffled_to_standard[0xC1]));
        eprintln!("full_map[0x2A]={} ({:?})", result.shuffled_to_standard[0x2A],
            crate::parser::opcodes::LuauOpcode::from_u8(result.shuffled_to_standard[0x2A]));

        // Show all NEW entries (not in prior256)
        eprintln!("\n--- New entries added beyond prior ---");
        for i in 0..256usize {
            if result.heuristic_map[i] != 255 && prior256[i] == 255 {
                eprintln!("  heuristic: 0x{:02X} -> {} ({:?})",
                    i, result.heuristic_map[i],
                    crate::parser::opcodes::LuauOpcode::from_u8(result.heuristic_map[i]));
            }
        }
        for i in 0..256usize {
            if result.shuffled_to_standard[i] != 255 && prior256[i] == 255 &&
               result.heuristic_map[i] == 255 {
                eprintln!("  permutation_complete only: 0x{:02X} -> {} ({:?})",
                    i, result.shuffled_to_standard[i],
                    crate::parser::opcodes::LuauOpcode::from_u8(result.shuffled_to_standard[i]));
            }
        }

        // ── 6. Run fresh detect (NO prior) to see what the detectors say ──────
        eprintln!("\n--- Fresh detect (no prior) ---");
        let fresh = OpcodeMap::detect(&chunk);
        let fresh_h = fresh.heuristic_map.iter().filter(|&&v| v != 255).count();
        let fresh_f = fresh.shuffled_to_standard.iter().filter(|&&v| v != 255).count();
        eprintln!("heuristic: {} opcodes, full: {} opcodes", fresh_h, fresh_f);
        eprintln!("fresh heuristic[0x35]={} ({:?})", fresh.heuristic_map[0x35],
            crate::parser::opcodes::LuauOpcode::from_u8(fresh.heuristic_map[0x35]));
        eprintln!("fresh heuristic[0xC1]={} ({:?})", fresh.heuristic_map[0xC1],
            crate::parser::opcodes::LuauOpcode::from_u8(fresh.heuristic_map[0xC1]));
        eprintln!("fresh heuristic[0x2A]={} ({:?})", fresh.heuristic_map[0x2A],
            crate::parser::opcodes::LuauOpcode::from_u8(fresh.heuristic_map[0x2A]));
        eprintln!("fresh heuristic[0x64]={} ({:?})", fresh.heuristic_map[0x64],
            crate::parser::opcodes::LuauOpcode::from_u8(fresh.heuristic_map[0x64]));
        // Show full fresh heuristic map
        eprintln!("--- Full fresh heuristic map ---");
        let mut fresh_mappings: Vec<(u8, u8)> = fresh.heuristic_map.iter().enumerate()
            .filter(|(_, &v)| v != 255)
            .map(|(i, &v)| (i as u8, v))
            .collect();
        fresh_mappings.sort_by_key(|&(_, std)| std);
        for (sh, st) in &fresh_mappings {
            eprintln!("  0x{:02X} -> {:2} ({:?})", sh, st,
                crate::parser::opcodes::LuauOpcode::from_u8(*st));
        }
        // ── Extra: trace 0x51 (ForGPrepINext in fresh) instructions ────────────
        eprintln!("\n--- 0x51 (ForGPrepINext per fresh) instructions ---");
        let mut cnt51 = 0usize;
        for (pi, proto) in chunk.protos.iter().enumerate() {
            for i in 0..proto.code.len() {
                let insn = proto.code[i];
                if insn_op(insn) != 0x51 { continue; }
                cnt51 += 1;
                let a = insn_a(insn);
                let d = insn_d(insn) as i32;
                let target = (i as i32 + d + 1) as usize;
                let in_range = target < proto.code.len();
                let tgt_op = if in_range { insn_op(proto.code[target]) } else { 255 };
                let tgt_a = if in_range { insn_a(proto.code[target]) } else { 255 };
                let tgt_d = if in_range { insn_d(proto.code[target]) as u16 as usize } else { 0 };
                if cnt51 <= 30 {
                    eprintln!("  proto={} pc={} A={} D={} -> target={}/{} tgt_op=0x{:02X} tgt_a={} tgt_d_u={} mapped_in_fresh={}",
                        pi, i, a, d, target, proto.code.len(), tgt_op, tgt_a, tgt_d,
                        fresh.heuristic_map[tgt_op as usize] != 255);
                }
            }
        }
        eprintln!("Total 0x51 = {}", cnt51);

        // Show conflicts between prior256 and fresh heuristic
        eprintln!("\n--- Conflicts: prior vs fresh ---");
        for i in 0..256usize {
            let p = prior256[i];
            let f = fresh.heuristic_map[i];
            if p != 255 && f != 255 && p != f {
                eprintln!("  CONFLICT 0x{:02X}: prior={} ({:?}) vs fresh={} ({:?})",
                    i, p, crate::parser::opcodes::LuauOpcode::from_u8(p),
                    f, crate::parser::opcodes::LuauOpcode::from_u8(f));
            }
        }
    }

    // ============================================================================
    // Phase B0.39C: detector test coverage for untested opcode detectors
    //
    // Adds positive + negative tests for:
    //   detect_arithmetic, detect_arithmetic_k, detect_unary_not_minus,
    //   detect_unary_ops, detect_call, detect_namecall, detect_getimport,
    //   detect_bitwise_ops, detect_move, detect_concat
    // ============================================================================

    /// ---- detect_call -------------------------------------------------------

    #[test]
    fn detect_call_assigns_frequent_abc_byte() {
        // CALL: A<stack, B<=8, C<=5, with C>0 for a meaningful fraction (>=15%).
        // Build 12 occurrences of a single shuffled byte, all with C=2 (→ 1 return).
        // This clears the strict filter (count>=10, C<=2 ratio>=60%, C>0 ratio>=15%).
        let call_byte: u8 = 0xC0;
        let mut code: Vec<u32> = Vec::new();
        for _ in 0..12 {
            code.push(insn_abc(call_byte, 0, 2, 2)); // A=0 func, B=2 nargs+1, C=2 nresults+1
        }
        code.push(insn_abc(0xDD, 0, 0, 0)); // filler
        let chunk = chunk_from_code(code, 8);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_call(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[call_byte as usize],
            LuauOpcode::Call as u8,
            "detect_call failed to map frequent CALL-shaped byte 0x{:02X}", call_byte
        );
        assert!(ctx.assigned[LuauOpcode::Call as usize]);
    }

    #[test]
    fn detect_call_rejects_c_always_zero_byte() {
        // A byte with C=0 on every instance looks like GETUPVAL / MOVE / unary —
        // NOT like CALL. detect_call requires C>0 for >=15% of instances.
        let fake_byte: u8 = 0xC5;
        let mut code: Vec<u32> = Vec::new();
        for _ in 0..20 {
            code.push(insn_abc(fake_byte, 0, 1, 0)); // C=0 always
        }
        code.push(insn_abc(0xDD, 0, 0, 0));
        let chunk = chunk_from_code(code, 8);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_call(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[fake_byte as usize], 255,
            "detect_call wrongly claimed C-always-0 byte 0x{:02X} as CALL", fake_byte
        );
        assert!(!ctx.assigned[LuauOpcode::Call as usize]);
    }

    /// ---- detect_namecall ---------------------------------------------------

    #[test]
    fn detect_namecall_with_call_pre_seeded() {
        // NAMECALL pattern: op | AUX(string_const_idx) | CALL(same A).
        // With CALL pre-seeded, a single occurrence must map.
        let namecall_byte: u8 = 0xE1;
        let call_byte: u8 = 0xC0;
        let code = vec![
            insn_abc(namecall_byte, 2, 1, 0), // 0: NAMECALL A=2 B=1(self)
            0x00000000,                        // 1: AUX = const idx 0 (string)
            insn_abc(call_byte, 2, 2, 1),      // 2: CALL A=2
            insn_abc(0xDD, 0, 0, 0),           // 3: filler
        ];
        let mut chunk = chunk_from_code(code, 8);
        chunk.protos[0].constants = vec![Constant::String("GetService".to_string())];

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        ctx.map[call_byte as usize] = LuauOpcode::Call as u8;
        ctx.assigned[LuauOpcode::Call as usize] = true;

        detect_namecall(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[namecall_byte as usize],
            LuauOpcode::NameCall as u8,
            "detect_namecall failed to map 0x{:02X} with CALL pre-seeded", namecall_byte
        );
    }

    #[test]
    fn detect_namecall_rejects_non_string_aux() {
        // AUX index points to a Number constant — not a method name — so NAMECALL
        // must NOT be assigned.
        let fake_byte: u8 = 0xE2;
        let call_byte: u8 = 0xC0;
        let code = vec![
            insn_abc(fake_byte, 2, 1, 0),
            0x00000000,                  // AUX = const idx 0 (but const 0 is Number!)
            insn_abc(call_byte, 2, 2, 1),
            insn_abc(0xDD, 0, 0, 0),
        ];
        let mut chunk = chunk_from_code(code, 8);
        chunk.protos[0].constants = vec![Constant::Number(42.0)];

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        ctx.map[call_byte as usize] = LuauOpcode::Call as u8;
        ctx.assigned[LuauOpcode::Call as usize] = true;

        detect_namecall(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[fake_byte as usize], 255,
            "detect_namecall wrongly claimed 0x{:02X} with non-string AUX", fake_byte
        );
        assert!(!ctx.assigned[LuauOpcode::NameCall as usize]);
    }

    /// ---- detect_getimport --------------------------------------------------

    #[test]
    fn detect_getimport_maps_with_aux_and_import_const() {
        // GETIMPORT: AD insn where D points to Constant::Import, next word is
        // an AUX with count in [1..3] and valid string ids. A single valid
        // instance is sufficient (the detector force-assigns on count>=1).
        let gi_byte: u8 = 0xF0;
        // AUX with count=1, id0=0 → chunk.strings[0] must exist.
        let aux = (1u32 << 30) | (0u32 << 20);
        let code = vec![
            insn_ad(gi_byte, 5, 0),  // D=0 → constants[0] = Import
            aux,                      // AUX
            insn_abc(0xDD, 0, 0, 0), // filler
        ];
        let mut chunk = chunk_from_code(code, 8);
        // pack id: count=1, id0=0
        let packed = (1u32 << 30) | (0u32 << 20);
        chunk.protos[0].constants = vec![Constant::Import(packed)];
        chunk.strings = vec!["game".to_string()];

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_getimport(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[gi_byte as usize],
            LuauOpcode::GetImport as u8,
            "detect_getimport failed to map 0x{:02X} with valid AUX+Import const", gi_byte
        );
    }

    #[test]
    fn detect_getimport_rejects_non_import_constant() {
        // D points to a String constant (not Import). Detector must not assign.
        let fake_byte: u8 = 0xF1;
        let aux = (1u32 << 30) | (0u32 << 20);
        let code = vec![
            insn_ad(fake_byte, 5, 0),
            aux,
            insn_abc(0xDD, 0, 0, 0),
        ];
        let mut chunk = chunk_from_code(code, 8);
        chunk.protos[0].constants = vec![Constant::String("x".to_string())];
        chunk.strings = vec!["x".to_string()];

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_getimport(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[fake_byte as usize], 255,
            "detect_getimport wrongly claimed 0x{:02X} (D points to String, not Import)", fake_byte
        );
        assert!(!ctx.assigned[LuauOpcode::GetImport as usize]);
    }

    /// ---- detect_arithmetic -------------------------------------------------

    #[test]
    fn detect_arithmetic_assigns_add_to_most_frequent_abc_reg_byte() {
        // Arithmetic: A,B,C<stack with C>0. count>=3, <3% of total_insns.
        // Inject 5 occurrences of arith_byte and 20 instructions total to keep
        // it under the 3% cap (total/30 = 0 when total<100, so cap is unbounded).
        let arith_byte: u8 = 0xA5;
        let mut code: Vec<u32> = Vec::new();
        for _ in 0..5 {
            code.push(insn_abc(arith_byte, 0, 1, 2)); // A=0 B=1 C=2
        }
        for _ in 0..10 {
            code.push(insn_abc(0xDD, 0, 0, 0)); // filler (C=0 → doesn't match)
        }
        let chunk = chunk_from_code(code, 4);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_arithmetic(&chunk, &mut ctx);

        // Assigns in order Add, Sub, Mul, ... — the single qualifying byte gets Add.
        assert_eq!(
            ctx.map[arith_byte as usize],
            LuauOpcode::Add as u8,
            "detect_arithmetic failed to assign Add to sole ABC-reg candidate 0x{:02X}", arith_byte
        );
    }

    #[test]
    fn detect_arithmetic_rejects_c_zero_bytes() {
        // A byte with C=0 must NOT be picked by detect_arithmetic
        // (detector requires C>0 to differentiate from MOVE/unary).
        let fake_byte: u8 = 0xA6;
        let mut code: Vec<u32> = Vec::new();
        for _ in 0..10 {
            code.push(insn_abc(fake_byte, 0, 1, 0)); // C=0
        }
        let chunk = chunk_from_code(code, 4);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_arithmetic(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[fake_byte as usize], 255,
            "detect_arithmetic wrongly mapped C=0 byte 0x{:02X}", fake_byte
        );
        assert!(!ctx.assigned[LuauOpcode::Add as usize]);
    }

    /// ---- detect_arithmetic_k -----------------------------------------------

    #[test]
    fn detect_arithmetic_k_maps_byte_with_number_const_in_c() {
        // ArithK: A,B<stack, C<const_len AND constants[C] is Number.
        // Filler uses C=5 which is out of const range (const_len=1) so it
        // does NOT qualify as an arithK candidate.
        let ark_byte: u8 = 0xB5;
        let mut code: Vec<u32> = Vec::new();
        for _ in 0..5 {
            code.push(insn_abc(ark_byte, 0, 1, 0)); // C=0 → constants[0]=Number
        }
        for _ in 0..10 {
            code.push(insn_abc(0xDD, 0, 0, 5)); // C=5 >= const_len → no match
        }
        let mut chunk = chunk_from_code(code, 8);
        chunk.protos[0].constants = vec![Constant::Number(3.14)];

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_arithmetic_k(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[ark_byte as usize],
            LuauOpcode::AddK as u8,
            "detect_arithmetic_k failed to assign AddK to 0x{:02X}", ark_byte
        );
    }

    #[test]
    fn detect_arithmetic_k_rejects_string_constant_in_c() {
        // Constants[0] is String, not Number — detector must not claim byte.
        let fake_byte: u8 = 0xB6;
        let mut code: Vec<u32> = Vec::new();
        for _ in 0..5 {
            code.push(insn_abc(fake_byte, 0, 1, 0));
        }
        let mut chunk = chunk_from_code(code, 4);
        chunk.protos[0].constants = vec![Constant::String("s".to_string())];

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_arithmetic_k(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[fake_byte as usize], 255,
            "detect_arithmetic_k wrongly claimed 0x{:02X} with String constant", fake_byte
        );
    }

    #[test]
    fn comparing_a_register_with_itself_is_not_evidence_of_a_comparison_jump() {
        // 0xD2 outnumbers 0xD1 four sites to three, but every one of its sites
        // compares a register with ITSELF — the shape JumpXEqKN produces for
        // free whenever its constant index happens to equal A. Counting those
        // lets it outrank the byte that has real evidence and take the slot.
        //
        // The sites are discounted, not the byte: a byte keeps every genuine
        // site it has. See the note on `self_cmp` for why disqualifying the
        // whole byte is wrong.
        let real: u8 = 0xD1;
        let selfcmp: u8 = 0xD2;
        let filler = insn_abc(0xEE, 200, 0, 0); // A >= max_stack: never a candidate

        let mut code: Vec<u32> = Vec::new();
        for _ in 0..3 {
            code.push(insn_ad(real, 0, 3)); // A = R0 ...
            code.push(1); // ... AUX = R1, a genuine two-register comparison
        }
        for _ in 0..4 {
            code.push(insn_ad(selfcmp, 2, 3)); // A = R2 ...
            code.push(2); // ... AUX = R2, the same register
        }
        code.resize(20, filler);
        let chunk = chunk_from_code(code, 4);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_comparison_jumps_aux(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[real as usize], LuauOpcode::JumpIfNotLT as u8,
            "the byte with genuine two-register sites must take the slot"
        );
        assert_eq!(
            ctx.map[selfcmp as usize], 255,
            "self-comparison sites alone are not evidence and must claim nothing"
        );
    }

    #[test]
    fn repeat_until_guard_is_read_as_a_jump_if_true_form() {
        // Two comparison-jump bytes, identical in every respect the encoding can
        // show, separated only by the shape they sit in:
        //
        //   0xC1  `repeat ... until a < b`  — jumps FORWARD over the backward
        //                                     jump, so TRUE takes the branch
        //   0xC2  a plain `if` guard        — TRUE falls through, so it is a
        //                                     NOT form
        //
        // Without the repeat-until site both land in the NOT bucket and 0xC1 is
        // mislabelled JumpIfNotLT.
        let cmp_true: u8 = 0xC1;
        let cmp_not: u8 = 0xC2;
        let jumpback: u8 = 0xC3;
        let filler = insn_abc(0xEE, 200, 0, 0); // A >= max_stack: never a candidate

        let mut code: Vec<u32> = vec![
            insn_ad(cmp_true, 0, 2), // 0: exits the loop when the test holds
            1,                       // 1: AUX = right register
            insn_ad(jumpback, 0, -3),// 2: closes the loop
            insn_ad(cmp_not, 0, 4),  // 3: ordinary forward guard
            1,                       // 4: AUX = right register
        ];
        code.resize(10, filler);
        let chunk = chunk_from_code(code, 4);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_comparison_jumps_aux(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[cmp_true as usize], LuauOpcode::JumpIfLT as u8,
            "a byte used to leave a repeat-until loop is a jump-if-TRUE form"
        );
        assert_eq!(
            ctx.map[cmp_not as usize], LuauOpcode::JumpIfNotLT as u8,
            "a byte with no true-form site must stay in the NOT bucket"
        );
    }

    #[test]
    fn detect_arithmetic_k_does_not_read_aux_words_as_instructions() {
        // The AUX word of an already-mapped AUX-bearing opcode is DATA — here a
        // GETIMPORT path constant. Its low byte is whatever that datum happens
        // to be, and treating it as an instruction credits that byte with
        // arithmetic-K sightings it never had.
        //
        // The chunk below contains no arithmetic-K at all: three GETIMPORTs
        // whose AUX words are crafted so that reading them as instructions
        // yields (op = 0xC7, A = 0, B = 1, C = 0), which passes the detector's
        // "A and B are registers, C indexes a number constant" test. Under an
        // every-word walk 0xC7 collects three sightings and is handed AddK.
        let aux_byte: u8 = 0xC7;
        let getimport: u8 = 0x2A;
        let aux_word = insn_abc(aux_byte, 0, 1, 0);

        let mut code: Vec<u32> = Vec::new();
        for _ in 0..3 {
            code.push(insn_abc(getimport, 0, 0, 0));
            code.push(aux_word);
        }
        let mut chunk = chunk_from_code(code, 8);
        chunk.protos[0].constants = vec![Constant::Number(1.0)];

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        // GetImport carries an AUX word, so the walk must step over it.
        ctx.try_assign(getimport, LuauOpcode::GetImport as u8);
        assert!(LuauOpcode::from_u8(LuauOpcode::GetImport as u8).has_aux());

        detect_arithmetic_k(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[aux_byte as usize], 255,
            "0x{:02X} occurs only as AUX data and must not be claimed as arithmetic-K",
            aux_byte
        );
        assert!(!ctx.assigned[LuauOpcode::AddK as usize]);
    }

    /// ---- detect_move -------------------------------------------------------

    #[test]
    fn detect_move_assigns_high_freq_c_zero_byte() {
        // MOVE: A<stack, B<stack, C=0 for ALL instances. min_count=1 for <100 insns.
        let move_byte: u8 = 0x10;
        let code = vec![
            insn_abc(move_byte, 1, 0, 0),
            insn_abc(move_byte, 2, 0, 0),
            insn_abc(move_byte, 3, 1, 0),
            insn_abc(move_byte, 0, 2, 0),
            insn_abc(0xDD, 0, 0, 0),
        ];
        let chunk = chunk_from_code(code, 4);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_move(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[move_byte as usize],
            LuauOpcode::Move as u8,
            "detect_move failed to assign Move to C=0 byte 0x{:02X}", move_byte
        );
    }

    #[test]
    fn detect_move_rejects_byte_with_any_c_nonzero() {
        // MOVE requires C=0 for ALL instances of the byte.
        // Even one C!=0 sample disqualifies THIS specific byte from being
        // picked as Move (detector may still pick a different qualifying byte,
        // which is fine — we only assert the C!=0 byte itself is rejected).
        let fake_byte: u8 = 0x11;
        let code = vec![
            insn_abc(fake_byte, 1, 0, 0),
            insn_abc(fake_byte, 2, 0, 0),
            insn_abc(fake_byte, 3, 1, 2), // C!=0 — disqualifies this byte
            insn_abc(0xDD, 0, 0, 0),
        ];
        let chunk = chunk_from_code(code, 4);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_move(&chunk, &mut ctx);

        // Specifically assert 0x11 is not mapped to Move.
        assert_ne!(
            ctx.map[fake_byte as usize],
            LuauOpcode::Move as u8,
            "detect_move wrongly claimed byte 0x{:02X} despite C!=0 in one instance", fake_byte
        );
    }

    /// ---- detect_concat -----------------------------------------------------

    #[test]
    fn detect_concat_maps_abc_with_b_lt_c_range() {
        // CONCAT: A<stack, B<stack, C<stack, B<C, C-B in [1..20].
        // valid>=2, total>=2, valid/total >= 80%.
        let concat_byte: u8 = 0x20;
        let code = vec![
            insn_abc(concat_byte, 0, 1, 3), // B=1, C=3 (range of 3)
            insn_abc(concat_byte, 0, 2, 5),
            insn_abc(concat_byte, 1, 0, 2),
            insn_abc(0xDD, 0, 0, 0),
        ];
        let chunk = chunk_from_code(code, 8);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_concat(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[concat_byte as usize],
            LuauOpcode::Concat as u8,
            "detect_concat failed to assign Concat to B<C byte 0x{:02X}", concat_byte
        );
    }

    #[test]
    fn detect_concat_rejects_b_equal_c_bytes() {
        // Byte where B>=C for every instance must not match (valid=0).
        let fake_byte: u8 = 0x21;
        let code = vec![
            insn_abc(fake_byte, 0, 2, 2), // B==C
            insn_abc(fake_byte, 1, 3, 1), // B>C
            insn_abc(fake_byte, 0, 4, 4),
            insn_abc(0xDD, 0, 0, 0),
        ];
        let chunk = chunk_from_code(code, 8);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_concat(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[fake_byte as usize], 255,
            "detect_concat wrongly claimed byte 0x{:02X} with B>=C", fake_byte
        );
        assert!(!ctx.assigned[LuauOpcode::Concat as usize]);
    }

    /// ---- detect_bitwise_ops ------------------------------------------------

    #[test]
    fn detect_bitwise_ops_bails_without_arith_mapped() {
        // Detector short-circuits if ADD..POW aren't all mapped. Supply a
        // clearly-unary-shaped byte; since ADD isn't mapped, nothing happens.
        let byte_u: u8 = 0x30;
        let code = vec![
            insn_abc(byte_u, 1, 0, 0),
            insn_abc(byte_u, 2, 0, 0),
            insn_abc(byte_u, 3, 1, 0),
            insn_abc(0xDD, 0, 0, 0),
        ];
        let chunk = chunk_from_code(code, 4);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_bitwise_ops(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[byte_u as usize], 255,
            "detect_bitwise_ops must bail when arithmetic ops are unmapped"
        );
    }
    #[test]
    fn detect_bitwise_ops_is_disabled_and_assigns_nothing() {
        // detect_bitwise_ops is disabled: Luau source has no bitwise operators,
        // so the stock compiler never emits opcodes 84-91, and shape-based
        // detection reliably mislabels real opcodes as bitwise ones. Measured
        // on a 1286-script corpus it fabricated BAND/BOR/BNOT/SHL/SHR/BANDK in
        // 262 files, including bitwise ops on nil, floats and a Vector3.
        //
        // This asserts the contract that replaced the old classification tests:
        // even with every prerequisite satisfied, the pass must assign NOTHING.
        let byte_u: u8 = 0x30;
        let code = vec![
            insn_abc(byte_u, 1, 0, 0),
            insn_abc(byte_u, 2, 0, 0),
            insn_abc(byte_u, 3, 1, 0),
            insn_abc(0xDD, 0, 0, 0),
        ];
        let chunk = chunk_from_code(code, 8);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        // Satisfy the old prerequisite gate so we prove the disable, not the gate.
        for (sh, std) in [
            (0xA0u8, LuauOpcode::Add), (0xA1, LuauOpcode::Sub), (0xA2, LuauOpcode::Mul),
            (0xA3, LuauOpcode::Div),   (0xA4, LuauOpcode::Mod), (0xA5, LuauOpcode::Pow),
        ].iter() {
            ctx.map[*sh as usize] = *std as u8;
            ctx.assigned[*std as usize] = true;
        }
        let before = ctx.map;

        detect_bitwise_ops(&chunk, &mut ctx);

        assert_eq!(ctx.map, before, "disabled pass must not modify the map");
        for op in [LuauOpcode::Band, LuauOpcode::Bor, LuauOpcode::Bxor,
                   LuauOpcode::Bnot, LuauOpcode::Shl, LuauOpcode::Shr,
                   LuauOpcode::Bandk, LuauOpcode::Bork] {
            assert!(!ctx.assigned[op as usize],
                "{:?} must never be assigned by the disabled pass", op);
        }
    }

    /// ---- detect_unary_not_minus -------------------------------------------

    #[test]
    fn detect_unary_not_minus_bails_without_move() {
        // Detector short-circuits if MOVE isn't mapped.
        let cand_byte: u8 = 0x40;
        let code = vec![
            insn_abc(cand_byte, 1, 0, 0),
            insn_abc(cand_byte, 2, 0, 0),
            insn_abc(cand_byte, 3, 1, 0),
            insn_abc(0xDD, 0, 0, 0),
        ];
        let chunk = chunk_from_code(code, 4);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_unary_not_minus(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[cand_byte as usize], 255,
            "detect_unary_not_minus must bail when MOVE is not yet mapped"
        );
        assert!(!ctx.assigned[LuauOpcode::Not as usize]);
        assert!(!ctx.assigned[LuauOpcode::Minus as usize]);
    }

    #[test]
    fn detect_unary_not_minus_assigns_with_numeric_context() {
        // With MOVE, LOADN, and ADD pre-mapped, a unary-shaped candidate whose
        // result feeds into an ADD (numeric consumer) and whose source was
        // produced by LOADN (numeric producer) should map to Not (first slot).
        let move_byte: u8 = 0x50;
        let loadn_byte: u8 = 0x51;
        let add_byte: u8 = 0x52;
        let unary_byte: u8 = 0x53;

        // Layout: each unary instance is flanked by LOADN (producer) and ADD
        // (consumer). 3 instances to satisfy total>=3 and ctx_hits*2>=total.
        let code = vec![
            insn_ad(loadn_byte, 0, 10),              // 0: LOADN r0, 10
            insn_abc(unary_byte, 1, 0, 0),           // 1: UNARY r1 = op(r0)
            insn_abc(add_byte, 2, 1, 0),             // 2: ADD r2 = r1 + r0 (consumes r1)
            insn_ad(loadn_byte, 0, 20),              // 3: LOADN r0, 20
            insn_abc(unary_byte, 1, 0, 0),           // 4: UNARY r1
            insn_abc(add_byte, 2, 1, 0),             // 5: ADD consumes r1
            insn_ad(loadn_byte, 0, 30),              // 6
            insn_abc(unary_byte, 1, 0, 0),           // 7
            insn_abc(add_byte, 2, 1, 0),             // 8
            insn_abc(0xDD, 0, 0, 0),                 // 9
        ];
        let chunk = chunk_from_code(code, 4);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        ctx.map[move_byte as usize] = LuauOpcode::Move as u8;
        ctx.assigned[LuauOpcode::Move as usize] = true;
        ctx.map[loadn_byte as usize] = LuauOpcode::LoadN as u8;
        ctx.assigned[LuauOpcode::LoadN as usize] = true;
        ctx.map[add_byte as usize] = LuauOpcode::Add as u8;
        ctx.assigned[LuauOpcode::Add as usize] = true;

        detect_unary_not_minus(&chunk, &mut ctx);

        // First slot in the detector's target order is Not.
        assert_eq!(
            ctx.map[unary_byte as usize],
            LuauOpcode::Not as u8,
            "detect_unary_not_minus failed to assign Not to 0x{:02X} with numeric context", unary_byte
        );
    }

    /// ---- detect_unary_ops (LENGTH fallback) -------------------------------

    #[test]
    fn detect_unary_ops_bails_without_move_and_getupval() {
        // Detector requires BOTH Move and GetUpval mapped.
        let cand_byte: u8 = 0x60;
        let code = vec![
            insn_abc(cand_byte, 1, 0, 0),
            insn_abc(0xDD, 0, 0, 0),
        ];
        let chunk = chunk_from_code(code, 4);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        // Map Move but NOT GetUpval — should still bail.
        ctx.map[0xAA as usize] = LuauOpcode::Move as u8;
        ctx.assigned[LuauOpcode::Move as usize] = true;

        detect_unary_ops(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[cand_byte as usize], 255,
            "detect_unary_ops must bail when GetUpval is not mapped"
        );
        assert!(!ctx.assigned[LuauOpcode::Length as usize]);
    }

    #[test]
    fn detect_unary_ops_assigns_length_when_result_feeds_numeric_consumer() {
        // With Move, GetUpval, and ADD (a numeric consumer) pre-mapped, a
        // unary-shaped candidate whose R(A) is read by the following ADD
        // within 8 insns should map to Length.
        let move_byte: u8 = 0x70;
        let getupval_byte: u8 = 0x71;
        let add_byte: u8 = 0x72;
        let len_byte: u8 = 0x73;

        let code = vec![
            insn_abc(len_byte, 1, 0, 0),  // 0: LEN r1 = #r0
            insn_abc(add_byte, 2, 1, 1),  // 1: ADD r2 = r1 + r1 (consumes r1)
            insn_abc(len_byte, 1, 0, 0),  // 2
            insn_abc(add_byte, 2, 1, 1),  // 3
            insn_abc(len_byte, 1, 0, 0),  // 4
            insn_abc(add_byte, 2, 1, 1),  // 5
            insn_abc(0xDD, 0, 0, 0),      // 6
        ];
        let chunk = chunk_from_code(code, 4);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        ctx.map[move_byte as usize] = LuauOpcode::Move as u8;
        ctx.assigned[LuauOpcode::Move as usize] = true;
        ctx.map[getupval_byte as usize] = LuauOpcode::GetUpval as u8;
        ctx.assigned[LuauOpcode::GetUpval as usize] = true;
        ctx.map[add_byte as usize] = LuauOpcode::Add as u8;
        ctx.assigned[LuauOpcode::Add as usize] = true;

        detect_unary_ops(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[len_byte as usize],
            LuauOpcode::Length as u8,
            "detect_unary_ops failed to assign Length to 0x{:02X} with numeric-consumer context", len_byte
        );
    }

    // ============================================================================
    // Phase B0.41B: Additional coverage for previously under-tested detectors.
    // Each test pairs with a corresponding negative test where applicable.
    // ============================================================================

    /// ---- detect_call additional coverage -----------------------------------

    #[test]
    fn detect_call_assigns_with_multireturn_c_zero_mix() {
        // Mix of C=0 (multi-return) and C=1 (no return) — both qualify for the
        // c<=2 bucket. The c>0 ratio (50%) clears the >=15% gate.
        let call_byte: u8 = 0xC1;
        let mut code: Vec<u32> = Vec::new();
        for _ in 0..6 {
            code.push(insn_abc(call_byte, 0, 1, 0)); // multi-return
        }
        for _ in 0..6 {
            code.push(insn_abc(call_byte, 0, 1, 1)); // no return value
        }
        code.push(insn_abc(0xDD, 0, 0, 0));
        let chunk = chunk_from_code(code, 8);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_call(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[call_byte as usize],
            LuauOpcode::Call as u8,
            "detect_call failed on mixed C=0/C=1 distribution"
        );
    }

    /// ---- detect_namecall additional coverage --------------------------------

    #[test]
    fn detect_namecall_without_call_requires_two_hits() {
        // Without CALL pre-seeded, the detector requires count >= 2 AND >= 50%
        // string-AUX ratio. Two valid NAMECALLs (each AUX → string) must map.
        let nc_byte: u8 = 0xE5;
        let code = vec![
            insn_abc(nc_byte, 2, 1, 0), // 0: NAMECALL
            0x00000000,                  // 1: AUX → const 0 (string)
            insn_abc(0xAA, 0, 0, 0),    // 2: filler (so we don't need CALL)
            insn_abc(nc_byte, 3, 2, 0), // 3: NAMECALL
            0x00000000,                  // 4: AUX → const 0 (string)
            insn_abc(0xBB, 0, 0, 0),    // 5: filler
            insn_abc(0xDD, 0, 0, 0),    // 6: filler
        ];
        let mut chunk = chunk_from_code(code, 8);
        chunk.protos[0].constants = vec![Constant::String("Method".to_string())];

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        // Note: CALL deliberately NOT seeded.
        detect_namecall(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[nc_byte as usize],
            LuauOpcode::NameCall as u8,
            "detect_namecall must accept 2+ string-AUX hits even without CALL"
        );
    }

    /// ---- detect_getimport additional coverage -------------------------------

    #[test]
    fn detect_getimport_rejects_invalid_aux_count() {
        // AUX has count=0 (invalid — must be 1..=3). Detector must skip.
        let gi_byte: u8 = 0xF2;
        let aux_bad = 0u32; // count = 0
        let code = vec![
            insn_ad(gi_byte, 5, 0),
            aux_bad,
            insn_abc(0xDD, 0, 0, 0),
        ];
        let mut chunk = chunk_from_code(code, 8);
        let packed = (1u32 << 30) | (0u32 << 20);
        chunk.protos[0].constants = vec![Constant::Import(packed)];
        chunk.strings = vec!["game".to_string()];

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_getimport(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[gi_byte as usize], 255,
            "detect_getimport must reject AUX with count=0"
        );
        assert!(!ctx.assigned[LuauOpcode::GetImport as usize]);
    }

    /// ---- detect_arithmetic additional coverage -----------------------------

    #[test]
    fn detect_arithmetic_assigns_multiple_ops_in_order() {
        // Two distinct ABC-reg bytes with C>0 must be assigned to Add and Sub
        // (in frequency-descending order, byte-asc tiebreak).
        let add_byte: u8 = 0xA0;
        let sub_byte: u8 = 0xA1;
        let mut code: Vec<u32> = Vec::new();
        for _ in 0..6 {
            code.push(insn_abc(add_byte, 0, 1, 2));
        }
        for _ in 0..4 {
            code.push(insn_abc(sub_byte, 0, 1, 2));
        }
        for _ in 0..30 {
            code.push(insn_abc(0xDD, 0, 0, 0)); // C=0 filler
        }
        let chunk = chunk_from_code(code, 4);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_arithmetic(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[add_byte as usize],
            LuauOpcode::Add as u8,
            "highest-frequency ABC-reg byte must get Add"
        );
        assert_eq!(
            ctx.map[sub_byte as usize],
            LuauOpcode::Sub as u8,
            "second-highest must get Sub"
        );
    }

    /// ---- detect_arithmetic_k additional coverage ---------------------------

    #[test]
    fn detect_arithmetic_k_rejects_when_count_below_threshold() {
        // Only 2 hits — below the count >= 3 gate. Must not assign.
        let ak_byte: u8 = 0xB5;
        let code = vec![
            insn_abc(ak_byte, 0, 1, 0),
            insn_abc(ak_byte, 0, 1, 0),
            insn_abc(0xDD, 0, 0, 0),
        ];
        let mut chunk = chunk_from_code(code, 4);
        chunk.protos[0].constants = vec![Constant::Number(7.0)];

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_arithmetic_k(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[ak_byte as usize], 255,
            "detect_arithmetic_k must require count >= 3"
        );
    }

    /// ---- detect_move additional coverage -----------------------------------

    #[test]
    fn detect_move_prefers_higher_frequency_byte() {
        // Two MOVE-shaped candidates (both C=0). The more frequent one wins.
        let high_byte: u8 = 0x70;
        let low_byte: u8 = 0x71;
        let mut code: Vec<u32> = Vec::new();
        for i in 0..15 {
            code.push(insn_abc(high_byte, (i % 4) as u8, ((i + 1) % 4) as u8, 0));
        }
        for i in 0..5 {
            code.push(insn_abc(low_byte, (i % 4) as u8, ((i + 1) % 4) as u8, 0));
        }
        code.push(insn_abc(0xDD, 0, 0, 1)); // filler with C!=0
        let chunk = chunk_from_code(code, 4);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_move(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[high_byte as usize],
            LuauOpcode::Move as u8,
            "higher-frequency C=0 byte should win MOVE"
        );
        // low_byte should remain unmapped (Move only assigned once)
        assert_ne!(
            ctx.map[low_byte as usize],
            LuauOpcode::Move as u8,
            "second MOVE candidate should not be assigned"
        );
    }

    /// ---- detect_concat additional coverage ---------------------------------

    #[test]
    fn detect_concat_requires_80pct_valid_ratio() {
        // 2 valid (B<C) and 3 invalid (B==C) for the same byte → 40% ratio.
        // Below the 80% gate — must NOT be assigned.
        let cc_byte: u8 = 0xC8;
        let code = vec![
            insn_abc(cc_byte, 0, 1, 3), // valid
            insn_abc(cc_byte, 0, 1, 3), // valid
            insn_abc(cc_byte, 0, 2, 2), // invalid (B==C)
            insn_abc(cc_byte, 0, 2, 2), // invalid
            insn_abc(cc_byte, 0, 2, 2), // invalid
            insn_abc(0xDD, 0, 0, 0),
        ];
        let chunk = chunk_from_code(code, 6);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_concat(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[cc_byte as usize], 255,
            "detect_concat must reject byte with <80% B<C ratio (got 40%)"
        );
    }

    /// ---- detect_bitwise_ops additional coverage ----------------------------



    /// ---- detect_unary_not_minus additional coverage -------------------------

    #[test]
    fn detect_unary_not_minus_rejects_without_numeric_context() {
        // 4 unary-shaped candidates whose target register is NEVER read by an
        // arithmetic op afterward → ctx_hits == 0 → must NOT assign.
        let move_byte: u8 = 0x70;
        let cand: u8 = 0x71;
        let code = vec![
            insn_abc(cand, 1, 0, 0), // 0
            insn_abc(0xAA, 5, 5, 0), // 1: unrelated MOVE-like op (not a numeric consumer)
            insn_abc(cand, 2, 0, 0), // 2
            insn_abc(0xAA, 5, 5, 0), // 3
            insn_abc(cand, 3, 0, 0), // 4
            insn_abc(0xAA, 5, 5, 0), // 5
            insn_abc(cand, 4, 0, 0), // 6
            insn_abc(0xDD, 0, 0, 0), // 7
        ];
        let chunk = chunk_from_code(code, 8);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        ctx.map[move_byte as usize] = LuauOpcode::Move as u8;
        ctx.assigned[LuauOpcode::Move as usize] = true;

        detect_unary_not_minus(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[cand as usize], 255,
            "detect_unary_not_minus must reject candidate with zero numeric-consumer context"
        );
        assert!(!ctx.assigned[LuauOpcode::Not as usize]);
        assert!(!ctx.assigned[LuauOpcode::Minus as usize]);
    }

    /// ---- detect_loadnil coverage -------------------------------------------

    #[test]
    fn detect_loadnil_assigns_when_all_b_c_zero() {
        // LOADNIL: B=0, C=0, A<stack. Two+ hits required.
        let ln_byte: u8 = 0x40;
        let code = vec![
            insn_abc(ln_byte, 0, 0, 0),
            insn_abc(ln_byte, 1, 0, 0),
            insn_abc(ln_byte, 2, 0, 0),
            insn_abc(0xDD, 0, 0, 0),
        ];
        let chunk = chunk_from_code(code, 4);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_loadnil(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[ln_byte as usize],
            LuauOpcode::LoadNil as u8,
            "detect_loadnil must map byte with all-instances B=0,C=0"
        );
    }

    #[test]
    fn detect_loadnil_rejects_byte_with_any_nonzero_b_or_c() {
        // Even one instance with B!=0 or C!=0 must disqualify.
        let fake_byte: u8 = 0x41;
        let code = vec![
            insn_abc(fake_byte, 0, 0, 0),
            insn_abc(fake_byte, 1, 0, 0),
            insn_abc(fake_byte, 2, 1, 0), // B=1 — disqualifying
            insn_abc(0xDD, 0, 0, 0),
        ];
        let chunk = chunk_from_code(code, 4);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_loadnil(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[fake_byte as usize], 255,
            "detect_loadnil must reject byte with any B!=0 or C!=0 instance"
        );
    }

    /// ---- detect_loadb coverage ---------------------------------------------

    #[test]
    fn detect_loadb_assigns_when_b_is_boolean() {
        // LOADB: A<stack, B in {0,1}, C in {0,1}. Need >= 3 hits.
        let lb_byte: u8 = 0x42;
        let code = vec![
            insn_abc(lb_byte, 0, 0, 0), // false
            insn_abc(lb_byte, 0, 1, 0), // true
            insn_abc(lb_byte, 1, 0, 0), // false
            insn_abc(lb_byte, 1, 1, 0), // true
            insn_abc(0xDD, 0, 0, 0),
        ];
        let chunk = chunk_from_code(code, 4);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_loadb(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[lb_byte as usize],
            LuauOpcode::LoadB as u8,
            "detect_loadb must map byte where all B in {{0,1}}"
        );
    }

    #[test]
    fn detect_loadb_rejects_when_b_exceeds_one() {
        // B=2 disqualifies any candidate from LoadB.
        let fake: u8 = 0x43;
        let code = vec![
            insn_abc(fake, 0, 0, 0),
            insn_abc(fake, 0, 1, 0),
            insn_abc(fake, 0, 2, 0), // B=2 — invalid for boolean
            insn_abc(0xDD, 0, 0, 0),
        ];
        let chunk = chunk_from_code(code, 4);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_loadb(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[fake as usize], 255,
            "detect_loadb must reject byte with B>1"
        );
    }

    /// ---- detect_return coverage --------------------------------------------

    #[test]
    fn detect_return_picks_byte_at_proto_end() {
        // RETURN appears as the last instruction of every proto. With 3 protos
        // all ending in the same byte → 100% of protos → assigned.
        let ret_byte: u8 = 0x50;
        let mut chunk = chunk_from_code(
            vec![insn_abc(0xAA, 0, 0, 0), insn_abc(ret_byte, 0, 1, 0)],
            4,
        );
        for _ in 0..2 {
            chunk.protos.push(Proto {
                max_stack_size: 4,
                num_params: 0,
                num_upvalues: 0,
                is_vararg: false,
                flags: 0,
                typeinfo: None,
                code: vec![insn_abc(0xBB, 0, 0, 0), insn_abc(ret_byte, 0, 1, 0)],
                constants: Vec::new(),
                child_protos: Vec::new(),
                line_defined: 0,
                debug_name: None,
                line_info: None,
                debug_info: None,
            });
        }

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_return(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[ret_byte as usize],
            LuauOpcode::Return as u8,
            "detect_return must pick byte that terminates all protos"
        );
    }

    /// ---- detect_prepvarargs coverage ---------------------------------------

    #[test]
    fn detect_prepvarargs_maps_first_insn_of_vararg_proto() {
        // PREPVARARGS sits at pc 0 of every vararg proto with A == num_params.
        let pv_byte: u8 = 0x60;
        let mut chunk = chunk_from_code(
            vec![insn_abc(pv_byte, 2, 0, 0), insn_abc(0xDD, 0, 0, 0)],
            4,
        );
        // Mark proto 0 as vararg with 2 params (matches A=2 above).
        chunk.protos[0].is_vararg = true;
        chunk.protos[0].num_params = 2;

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_prepvarargs(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[pv_byte as usize],
            LuauOpcode::PrepVarargs as u8,
            "detect_prepvarargs must map first insn of vararg proto when A==num_params"
        );
    }

    #[test]
    fn detect_prepvarargs_skips_non_vararg_proto() {
        // Proto is NOT vararg → detector's gate excludes it → no assignment.
        let pv_byte: u8 = 0x61;
        let chunk = chunk_from_code(
            vec![insn_abc(pv_byte, 0, 0, 0), insn_abc(0xDD, 0, 0, 0)],
            4,
        );
        // Default proto is_vararg = false.

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_prepvarargs(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[pv_byte as usize], 255,
            "detect_prepvarargs must not assign from non-vararg protos"
        );
    }

    /// ---- detect_closeupvals coverage ---------------------------------------

    #[test]
    fn detect_closeupvals_maps_when_proto_creates_upval_closures() {
        // CloseUpvals: B=0, C=0. Detector requires the proto to either have
        // upvalues itself OR create child closures with upvalues.
        let cu_byte: u8 = 0x80;
        let mut chunk = chunk_from_code(
            vec![
                insn_abc(cu_byte, 0, 0, 0),
                insn_abc(cu_byte, 1, 0, 0),
                insn_abc(0xDD, 0, 0, 0),
            ],
            4,
        );
        // Outer proto has no upvalues but creates a child with 2 upvalues.
        chunk.protos[0].child_protos = vec![1];
        chunk.protos.push(Proto {
            max_stack_size: 4,
            num_params: 0,
            num_upvalues: 2,
            is_vararg: false,
            flags: 0,
            typeinfo: None,
            code: vec![insn_abc(0xDD, 0, 0, 0)],
            constants: Vec::new(),
            child_protos: Vec::new(),
            line_defined: 0,
            debug_name: None,
            line_info: None,
            debug_info: None,
        });

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_closeupvals(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[cu_byte as usize],
            LuauOpcode::CloseUpvals as u8,
            "detect_closeupvals must map B=0,C=0 byte in proto that creates upval closures"
        );
    }

    /// ---- Phase B0.44B: detect_xeq_single_hit_return_target coverage -----------

    /// Build a proto containing a single `if v == <const> then return x end`
    /// pattern using an arbitrary shuffled byte for the XEQ instruction.
    /// Layout (7 words):
    ///   0: <xeq_byte> A=0 D=3   ; forward jump skipping the return
    ///   1: <aux>                ; AUX: 0 for nil, 1 for bool-true
    ///   2: <loadk_byte>         ; body: LOADK (return value)
    ///   3: <return_byte>        ; body: RETURN
    ///   4: <loadk_byte>         ; post-return fall-through
    ///   5: <return_byte>
    fn build_xeq_return_proto(xeq_byte: u8, aux_low31: u32, not_flag: bool) -> Chunk {
        let return_byte = 0x82u8;
        let loadk_byte = 0x8Cu8;
        let aux = aux_low31 | (if not_flag { 1u32 << 31 } else { 0 });
        let code = vec![
            insn_ad(xeq_byte, 0, 3),                       // 0: XEQ A=0 D=+3
            aux,                                            // 1: AUX
            insn_ad(loadk_byte, 1, 0),                     // 2: LOADK R1 K0
            insn_abc(return_byte, 1, 2, 0),                // 3: RETURN R1
            insn_ad(loadk_byte, 1, 0),                     // 4: LOADK R1 K0 (tail)
            insn_abc(return_byte, 1, 2, 0),                // 5: RETURN R1
        ];
        let mut chunk = chunk_from_code(code, 4);
        chunk.protos[0].num_params = 1;
        // Seed return and loadk mappings so the detector can find them.
        chunk
    }

    fn seed_return_loadk_mappings(ctx: &mut DetectCtx) {
        // 0x82 -> RETURN, 0x8C -> LOADK.
        ctx.map[0x82u8 as usize] = LuauOpcode::Return as u8;
        ctx.assigned[LuauOpcode::Return as usize] = true;
        ctx.map[0x8Cu8 as usize] = LuauOpcode::LoadK as u8;
        ctx.assigned[LuauOpcode::LoadK as usize] = true;
    }

    #[test]
    fn detect_xeq_single_hit_assigns_knil_for_aux_zero() {
        // Single instance of an unmapped byte with AUX=0x00000000 (bits 0-30 all
        // zero) and jump target landing on a mapped RETURN → JumpXEqKNil.
        let xeq_byte: u8 = 0x47;
        let chunk = build_xeq_return_proto(xeq_byte, 0, false);
        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        seed_return_loadk_mappings(&mut ctx);

        detect_xeq_single_hit_return_target(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[xeq_byte as usize],
            LuauOpcode::JumpXEqKNil as u8,
            "single-hit 0x{:02X} with AUX=0 and RETURN body must map to JumpXEqKNil",
            xeq_byte
        );
    }

    #[test]
    fn detect_xeq_single_hit_assigns_kb_for_aux_one() {
        // Single instance with AUX=0x00000001 (bool-true) → JumpXEqKB.
        let xeq_byte: u8 = 0x2A;
        let chunk = build_xeq_return_proto(xeq_byte, 1, false);
        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        seed_return_loadk_mappings(&mut ctx);

        detect_xeq_single_hit_return_target(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[xeq_byte as usize],
            LuauOpcode::JumpXEqKB as u8,
            "single-hit 0x{:02X} with AUX=1 and RETURN body must map to JumpXEqKB",
            xeq_byte
        );
    }

    #[test]
    fn detect_xeq_single_hit_accepts_not_flag_in_aux() {
        // AUX with bit 31 set (NOT flag) and low 31 bits == 0 must still map to
        // JumpXEqKNil — the NOT flag just inverts the jump condition but the
        // underlying compare is still against nil.
        let xeq_byte: u8 = 0x48;
        let chunk = build_xeq_return_proto(xeq_byte, 0, true); // AUX = 0x80000000
        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        seed_return_loadk_mappings(&mut ctx);

        detect_xeq_single_hit_return_target(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[xeq_byte as usize],
            LuauOpcode::JumpXEqKNil as u8,
            "AUX=0x80000000 (NOT flag + nil) must still map to JumpXEqKNil"
        );
    }

    #[test]
    fn detect_xeq_single_hit_rejects_aux_out_of_range() {
        // AUX=5 (neither 0 nor 1) must NOT map to KNil/KB — that's a register
        // reference or a larger constant index.
        let xeq_byte: u8 = 0x49;
        let chunk = build_xeq_return_proto(xeq_byte, 5, false);
        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        seed_return_loadk_mappings(&mut ctx);

        detect_xeq_single_hit_return_target(&chunk, &mut ctx);

        assert!(
            !ctx.is_mapped(xeq_byte),
            "AUX=5 must not map to KNil or KB"
        );
    }

    #[test]
    fn detect_xeq_single_hit_rejects_no_return_in_body() {
        // Build a variant where the body between xeq and target_pc does NOT
        // contain a RETURN. The detector must leave the byte unmapped.
        let xeq_byte: u8 = 0x4A;
        let loadk_byte: u8 = 0x8Cu8;
        let filler_byte: u8 = 0x50u8; // something we don't map; no RETURN in body
        let code = vec![
            insn_ad(xeq_byte, 0, 3),         // 0: XEQ A=0 D=+3
            0x00000000u32,                    // 1: AUX=0 (would match KNil)
            insn_ad(loadk_byte, 1, 0),        // 2: LOADK
            insn_abc(filler_byte, 1, 0, 0),   // 3: FILLER (not return)
            insn_ad(loadk_byte, 1, 0),        // 4: LOADK post-jump
            insn_abc(filler_byte, 1, 0, 0),   // 5: FILLER
        ];
        let mut chunk = chunk_from_code(code, 4);
        chunk.protos[0].num_params = 1;

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        // Seed RETURN+LOADK — but the body between pc+2 and target_pc
        // (PCs 2 and 3) contains LOADK and FILLER only, no RETURN.
        // RETURN byte (0x82) is NOT in this chunk, so ctx.find_shuffled(Return)
        // returns None — detector bails immediately.
        // To truly test has_return = false, we need Return mapped to a byte
        // not present in the body. Map it to 0x83 (not used in chunk).
        ctx.map[0x83u8 as usize] = LuauOpcode::Return as u8;
        ctx.assigned[LuauOpcode::Return as usize] = true;
        ctx.map[loadk_byte as usize] = LuauOpcode::LoadK as u8;
        ctx.assigned[LuauOpcode::LoadK as usize] = true;

        detect_xeq_single_hit_return_target(&chunk, &mut ctx);

        assert!(
            !ctx.is_mapped(xeq_byte),
            "missing RETURN in jump body must leave byte unmapped"
        );
    }

    #[test]
    fn detect_xeq_single_hit_rejects_mixed_shape_bytes() {
        // Same byte appearing in two instances: one valid KNil shape, one
        // "other" shape (no RETURN in body). Detector must reject the byte
        // because not ALL instances match KNil/KB.
        let xeq_byte: u8 = 0x4B;
        let return_byte: u8 = 0x82;
        let loadk_byte: u8 = 0x8C;
        let code = vec![
            // Instance 1: valid KNil (jumps to RETURN body)
            insn_ad(xeq_byte, 0, 3),                // 0
            0x00000000u32,                           // 1: AUX=0
            insn_ad(loadk_byte, 1, 0),              // 2
            insn_abc(return_byte, 1, 2, 0),         // 3: RETURN
            // Instance 2: same byte but body has no RETURN
            insn_ad(xeq_byte, 0, 2),                // 4: D=+2 target=7
            0x00000000u32,                           // 5: AUX=0
            insn_abc(loadk_byte, 1, 0, 0),          // 6: LOADK (no RETURN)
            insn_abc(return_byte, 1, 2, 0),         // 7: post-target
        ];
        let mut chunk = chunk_from_code(code, 4);
        chunk.protos[0].num_params = 1;

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        seed_return_loadk_mappings(&mut ctx);

        detect_xeq_single_hit_return_target(&chunk, &mut ctx);

        // Byte 0x4B has insn_count=2, knil_match=1, other_shape=1.
        // My detector requires other_shape == 0, so 0x4B is not eligible.
        assert!(
            !ctx.is_mapped(xeq_byte),
            "mixed-shape bytes must not be assigned to KNil/KB"
        );
    }

    #[test]
    fn detect_xeq_single_hit_does_not_steal_mapped_byte() {
        // A byte already mapped to another opcode must not be reassigned by
        // this detector, even when its structural pattern matches KNil.
        let xeq_byte: u8 = 0x4D;
        let chunk = build_xeq_return_proto(xeq_byte, 0, false);
        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        seed_return_loadk_mappings(&mut ctx);
        // Pre-map the byte to JumpIfNotLE.
        ctx.map[xeq_byte as usize] = LuauOpcode::JumpIfNotLE as u8;
        ctx.assigned[LuauOpcode::JumpIfNotLE as usize] = true;

        detect_xeq_single_hit_return_target(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[xeq_byte as usize],
            LuauOpcode::JumpIfNotLE as u8,
            "pre-mapped byte must not be reassigned"
        );
        assert!(
            !ctx.assigned[LuauOpcode::JumpXEqKNil as usize],
            "JumpXEqKNil must remain unmapped when the only candidate is already taken"
        );
    }

    #[test]
    fn detect_move_rejects_byte_whose_operands_leave_the_register_window() {
        // A LOADN-shaped byte in a proto with a small register window: the small
        // literals decode as a valid MOVE, the large ones do not. The true MOVE
        // byte never produces the latter, so a byte that does must be vetoed even
        // though its conforming count is the highest in the chunk.
        let literal_byte: u8 = 0x71;
        let real_move: u8 = 0x72;
        let mut code = Vec::new();
        for _ in 0..8 {
            code.push(insn_abc(literal_byte, 1, 2, 0)); // looks like MOVE
        }
        for _ in 0..8 {
            code.push(insn_abc(literal_byte, 1, 200, 0)); // literal past the window
        }
        for _ in 0..4 {
            code.push(insn_abc(real_move, 1, 2, 0));
        }
        let chunk = chunk_from_code(code, 4);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_move(&chunk, &mut ctx);

        assert_ne!(
            ctx.map[literal_byte as usize],
            LuauOpcode::Move as u8,
            "a byte that half the time addresses a non-register is not MOVE"
        );
        assert_eq!(
            ctx.map[real_move as usize],
            LuauOpcode::Move as u8,
            "the fully-conforming byte must win despite its lower count"
        );
    }

    #[test]
    fn detect_conditional_jumps_rejects_byte_that_branches_out_of_the_proto() {
        // A LOADN-shaped byte: A is a register and D is positive, so it satisfies
        // the conditional-jump shape test, and it is far more frequent than any
        // real branch. What gives it away is that D is a literal, not a
        // displacement, so a good share of its "targets" fall past the end of the
        // proto — something a real branch never does.
        let literal_byte: u8 = 0x39;
        let mut code = Vec::new();
        for _ in 0..6 {
            code.push(insn_ad(literal_byte, 1, 3)); // lands in range
        }
        for _ in 0..6 {
            code.push(insn_ad(literal_byte, 1, 900)); // lands far past the proto
        }
        code.push(insn_abc(0xCC, 0, 0, 0));
        let chunk = chunk_from_code(code, 4);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_conditional_jumps(&chunk, &mut ctx);

        assert_ne!(ctx.map[literal_byte as usize], LuauOpcode::JumpIfNot as u8);
        assert_ne!(ctx.map[literal_byte as usize], LuauOpcode::JumpIf as u8);
    }

    #[test]
    fn detect_conditional_jumps_still_accepts_a_well_behaved_branch() {
        // The converse: every occurrence lands inside the proto, so the veto must
        // not interfere.
        let branch: u8 = 0x3A;
        let mut code = Vec::new();
        for _ in 0..6 {
            code.push(insn_ad(branch, 1, 4));
        }
        for _ in 0..6 {
            code.push(insn_abc(0xAA, 1, 0, 0));
        }
        let chunk = chunk_from_code(code, 4);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_conditional_jumps(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[branch as usize],
            LuauOpcode::JumpIfNot as u8,
            "a byte whose branches all stay in-proto must still be detectable"
        );
    }

    // ═══════════════════════════════════════════════════════════════
    // detect_loadk purity veto
    // ═══════════════════════════════════════════════════════════════

    #[test]
    fn detect_loadk_prefers_pure_low_count_candidate_over_impure_high_count() {
        // The shape that defeats absolute-count ranking. `impure` is a LOADN-like
        // byte: more occurrences, but a third of its D values run past the end of
        // the constant table because they are integer literals, not indices.
        // `pure` is the real LOADK: fewer occurrences, every D in range and every
        // referenced constant of a type LOADK can actually load.
        let pure: u8 = 0x31;
        let impure: u8 = 0x32;
        let mut code = Vec::new();
        for i in 0..6u8 {
            code.push(insn_ad(pure, 1, (i % 4) as i16));
        }
        for i in 0..9u8 {
            // 6 in range, 3 well past the 4-entry constant table
            let d = if i < 6 { (i % 4) as i16 } else { 900 };
            code.push(insn_ad(impure, 1, d));
        }
        let mut chunk = chunk_from_code(code, 4);
        chunk.protos[0].constants = vec![
            Constant::Number(1.0),
            Constant::String("a".into()),
            Constant::Number(2.0),
            Constant::String("b".into()),
        ];

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_loadk(&chunk, &mut ctx);

        assert_eq!(
            ctx.map[pure as usize],
            LuauOpcode::LoadK as u8,
            "the pure candidate must win even though it has fewer conforming hits"
        );
        assert_ne!(ctx.map[impure as usize], LuauOpcode::LoadK as u8);
    }

    #[test]
    fn detect_loadk_rejects_import_indexed_byte() {
        // GETIMPORT's D is an index into the constant table too, so it passes the
        // range test with 100% purity. What separates it is the constant TYPE:
        // imports are never loaded by LOADK. Measured on a real Roblox module,
        // the GETIMPORT byte scored 99.9% in-range but 0.1% loadable.
        let import_byte: u8 = 0x41;
        let code: Vec<u32> = (0..8u8).map(|i| insn_ad(import_byte, 1, (i % 2) as i16)).collect();
        let mut chunk = chunk_from_code(code, 4);
        chunk.protos[0].constants = vec![Constant::Import(0x4010_0000), Constant::Import(0x4020_0000)];

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_loadk(&chunk, &mut ctx);

        assert_ne!(
            ctx.map[import_byte as usize],
            LuauOpcode::LoadK as u8,
            "a byte whose constants are all imports is GETIMPORT, not LOADK"
        );
        assert!(!ctx.assigned[LuauOpcode::LoadK as usize]);
    }

    #[test]
    fn detect_loadk_counts_out_of_window_occurrences_as_impurity() {
        // Occurrences failing the `d >= 0 && a < max_stack` window are the very
        // evidence that the byte is not LOADK, yet the original accumulator only
        // incremented inside that window — so they could never show up as
        // impurity. A byte that is in-window a third of the time must be vetoed.
        let byte: u8 = 0x37;
        let mut code = Vec::new();
        for i in 0..4u8 {
            code.push(insn_ad(byte, 1, (i % 2) as i16));
        }
        for _ in 0..8 {
            code.push(insn_ad(byte, 200, 1)); // A beyond max_stack
        }
        let mut chunk = chunk_from_code(code, 4);
        chunk.protos[0].constants = vec![Constant::Number(1.0), Constant::String("x".into())];

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        detect_loadk(&chunk, &mut ctx);

        assert_ne!(ctx.map[byte as usize], LuauOpcode::LoadK as u8);
    }

    #[test]
    fn detect_forgloopinext_rejects_byte_far_too_frequent_for_its_pair_evidence() {
        // One structurally valid ForGPrepINext -> ForGLoopINext pair, but the
        // candidate loop byte occurs 40 times in the chunk. A real ForGLoopINext
        // is emitted once per ipairs loop, always as the target of its own prep,
        // so 1 match cannot account for 40 occurrences — the byte is something
        // else (on real Roblox bytecode it was LOADK, appearing 934 times).
        //
        // This matters more than a normal mis-assignment: the lifter emits
        // NOTHING for Deprecated61, so every stolen instruction disappears from
        // the output silently.
        let prep: u8 = 0x64;
        let loop_b: u8 = 0x35;
        let mut code = vec![
            insn_ad(prep, 0, 2),        // 0: prep, jumps to 3
            insn_abc(0xAA, 1, 0, 0),    // 1
            insn_abc(0xBB, 1, 1, 0),    // 2
            insn_ad(loop_b, 0, 30),     // 3: the one genuine-looking target
        ];
        // Bulk out the chunk with unrelated uses of the same byte.
        for _ in 0..39 {
            code.push(insn_ad(loop_b, 1, 5));
        }
        code.push(insn_abc(0xCC, 0, 0, 0));
        let chunk = chunk_from_code(code, 4);

        let mut ctx = DetectCtx::new();
        ctx.compute_frequencies(&chunk);
        ctx.try_assign_force(prep, LuauOpcode::ForGPrepINext as u8);

        detect_forgloopinext(&chunk, &mut ctx);

        assert_ne!(
            ctx.map[loop_b as usize],
            LuauOpcode::Deprecated61 as u8,
            "a byte occurring 40 times with a single pair match must not be claimed \
             as ForGLoopINext"
        );
        assert!(!ctx.assigned[LuauOpcode::Deprecated61 as usize]);
    }

    #[test]
    fn loop_byte_frequency_gate_admits_fully_explained_byte() {
        // The converse: when the pair evidence accounts for the byte's
        // occurrences, the gate must not interfere. Animate.lua's ~69 ipairs
        // loops produce ~69 pairs for ~69 occurrences.
        let mut ctx = DetectCtx::new();
        ctx.freq[0x35] = 69;
        assert!(loop_byte_frequency_is_plausible(&ctx, 0x35, 69));
        assert!(loop_byte_frequency_is_plausible(&ctx, 0x35, 7));
        assert!(!loop_byte_frequency_is_plausible(&ctx, 0x35, 6));
        // A byte that never occurs cannot contradict the evidence.
        assert!(loop_byte_frequency_is_plausible(&ctx, 0x99, 1));
    }

    // ═══════════════════════════════════════════════════════════════
    // Coverage / confidence accounting
    // ═══════════════════════════════════════════════════════════════

    /// Build an OpcodeMap directly from a final map plus the map that existed
    /// before speculative completion. Mirrors the shape lib.rs assembles.
    fn opmap_from(final_map: [u8; 256], pre: [u8; 256]) -> OpcodeMap {
        OpcodeMap {
            shuffled_to_standard: final_map,
            mapped_count: final_map.iter().filter(|&&v| v != 255).count(),
            heuristic_map: pre,
            heuristic_count: pre.iter().filter(|&&v| v != 255).count(),
            heuristic_evidence: [0u16; 256],
            pre_completion_map: pre,
        }
    }

    #[test]
    fn coverage_counts_instruction_positions_not_aux_words() {
        // GETIMPORT carries an AUX word. Choose an AUX payload whose low byte
        // collides with a real opcode byte: a naive frequency count would see
        // two occurrences of 0x11, the instruction walk must see one.
        let import_byte: u8 = 0x10;
        let other_byte: u8 = 0x11;
        let aux_colliding_with_other: u32 = 0x0000_0011;
        let chunk = chunk_from_code(
            vec![
                insn_abc(import_byte, 0, 0, 0),
                aux_colliding_with_other,
                insn_abc(other_byte, 1, 0, 0),
            ],
            4,
        );

        let mut map = [255u8; 256];
        map[import_byte as usize] = LuauOpcode::GetImport as u8;
        map[other_byte as usize] = LuauOpcode::Move as u8;

        let cov = opmap_from(map, map).coverage(&chunk);

        assert_eq!(cov.insn_words, 2, "AUX word must not count as an instruction");
        assert_eq!(cov.present_bytes, 2);
        assert_eq!(cov.present_confident, 2);
        assert_eq!(cov.confidence_pct(), 100);
    }

    #[test]
    fn coverage_separates_invented_from_unmapped_and_unused() {
        // Three bytes at instruction positions: one evidence-backed, one filled
        // in only by completion, one left unmapped entirely. Plus one mapping
        // for a byte the chunk never uses.
        let backed: u8 = 0x20;
        let invented: u8 = 0x21;
        let unmapped: u8 = 0x22;
        let unused: u8 = 0x23;
        let chunk = chunk_from_code(
            vec![
                insn_abc(backed, 0, 0, 0),
                insn_abc(backed, 1, 0, 0),
                insn_abc(invented, 2, 0, 0),
                insn_abc(unmapped, 3, 0, 0),
            ],
            4,
        );

        let mut pre = [255u8; 256];
        pre[backed as usize] = LuauOpcode::Move as u8;
        let mut final_map = pre;
        final_map[invented as usize] = LuauOpcode::LoadNil as u8;
        final_map[unused as usize] = LuauOpcode::Break as u8;

        let cov = opmap_from(final_map, pre).coverage(&chunk);

        assert_eq!(cov.present_bytes, 3);
        assert_eq!(cov.present_confident, 1);
        assert_eq!(cov.present_invented, 1, "completion-filled byte is not evidence");
        assert_eq!(cov.present_unmapped, 1, "unmapped byte is honest doubt, not invention");
        assert_eq!(cov.ghost_mappings, 1, "mapping for an absent byte is pure filler");
        assert_eq!(cov.insn_words, 4);
        assert_eq!(cov.insn_words_confident, 2);
        assert_eq!(cov.insn_words_invented, 1);
        assert_eq!(cov.confidence_pct(), 50);
    }

    #[test]
    fn canonical_map_is_fully_confident() {
        // A canonical translation is exact by construction. Nothing in it may
        // ever be reported as a guess, whatever confidence floors are added to
        // the shuffled path.
        let chunk = chunk_from_code(
            vec![
                insn_abc(LuauOpcode::Move as u8, 0, 1, 0),
                insn_abc(LuauOpcode::Return as u8, 0, 1, 0),
            ],
            4,
        );
        let cov = OpcodeMap::canonical_luau().coverage(&chunk);
        assert_eq!(cov.present_invented, 0);
        assert_eq!(cov.present_unmapped, 0);
        assert_eq!(cov.confidence_pct(), 100);
    }
}
