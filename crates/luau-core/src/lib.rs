pub mod parser;
pub mod disasm;
pub mod analysis;
pub mod decompiler;
pub mod ast;
pub mod roundtrip;

use anyhow::Result;

/// Decompiler version string, injected from Cargo.toml
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Install a ground-truth opmap (canonical shuffled-byte → opcode table)
/// obtained from an external source such as the an executor probe script. Ground
/// truth is applied with top priority in every subsequent `detect` call.
/// Pass `None` to clear any previously-installed ground truth.
pub fn set_ground_truth_opmap(map: Option<[u8; 256]>) {
    parser::opmap::set_ground_truth(map);
}

/// Read the ground-truth opmap currently installed, if any.
pub fn get_ground_truth_opmap() -> Option<[u8; 256]> {
    parser::opmap::get_ground_truth()
}

/// Parse a JSON blob (shape: `{ "0xNN": "OPCODE_NAME", ... }` or envelope
/// with `"mappings"` key) into a ground-truth `[u8; 256]` map. Returns
/// `None` on unparseable JSON. Malformed entries are silently skipped.
pub fn parse_ground_truth_json(json: &str) -> Option<[u8; 256]> {
    parser::ground_truth::parse_ground_truth_json(json)
}

/// Serialize a ground-truth map into pretty JSON (the inverse of
/// [`parse_ground_truth_json`]).
pub fn serialize_ground_truth_opmap(map: &[u8; 256]) -> String {
    parser::ground_truth::serialize_ground_truth(map)
}

/// Canonical opcode name → canonical byte value. `None` for "UNKNOWN" and
/// anything unrecognised. Accepts mixed case and surrounding whitespace.
pub fn opcode_name_to_byte(name: &str) -> Option<u8> {
    parser::ground_truth::opcode_name_to_byte(name)
}

/// Known-overridable conflict: when the cache maps a byte to a JumpXEq-family
/// opcode (78-81) but fresh detection says the same byte is the ForGLoop/Prep
/// pair (60 or 61), fresh wins. Reason: JumpXEq is detected via permissive AUX
/// heuristics (aux_low31 <= 1), which matches AD-format instructions without
/// AUX (like ForGLoopINext). The pair detector in detect_forgprep_inext_pair
/// uses structural pair evidence (ForGPrepINext → ForGLoopINext with matching
/// A register and valid target), which is much stronger than AUX pattern
/// matching. When a small script (no ForGPrepINext loops) seeds the cache
/// with a false JumpXEqKB assignment, a later code-rich script (Animate.lua
/// with 69 ForGLoopINext occurrences) must be able to override it.
///
pub fn fresh_overrides_cache(cache_canon: u8, fresh_canon: u8) -> bool {
    use parser::opcodes::LuauOpcode;
    let fresh_is_forgloop_pair = fresh_canon == LuauOpcode::Deprecated61 as u8
        || fresh_canon == LuauOpcode::ForGPrepINext as u8;
    if !fresh_is_forgloop_pair {
        return false;
    }
    // Allow override of JumpXEq family: small scripts can set 0x6F → JumpXEqKB via
    // permissive AUX heuristic, but Animate.lua's structural pair detection at
    // 0xC5 → ForGPrepINext + 0x6F → Deprecated61 must take precedence.
    let cache_is_jumpxeq = cache_canon >= LuauOpcode::JumpXEqKNil as u8
        && cache_canon <= LuauOpcode::JumpXEqKS as u8;
    cache_is_jumpxeq
}

/// Merge a fresh-heuristic opmap with a cached opmap — CACHE WINS on conflict.
///
/// The cache represents distilled consensus from many prior scripts (already
/// filtered through consolidate_cache's 80% agreement check), so it's more
/// authoritative than any single script's fresh heuristic detection. Fresh
/// entries are only applied where the cache has a gap AND the standard opcode
/// isn't already assigned elsewhere in the cache.
///
/// Without this cache-first ordering, a single noisy fresh detection
/// (e.g., wrong byte for MINUS) can shadow the cache's correct mapping,
/// poisoning per-script output even when 30+ prior scripts agree.
///
/// EXCEPTION: specific known-overridable patterns (see fresh_overrides_cache)
/// let fresh win, because the fresh detector used structural evidence stronger
/// than the cache's initial permissive heuristic.
fn merge_cache_first(fresh: &[u8; 256], cached: &[u8; 256]) -> [u8; 256] {
    // Step 1: Seed from the cache (authoritative consensus).
    let mut merged = *cached;
    let mut assigned_std = [false; 256];
    for &v in merged.iter() {
        if v != 255 { assigned_std[v as usize] = true; }
    }
    // Step 2: Layer fresh heuristic.
    //   - Fill gaps (cache has 255 at this index, fresh has a value not yet assigned).
    //   - Apply known-overridable conflicts (fresh_overrides_cache returns true).
    for (idx, &v) in fresh.iter().enumerate() {
        if v == 255 { continue; }
        let cur = merged[idx];
        if cur == 255 {
            if !assigned_std[v as usize] {
                merged[idx] = v;
                assigned_std[v as usize] = true;
            }
        } else if v != cur && fresh_overrides_cache(cur, v) {
            // Fresh's structural detection overrides cache's permissive heuristic.
            // Un-assign the displaced canonical so it remains available elsewhere
            // (either fresh places it at its correct byte, or it stays unmapped —
            // both preferable to leaving the wrong byte locked in the cache).
            assigned_std[cur as usize] = false;
            merged[idx] = v;
            assigned_std[v as usize] = true;
        }
    }
    merged
}

/// Phase B0.33: Fresh-first merge. Opposite of `merge_cache_first`.
///
/// The script's own structural detection wins on every byte. The cache is
/// consulted ONLY to fill in bytes the script's solo detection couldn't assign
/// (and where the canonical opcode isn't already claimed by the fresh map).
///
/// Rationale: cross-shuffle cache pollution. Different scripts can come from
/// different Roblox client shuffles, and the fingerprint-keyed variant cache
/// isn't always perfect — small scripts with identical short-prefix fingerprints
/// can end up sharing a variant even when their true shuffles differ. In that
/// case `merge_cache_first` forces the wrong mapping. `merge_fresh_first` lets
/// each script's own evidence override cache assumptions.
///
/// Risk: small scripts with noisy fresh detection can pick the wrong byte and
/// the cache can no longer correct them. Mitigation: fresh detectors are
/// already strict (see detect_jumpback's FORGLOOP-shape rejection).
// Intentionally retained but currently unwired (see the Phase B0.33 note at the
// cache-first merge call site): kept as a documented alternative merge strategy
// for possible future per-shuffle fingerprinting.
#[allow(dead_code)]
fn merge_fresh_first(fresh: &[u8; 256], cached: &[u8; 256]) -> [u8; 256] {
    // Step 1: Seed from fresh (per-script authoritative evidence).
    let mut merged = *fresh;
    let mut assigned_std = [false; 256];
    for &v in merged.iter() {
        if v != 255 { assigned_std[v as usize] = true; }
    }
    // Step 2: Cache fills only the gaps.
    for (idx, &v) in cached.iter().enumerate() {
        if v == 255 { continue; }
        if merged[idx] == 255 && !assigned_std[v as usize] {
            merged[idx] = v;
            assigned_std[v as usize] = true;
        }
    }
    merged
}

#[cfg(test)]
mod merge_tests {
    use super::{build_consensus_map, merge_cache_first, select_best_variant};

    /// The core bug: fresh says byte 0x0E is MINUS, cache says 0x2B is MINUS.
    /// Cache must win — fresh's wrong assignment must be dropped entirely.
    #[test]
    fn cache_wins_on_conflict_same_std_opcode() {
        let mut fresh = [255u8; 256];
        fresh[0x0E] = 51; // MINUS at wrong byte
        let mut cached = [255u8; 256];
        cached[0x2B] = 51; // MINUS at correct byte

        let merged = merge_cache_first(&fresh, &cached);
        assert_eq!(merged[0x2B], 51, "cache's MINUS byte must be preserved");
        assert_eq!(merged[0x0E], 255, "fresh's wrong MINUS byte must be dropped");
    }

    #[test]
    fn fresh_fills_gaps_not_covered_by_cache() {
        let mut fresh = [255u8; 256];
        fresh[0x10] = 7; // GETGLOBAL
        let mut cached = [255u8; 256];
        cached[0x20] = 6; // MOVE

        let merged = merge_cache_first(&fresh, &cached);
        assert_eq!(merged[0x20], 6, "cache MOVE preserved");
        assert_eq!(merged[0x10], 7, "fresh fills gap for GETGLOBAL");
    }

    #[test]
    fn empty_cache_falls_through_to_fresh() {
        let mut fresh = [255u8; 256];
        fresh[0x10] = 7;
        fresh[0x20] = 6;
        let cached = [255u8; 256];

        let merged = merge_cache_first(&fresh, &cached);
        assert_eq!(merged[0x10], 7);
        assert_eq!(merged[0x20], 6);
    }

    #[test]
    fn fresh_and_cache_agree() {
        let mut fresh = [255u8; 256];
        fresh[0x2B] = 51;
        let mut cached = [255u8; 256];
        cached[0x2B] = 51;

        let merged = merge_cache_first(&fresh, &cached);
        assert_eq!(merged[0x2B], 51);
        // No other slots should have been filled
        assert_eq!(merged.iter().filter(|&&v| v != 255).count(), 1);
    }

    /// Cache seeded with JumpXEqKB (canonical 79) at 0x35 by early small scripts,
    /// but fresh detection on Animate.lua finds the structural ForGPrepINext →
    /// ForGLoopINext pair that lands on 0x35 with canonical 61 (Deprecated61).
    /// Fresh must win because pair detection is structurally stronger than
    /// JumpXEq's permissive AUX heuristic.
    #[test]
    fn fresh_overrides_cache_jumpxeq_to_deprecated61() {
        let mut fresh = [255u8; 256];
        fresh[0x35] = 61; // Deprecated61 (ForGLoopINext) — from pair detector
        fresh[0x65] = 60; // ForGPrepINext — pair detected both

        let mut cached = [255u8; 256];
        cached[0x35] = 79; // JumpXEqKB — wrong, from earlier small script

        let merged = merge_cache_first(&fresh, &cached);
        assert_eq!(merged[0x35], 61, "fresh's Deprecated61 must override cache's JumpXEqKB");
        assert_eq!(merged[0x65], 60, "fresh's ForGPrepINext must fill gap");
    }

    /// Symmetric override: ForGPrepINext (canonical 60) vs JumpXEqKNil (78) at same byte.
    #[test]
    fn fresh_overrides_cache_jumpxeq_to_forgprep_inext() {
        let mut fresh = [255u8; 256];
        fresh[0x40] = 60; // ForGPrepINext — from pair detector

        let mut cached = [255u8; 256];
        cached[0x40] = 78; // JumpXEqKNil — false positive

        let merged = merge_cache_first(&fresh, &cached);
        assert_eq!(merged[0x40], 60, "fresh's ForGPrepINext must override cache's JumpXEqKNil");
    }

    /// Non-overridable conflict: cache MINUS at 0x2B vs fresh MINUS at 0x0E.
    /// The original cache-wins behavior must be preserved (fresh_overrides_cache
    /// returns false for this combo).
    #[test]
    fn non_overridable_conflict_cache_wins() {
        let mut fresh = [255u8; 256];
        fresh[0x0E] = 51; // MINUS at wrong byte
        let mut cached = [255u8; 256];
        cached[0x2B] = 51; // MINUS at correct byte

        let merged = merge_cache_first(&fresh, &cached);
        assert_eq!(merged[0x2B], 51, "cache's MINUS byte must be preserved");
        assert_eq!(merged[0x0E], 255, "fresh's wrong MINUS byte must be dropped");
    }

    /// B0.74: select_best_variant picks the variant with highest solo-detection
    /// agreement, not just the biggest.
    #[test]
    fn select_best_variant_picks_matching_shuffle() {
        // Variant A: maps 0x07 → LOADN (4), 0x9F → CALL (21)
        let mut va = [255u8; 256];
        va[0x07] = 4; // LOADN
        va[0x9F] = 21; // CALL
        va[0x82] = 22; // RETURN
        // Variant B: maps 0x03 → LOADN (4), 0xA9 → CALL (21) — different shuffle
        let mut vb = [255u8; 256];
        vb[0x03] = 4; // LOADN
        vb[0xA9] = 21; // CALL
        vb[0x82] = 22; // RETURN
        vb[0x30] = 16; // extra mapping — bigger variant

        let variants = vec![va, vb];

        // select_best_variant with empty/invalid bytecode should return Some
        // (falls back to biggest = variant B at index 1)
        let result = select_best_variant(&[], &variants);
        assert!(result.is_some());

        // With no variants, returns None
        assert_eq!(select_best_variant(&[], &[]), None);

        // Single variant always returns 0
        assert_eq!(select_best_variant(&[], &[va]), Some(0));
    }

    /// B0.75: consensus map picks highest-voted mapping per byte/opcode pair.
    #[test]
    fn consensus_map_picks_majority_voted_mappings() {
        // 3 variants: all agree RETURN→0x82, but disagree on ADD
        let mut v1 = [255u8; 256];
        v1[0x82] = 22; // RETURN
        v1[0x87] = 33; // ADD
        v1[0x43] = 34; // SUB

        let mut v2 = [255u8; 256];
        v2[0x82] = 22; // RETURN
        v2[0x87] = 33; // ADD — agrees with v1
        v2[0x43] = 35; // MUL — disagrees with v1 on 0x43

        let mut v3 = [255u8; 256];
        v3[0x82] = 22; // RETURN
        v3[0x26] = 33; // ADD — disagrees on which byte is ADD
        v3[0x43] = 34; // SUB — agrees with v1

        let consensus = build_consensus_map(&[v1, v2, v3]);
        // RETURN at 0x82: 3/3 votes — assigned
        assert_eq!(consensus[0x82], 22);
        // ADD at 0x87: 2/3 votes vs ADD at 0x26: 1/3 — 0x87 wins
        assert_eq!(consensus[0x87], 33);
        // SUB at 0x43: 2/3 votes — assigned (MUL at 0x43 only 1/3)
        assert_eq!(consensus[0x43], 34);

        // Gap filling: v3 has 0x26→ADD (1 vote, below threshold). But v3 is
        // the biggest variant if we add an extra mapping. Let's test with v1
        // being biggest: MUL (35) only in v2 at 0x43 (1 vote, below threshold
        // as SUB got 0x43). But v1 has no MUL. Add it to v1 to test gap fill.
        let mut v1b = v1;
        v1b[0xAA] = 35; // MUL — unique to biggest variant, should be gap-filled
        let consensus2 = build_consensus_map(&[v1b, v2, v3]);
        assert_eq!(consensus2[0x82], 22); // consensus
        assert_eq!(consensus2[0x87], 33); // consensus
        assert_eq!(consensus2[0x43], 34); // consensus
        assert_eq!(consensus2[0xAA], 35); // gap-filled from biggest (v1b)

        // Empty variants → all 255
        let empty = build_consensus_map(&[]);
        assert!(empty.iter().all(|&b| b == 255));

        // Single variant → exact copy
        let single = build_consensus_map(&[v1]);
        assert_eq!(single, v1);
    }

    /// Override only fires on the specific JumpXEq↔ForGLoop pattern. Other
    /// mis-detections must not trigger overrides (preserves cache authority).
    #[test]
    fn override_is_narrow_to_jumpxeq_vs_forgloop_pair() {
        let mut fresh = [255u8; 256];
        fresh[0x10] = 7; // GETGLOBAL — fresh wrong
        let mut cached = [255u8; 256];
        cached[0x10] = 79; // JumpXEqKB — cached, fresh has GETGLOBAL not Deprecated61

        let merged = merge_cache_first(&fresh, &cached);
        assert_eq!(merged[0x10], 79, "non-targeted override pattern must not fire");
    }
}

/// Select the best matching cached opmap variant for a given bytecode.
///
/// When the cache holds multiple variants (from different Roblox client
/// shuffles accumulated over time), blindly picking the biggest variant
/// can corrupt output for scripts compiled with a different shuffle.
///
/// This function parses the bytecode, runs a quick solo detection, and
/// picks the cached variant with the highest agreement on co-mapped bytes.
/// Returns the index of the best variant, or None if variants is empty.
pub fn select_best_variant(bytecode: &[u8], variants: &[[u8; 256]]) -> Option<usize> {
    if variants.is_empty() {
        return None;
    }
    // Try to parse and solo-detect. If parsing fails, fall back to biggest.
    // (If only one variant, we still want to SCORE it so a wrong-shuffle single
    // variant doesn't poison solo — see Phase B0.136 rationale below.)
    let chunk = match parser::parse(bytecode) {
        Ok(c) => c,
        Err(_) => {
            return Some(variants.iter().enumerate()
                .max_by_key(|(_, v)| v.iter().filter(|&&b| b != 255).count())
                .map(|(i, _)| i)
                .unwrap_or(0));
        }
    };
    if !parser::opmap::OpcodeMap::needs_remapping(&chunk) {
        return Some(0); // No remapping needed, variant choice irrelevant
    }
    let solo = parser::opmap::OpcodeMap::detect(&chunk);
    let solo_map = &solo.heuristic_map;

    // Score each variant by agreement with solo detection.
    // Agreement = number of bytes where both solo and variant map to the same
    // standard opcode (both non-255 and equal). Penalize conflicts.
    let mut best_idx = 0;
    let mut best_score: i32 = i32::MIN;
    for (idx, variant) in variants.iter().enumerate() {
        let mut agreements: i32 = 0;
        let mut conflicts: i32 = 0;
        for i in 0..256 {
            if solo_map[i] != 255 && variant[i] != 255 {
                if solo_map[i] == variant[i] {
                    agreements += 1;
                } else {
                    conflicts += 1;
                }
            }
        }
        // Score: agreements minus weighted conflicts. A conflict means
        // the variant definitely disagrees with this script's detection.
        let score = agreements - conflicts * 3;
        if score > best_score || (score == best_score
            && variant.iter().filter(|&&b| b != 255).count()
                > variants[best_idx].iter().filter(|&&b| b != 255).count())
        {
            best_score = score;
            best_idx = idx;
        }
    }
    // Phase B0.136: reject all variants when even the best has more weighted
    // conflicts than agreements with this script's solo detection. This happens
    // when the cache was seeded by scripts from a *different* Roblox shuffle
    // (different client version) and the current script's shuffle is not
    // represented in the cache. Returning None makes decompile_with_opmap fall
    // back to solo-only detection, which is strictly better than merging a
    // wrong-shuffle variant via merge_cache_first.
    //
    // Repro: VRVehicleCamera (solo=77 < SOLO_CONFIDENCE_THRESHOLD=83) against
    // the reference's 8-variant cache selected a variant that mapped 0x52→GETUPVAL
    // while solo correctly mapped 0x52→LOADK. Every LOADK then lifted as
    // GETUPVAL, producing 72 out-of-range `upval_N` emissions.
    if best_score <= 0 {
        return None;
    }
    Some(best_idx)
}

/// B0.75: Build a consensus opmap from multiple cached variants by majority voting.
///
/// Instead of picking ONE variant (which has random arithmetic assignments),
/// this builds a single map where each (shuffled_byte → standard_opcode) pair
/// is assigned based on how many variants agree on that mapping.
///
/// Uses greedy bipartite matching: sort all (vote_count, byte, opcode) triples
/// by vote count descending, then assign pairs that don't conflict with already-
/// assigned bytes or opcodes. This naturally resolves competition: structural
/// opcodes (RETURN, CALL) with 15/15 votes get assigned first, then arithmetic
/// opcodes fill in with whatever agreement exists (5/15, 3/15, etc.).
///
/// Uses a two-phase approach:
/// 1. Majority voting: assign high-confidence mappings (≥2 votes when 3+ variants)
/// 2. Gap filling: for opcodes left unmapped, inherit from the most complete variant
///    (the one with the most mapped opcodes that doesn't conflict with phase 1).
/// This gives structural stability from consensus while preserving coverage from
/// individual variant detections for rare/noisy opcodes like arithmetic.
pub fn build_consensus_map(variants: &[[u8; 256]]) -> [u8; 256] {
    let mut result = [255u8; 256];
    if variants.is_empty() {
        return result;
    }
    if variants.len() == 1 {
        return variants[0];
    }

    // Step 1: Count votes for each (shuffled_byte → standard_opcode) pair
    let mut votes = vec![[0u32; 256]; 256];
    for variant in variants {
        for (byte, &opcode) in variant.iter().enumerate() {
            if opcode != 255 {
                votes[byte][opcode as usize] += 1;
            }
        }
    }

    // Step 2: Build priority list sorted by vote count descending
    let mut triples: Vec<(u32, u8, u8)> = Vec::new();
    for byte in 0..256u16 {
        for opcode in 0..256u16 {
            let count = votes[byte as usize][opcode as usize];
            if count > 0 {
                triples.push((count, byte as u8, opcode as u8));
            }
        }
    }
    triples.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)).then_with(|| a.2.cmp(&b.2)));

    // Step 3: Greedy assignment — pick highest-voted pairs that don't conflict
    let mut byte_assigned = [false; 256];
    let mut opcode_assigned = [false; 256];
    let min_votes: u32 = if variants.len() >= 3 { 2 } else { 1 };
    for &(count, byte, opcode) in &triples {
        if count < min_votes {
            break;
        }
        if byte_assigned[byte as usize] || opcode_assigned[opcode as usize] {
            continue;
        }
        result[byte as usize] = opcode;
        byte_assigned[byte as usize] = true;
        opcode_assigned[opcode as usize] = true;
    }

    // Step 4: Gap filling — for opcodes still unmapped, use the remaining
    // single-vote triples (which were skipped by the min_votes threshold).
    // This ensures rare opcodes (DIV, DIVK, AND) that only one variant
    // detected still get mapped, as long as their byte isn't already claimed.
    // Process in vote-count order (all 1-vote triples) for determinism.
    for &(count, byte, opcode) in &triples {
        if count >= min_votes {
            continue; // already processed in Step 3
        }
        if byte_assigned[byte as usize] || opcode_assigned[opcode as usize] {
            continue;
        }
        result[byte as usize] = opcode;
        byte_assigned[byte as usize] = true;
        opcode_assigned[opcode as usize] = true;
    }

    result
}

/// Phase C1 stability guard: hard ceiling on emitted decompiled source
/// (in bytes). 20 MB is far above any legitimate hand-written or compiled
/// Roblox script; hitting this signals a lifter regression and we prefer a
/// clean structured error over streaming a multi-megabyte blob to the caller.
pub const PROTO_SOURCE_BAIL_BYTES: usize = 20 * 1024 * 1024; // 20 MB

/// Phase C1: returns `Err(...)` when `source_len` exceeds
/// [`PROTO_SOURCE_BAIL_BYTES`]. Pulled out of `decompile_with_opmap` so it
/// can be unit-tested without building a 20 MB bytecode fixture.
pub fn check_source_size_bail(source_len: usize) -> Result<()> {
    if source_len > PROTO_SOURCE_BAIL_BYTES {
        anyhow::bail!(
            "decompiled source exceeds {}-byte stability ceiling ({} bytes emitted)",
            PROTO_SOURCE_BAIL_BYTES,
            source_len
        );
    }
    Ok(())
}

/// High-level API: decompile raw bytecode into Luau source
pub fn decompile(bytecode: &[u8]) -> Result<String> {
    decompile_with_opmap(bytecode, None).map(|(source, _)| source)
}

/// Decompile with optional cached opcode map. Returns (source, detected_opmap).
/// If `cached_opmap` is provided and the bytecode uses shuffled opcodes, the cached
/// map is tried first. If it produces fewer unknowns than fresh detection, it's used.
/// The returned opmap is the HEURISTIC-only map (safe to cache, no speculative
/// completion). The decompilation itself uses the full merged+completed map.
pub fn decompile_with_opmap(
    bytecode: &[u8],
    cached_opmap: Option<&[u8; 256]>,
) -> Result<(String, Option<[u8; 256]>)> {
    let mut chunk = parser::parse(bytecode)?;

    // Canonical (non-Roblox) Luau bytecode needs a handful of opcodes lifted
    // with their real semantics rather than Roblox's passthrough behaviour.
    // Captured here because `is_canonical_luau` inspects the chunk before it
    // is remapped.
    let is_canonical_luau = !parser::opmap::OpcodeMap::needs_remapping(&chunk)
        && parser::opmap::OpcodeMap::is_canonical_luau(&chunk);

    // Auto-detect and apply opcode remapping for Roblox-shuffled bytecode
    let opmap_info = if parser::opmap::OpcodeMap::needs_remapping(&chunk) {
        // Phase B0.31: Self-detect-first. Always try solo detection before applying
        // any prior. If solo detection produces >= SOLO_CONFIDENCE_THRESHOLD opcodes,
        // the script has enough structural evidence to detect its own shuffle
        // reliably — skip the prior entirely to avoid cross-shuffle pollution.
        //
        // Background: prior observational data showed Animate.lua (96KB) solo-detects
        // 87 opcodes with 17 unresolved and emits 104KB of readable decompile, but
        // when seeded with a 47-small-script cache that detected a *different*
        // shuffle, its output collapsed to 8KB with 32 unresolved. Small scripts
        // lack evidence to uniquely pin their shuffle, so their solo detections can
        // disagree with large-script detections. Cross-seeding corrupts the
        // large-script pipeline.
        // Threshold tuning history:
        //   80: too permissive — idx 35 (10830b) solo=80 had 18 unresolved vs 2 with prior
        //   85: covered idx 45/46/47 at 86-87 solo, Phase B0.31 baseline (113 unresolved)
        //   83: Phase B0.33 — after detect_jumpback FORGLOOP-aware fix, idx 33 solo=83
        //       dropped to 1 unresolved (vs 5 with polluted prior). Lowering to 83 lets
        //       idx 33 use its own authoritative detection, reaching 75 corpus unresolved.
        //       idx 35 still at solo=80 → remains below threshold → uses prior (no regression).
        const SOLO_CONFIDENCE_THRESHOLD: usize = 83;
        let solo_detected = parser::opmap::OpcodeMap::detect(&chunk);
        let solo_is_authoritative = solo_detected.mapped_count >= SOLO_CONFIDENCE_THRESHOLD;

        let detected_opmap = if solo_is_authoritative {
            // Large script self-detects reliably. Ignore the prior entirely.
            solo_detected
        } else if let Some(cached) = cached_opmap {
            // Small script: use prior to help detection.
            parser::opmap::OpcodeMap::detect_with_prior(&chunk, cached)
        } else {
            solo_detected
        };

        let fresh_heuristic = detected_opmap.heuristic_map;

        let (decompile_map, mapped, cache_return_map) = if let Some(cached) = cached_opmap {
            if solo_is_authoritative {
                // Authoritative solo detection. Use it directly (no cache-first merge),
                // but return it as cache_return so the server can REPLACE the cached
                // variant if this script has more/better coverage.
                let mut map_for_decompile = fresh_heuristic;
                parser::opmap::OpcodeMap::permutation_complete_map(&mut map_for_decompile, &chunk);
                let mapped_count = map_for_decompile.iter().filter(|&&v| v != 255).count();

                let decompile_opmap = parser::opmap::OpcodeMap {
                    shuffled_to_standard: map_for_decompile,
                    mapped_count,
                    heuristic_map: fresh_heuristic,
                    heuristic_count: detected_opmap.heuristic_count,
                    heuristic_evidence: detected_opmap.heuristic_evidence,
                };
                (decompile_opmap, mapped_count, fresh_heuristic)
            } else {
                // Non-authoritative: fall back to original cache-first merge path.
                //
                // Phase B0.33: experimentally swapped to merge_fresh_first (letting
                // the script's own detection win on every byte) to test cross-shuffle
                // pollution hypothesis. Result: corpus unresolved 72 → 359. Small
                // scripts lack evidence to reliably pin their shuffle, their noisy
                // fresh detection picks wrong bytes, and without cache to correct
                // them, individual scripts regressed from 0-5 to 20-50 unresolved.
                // The cache-first merge is strictly better; fresh-only is the wrong
                // answer. Kept merge_fresh_first defined (unused) in case future
                // per-shuffle fingerprinting makes it useful.
                let mut merged = merge_cache_first(&fresh_heuristic, cached);
                let cache_map = merged;

                parser::opmap::OpcodeMap::permutation_complete_map(&mut merged, &chunk);
                let merged_count = merged.iter().filter(|&&v| v != 255).count();

                let decompile_opmap = parser::opmap::OpcodeMap {
                    shuffled_to_standard: merged,
                    mapped_count: merged_count,
                    heuristic_map: fresh_heuristic,
                    heuristic_count: detected_opmap.heuristic_count,
                    heuristic_evidence: detected_opmap.heuristic_evidence,
                };
                (decompile_opmap, merged_count, cache_map)
            }
        } else {
            // No cache — use the full detection for decompilation, return heuristic for caching.
            let mapped = detected_opmap.mapped_count;
            let cache_map = fresh_heuristic;
            (detected_opmap, mapped, cache_map)
        };

        // Return the heuristic-only map for safe caching (no speculative guesses).
        let (remap_unknowns, unknown_byte_freq, unknown_byte_sample) = decompile_map.remap_chunk(&mut chunk);
        Some((mapped, cache_return_map, remap_unknowns, unknown_byte_freq, unknown_byte_sample))
    } else if parser::opmap::OpcodeMap::is_canonical_luau(&chunk) {
        // Standard/canonical open-source Luau bytecode (e.g. from `luau-compile`).
        // It carries no Roblox opcode shuffle, but its canonical opcode numbering
        // differs from the Roblox layout this decompiler targets, so a plain
        // identity decode would misread opcodes such as DUPCLOSURE. Translate the
        // canonical numbering into the internal one and skip shuffle detection.
        // Not cached: a canonical map must never pollute the Roblox per-shuffle cache.
        let _ = parser::opmap::OpcodeMap::canonical_luau().remap_chunk(&mut chunk);
        None
    } else {
        None
    };

    let main_idx = chunk.main_proto as usize;
    if main_idx >= chunk.protos.len() {
        anyhow::bail!("invalid main_proto index {} (only {} protos)", main_idx, chunk.protos.len());
    }
    let main = &chunk.protos[main_idx];
    let mut ctx = decompiler::DecompileContext::new(&chunk);
    ctx.set_canonical_luau(is_canonical_luau);
    let source = decompiler::decompile_proto(&mut ctx, main, main_idx, 0);

    // Safety: if the decompiled source is absurdly large, truncate it
    // to prevent memory issues. This typically means expression duplication.
    const MAX_OUTPUT_CHARS: usize = 50_000_000; // 50MB — effectively unlimited
    let source = if source.len() > MAX_OUTPUT_CHARS {
        let mut truncated = source[..MAX_OUTPUT_CHARS].to_string();
        truncated.push_str(&format!(
            "\n-- TRUNCATED: output exceeded {} chars ({} total). Possible expression duplication.\n",
            MAX_OUTPUT_CHARS, source.len()
        ));
        truncated
    } else {
        source
    };

    // Phase C1 stability guard: hard 20 MB ceiling on emitted source. Unlike
    // the larger truncation above (which is effectively a diagnostic), this
    // surfaces as a structured `Err(...)` so the caller (HTTP handler, CLI)
    // can reject clearly instead of streaming a multi-megabyte blob back.
    // Runaway output past this point signals either a lifter regression or
    // a bytecode file we cannot reasonably process on the current build.
    check_source_size_bail(source.len())?;

    // Build the opcode map dump for diagnostics (shows shuffled→standard mapping)
    // Use the accurate unknown count from remap_chunk (which walks bytecode correctly,
    // properly skipping AUX words) instead of post-remap counting which misattributes
    // AUX words as unknown instructions.
    let opmap_dump = if let Some((_, ref detected, _, _, _)) = opmap_info {
        let map = detected;
        let mut dump = String::from("-- SHUFFLE MAP: shuffled_byte -> standard_opcode (name)\n");
        let mut mappings: Vec<(u8, u8)> = map.iter().enumerate()
            .filter(|(_, &v)| v != 255)
            .map(|(i, &v)| (i as u8, v))
            .collect();
        mappings.sort_by_key(|&(_, std)| std);
        for (shuffled, standard) in &mappings {
            let name = parser::opcodes::LuauOpcode::from_u8(*standard).name();
            dump.push_str(&format!("--   0x{:02X} -> {:2} {}\n", shuffled, standard, name));
        }
        Some(dump)
    } else {
        None
    };

    let mapped_count = opmap_info.as_ref().map(|(m, _, _, _, _)| *m);
    let unknown_insn_count = opmap_info.as_ref().map(|(_, _, u, _, _)| *u).unwrap_or(0);
    let unknown_byte_freq: Option<[u32; 256]> = opmap_info.as_ref().map(|(_, _, _, f, _)| *f);
    let unknown_byte_sample: Option<[Option<u32>; 256]> = opmap_info.as_ref().map(|(_, _, _, _, s)| *s);
    let returned_opmap = opmap_info.map(|(_, detected, _, _, _)| detected);

    // Add header with remap info
    if let Some(mapped) = mapped_count {
        let mut header = format!(
            "-- Luau Decompiler v{}\n-- Opcode remapping applied ({} opcodes detected)\n-- Protos: {} total, main={}\n",
            VERSION, mapped, chunk.protos.len(), chunk.main_proto
        );
        if unknown_insn_count > 0 {
            header.push_str(&format!("-- {} unresolved instructions (unmapped opcodes)\n", unknown_insn_count));
            // Emit per-byte breakdown of unresolved bytes with instruction pattern samples
            if let Some(ref freq) = unknown_byte_freq {
                let mut unresolved: Vec<(u8, u32)> = freq.iter().enumerate()
                    .filter(|(_, &c)| c > 0)
                    .map(|(b, &c)| (b as u8, c))
                    .collect();
                unresolved.sort_by(|a, b| b.1.cmp(&a.1)); // descending by count
                let parts: Vec<String> = unresolved.iter()
                    .map(|(b, c)| format!("0x{:02X}({})", b, c))
                    .collect();
                header.push_str(&format!("-- Unresolved bytes: {}\n", parts.join(", ")));

                // Diagnostic: for each unresolved byte, show sample A/B/C fields and
                // next-word value so we can identify what kind of opcode it is.
                if let Some(ref samples) = unknown_byte_sample {
                    header.push_str("-- Unresolved patterns (sample A,B,C,next_word):\n");
                    for (b, _c) in &unresolved {
                        if let Some(insn) = samples[*b as usize] {
                            let a = (insn >> 8) & 0xFF;
                            let rb = (insn >> 16) & 0xFF;
                            let c = (insn >> 24) & 0xFF;
                            let d = ((insn >> 16) as i16) as i32; // signed D field
                            header.push_str(&format!("--   0x{:02X}: A={} B={} C={} D={} raw=0x{:08X}\n", b, a, rb, c, d, insn));
                        }
                    }
                }
            }
        }
        // Phase C10U: SHUFFLE MAP is a ~30-50 line diagnostic dump useful only
        // when there are unresolved instructions to investigate. Emitting it
        // on every file adds ~10% pure-noise bytes to a clean corpus. Gate on
        // unknown_insn_count>0 so the diagnostic only appears when needed.
        if unknown_insn_count > 0 {
            if let Some(ref dump) = opmap_dump {
                header.push_str(dump);
            }
        }
        header.push_str(&source);
        Ok((header, returned_opmap))
    } else {
        let versioned = format!("-- Luau Decompiler v{}\n{}", VERSION, source);
        Ok((versioned, None))
    }
}

/// Lightweight opcode detection only (no decompilation).
/// Parses bytecode, runs opcode detection, merges with cached map, returns the
/// HEURISTIC-only detected opmap (safe to cache, no speculative completion).
/// Used for pre-scanning all scripts to build a complete cache before decompiling.
pub fn scan_opmap(
    bytecode: &[u8],
    cached_opmap: Option<&[u8; 256]>,
) -> Result<Option<[u8; 256]>> {
    let chunk = parser::parse(bytecode)?;

    if parser::opmap::OpcodeMap::needs_remapping(&chunk) {
        // Seed per-script detection with the cache (same architecture as decompile_with_opmap).
        let fresh_opmap = if let Some(cached) = cached_opmap {
            parser::opmap::OpcodeMap::detect_with_prior(&chunk, cached)
        } else {
            parser::opmap::OpcodeMap::detect(&chunk)
        };

        // Use heuristic map (pre-completion) — only high-confidence detections.
        // Safety-net merge in case validation dropped a cached entry.
        let result_map = if let Some(cached) = cached_opmap {
            merge_cache_first(&fresh_opmap.heuristic_map, cached)
        } else {
            fresh_opmap.heuristic_map
        };

        Ok(Some(result_map))
    } else {
        Ok(None)
    }
}

/// Quick peek at the bytecode version (first byte). Returns None if empty or error bytecode (version 0).
pub fn bytecode_version(bytecode: &[u8]) -> Option<u8> {
    bytecode.first().copied().filter(|&v| v >= 3 && v <= 8)
}

/// High-level API: disassemble raw bytecode into readable text
pub fn disassemble(bytecode: &[u8], show_debug: bool) -> Result<String> {
    let chunk = parser::parse(bytecode)?;
    Ok(disasm::disassemble(&chunk, show_debug))
}

/// Disassemble with opmap applied — applies the same shuffle detection/remap
/// as decompile_with_opmap, then disassembles the remapped chunk. This is the
/// diagnostic view of what the lifter actually processes.
pub fn disassemble_with_opmap(
    bytecode: &[u8],
    cached_opmap: Option<&[u8; 256]>,
) -> Result<String> {
    let mut chunk = parser::parse(bytecode)?;

    let mut header = String::new();
    if parser::opmap::OpcodeMap::needs_remapping(&chunk) {
        let fresh_opmap = if let Some(cached) = cached_opmap {
            parser::opmap::OpcodeMap::detect_with_prior(&chunk, cached)
        } else {
            parser::opmap::OpcodeMap::detect(&chunk)
        };
        let fresh_heuristic = fresh_opmap.heuristic_map;

        let decompile_map = if let Some(cached) = cached_opmap {
            let mut merged = merge_cache_first(&fresh_heuristic, cached);
            parser::opmap::OpcodeMap::permutation_complete_map(&mut merged, &chunk);
            let merged_count = merged.iter().filter(|&&v| v != 255).count();
            parser::opmap::OpcodeMap {
                shuffled_to_standard: merged,
                mapped_count: merged_count,
                heuristic_map: fresh_heuristic,
                heuristic_count: fresh_opmap.heuristic_count,
                heuristic_evidence: fresh_opmap.heuristic_evidence,
            }
        } else {
            fresh_opmap
        };

        header.push_str(&format!("; Luau bytecode v{} — remapped ({} opcodes)\n",
            chunk.version, decompile_map.mapped_count));
        header.push_str("; SHUFFLE MAP: shuffled_byte -> standard_opcode (name)\n");
        let mut mappings: Vec<(u8, u8)> = decompile_map.shuffled_to_standard.iter().enumerate()
            .filter(|(_, &v)| v != 255)
            .map(|(i, &v)| (i as u8, v))
            .collect();
        mappings.sort_by_key(|&(_, std)| std);
        for (shuffled, standard) in &mappings {
            let name = parser::opcodes::LuauOpcode::from_u8(*standard).name();
            header.push_str(&format!(";   0x{:02X} -> {:2} {}\n", shuffled, standard, name));
        }
        header.push('\n');

        let (_unknowns, _, _) = decompile_map.remap_chunk(&mut chunk);
    } else if parser::opmap::OpcodeMap::is_canonical_luau(&chunk) {
        // Canonical open-source Luau bytecode: no shuffle, but its opcode
        // numbering must be translated into the internal (Roblox) layout.
        let (_unknowns, _, _) = parser::opmap::OpcodeMap::canonical_luau().remap_chunk(&mut chunk);
        header.push_str(&format!(
            "; Luau bytecode v{} — canonical (no shuffle; canonical opcode numbering translated)\n\n",
            chunk.version
        ));
    } else {
        header.push_str(&format!("; Luau bytecode v{} — no remapping needed\n\n", chunk.version));
    }

    let body = disasm::disassemble(&chunk, true);
    Ok(format!("{}{}", header, body))
}

/// High-level API: parse and return structured info about the bytecode
pub fn info(bytecode: &[u8]) -> Result<BytecodeInfo> {
    let chunk = parser::parse(bytecode)?;
    Ok(BytecodeInfo {
        version: chunk.version,
        types_version: chunk.types_version,
        num_protos: chunk.protos.len(),
        num_strings: chunk.strings.len(),
        main_proto: chunk.main_proto as usize,
        protos: chunk
            .protos
            .iter()
            .enumerate()
            .map(|(i, p)| ProtoInfo {
                index: i,
                name: p.debug_name.clone(),
                num_params: p.num_params,
                num_upvalues: p.num_upvalues,
                max_stack: p.max_stack_size,
                is_vararg: p.is_vararg,
                num_instructions: p.code.len(),
                num_constants: p.constants.len(),
                num_children: p.child_protos.len(),
                line_defined: p.line_defined,
                has_debug_info: p.debug_info.is_some(),
                has_line_info: p.line_info.is_some(),
            })
            .collect(),
    })
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BytecodeInfo {
    pub version: u8,
    pub types_version: u8,
    pub num_protos: usize,
    pub num_strings: usize,
    pub main_proto: usize,
    pub protos: Vec<ProtoInfo>,
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ProtoInfo {
    pub index: usize,
    pub name: Option<String>,
    pub num_params: u8,
    pub num_upvalues: u8,
    pub max_stack: u8,
    pub is_vararg: bool,
    pub num_instructions: usize,
    pub num_constants: usize,
    pub num_children: usize,
    pub line_defined: u32,
    pub has_debug_info: bool,
    pub has_line_info: bool,
}

#[cfg(test)]
mod phase_c1_stability_tests {
    //! Phase C1 stability guards: the 20 MB output-source ceiling and the
    //! proto-wide statement-budget sentinel added to the lifter.

    use super::{check_source_size_bail, PROTO_SOURCE_BAIL_BYTES};
    use crate::ast::Stat;
    use crate::decompiler::lifter::{
        note_stmts_pushed, reset_stmt_budget, stmt_budget_tripped,
        MAX_STMTS_PER_PROTO,
    };

    #[test]
    fn c1_source_size_bail_returns_ok_at_ceiling() {
        // Exactly at the ceiling is allowed — we only bail strictly above.
        assert!(check_source_size_bail(0).is_ok());
        assert!(check_source_size_bail(1).is_ok());
        assert!(check_source_size_bail(PROTO_SOURCE_BAIL_BYTES).is_ok());
    }

    #[test]
    fn c1_source_size_bail_returns_err_above_ceiling() {
        let err = check_source_size_bail(PROTO_SOURCE_BAIL_BYTES + 1)
            .expect_err("should bail when source exceeds 20 MB");
        let msg = format!("{}", err);
        assert!(
            msg.contains("20 MB") || msg.contains("stability ceiling"),
            "error should mention stability ceiling, got: {msg}",
        );
        assert!(check_source_size_bail(50 * 1024 * 1024).is_err());
    }

    #[test]
    fn c1_statement_budget_emits_sentinel_comment_on_overrun() {
        // After `note_stmts_pushed` is called with a count that blows past
        // the cap, the block should be truncated and the sentinel comment
        // appended exactly once.
        reset_stmt_budget();
        // Build a block overshooting the cap.
        let mut block: Vec<Stat> = (0..(MAX_STMTS_PER_PROTO + 32))
            .map(|i| Stat::Comment(format!("stmt {i}")))
            .collect();
        let pushed = block.len();
        note_stmts_pushed(&mut block, pushed);
        assert!(stmt_budget_tripped(), "budget should be tripped");
        let last = block.last().expect("block has a last element");
        match last {
            Stat::Comment(c) => assert_eq!(c, "-- statement budget exceeded"),
            other => panic!("expected sentinel comment, got {other:?}"),
        }
        // Block should have been trimmed to MAX_STMTS_PER_PROTO + 1 (cap + sentinel).
        assert_eq!(block.len(), MAX_STMTS_PER_PROTO + 1);
    }

    #[test]
    fn c1_statement_budget_is_silent_below_ceiling() {
        reset_stmt_budget();
        let mut block: Vec<Stat> = (0..100)
            .map(|i| Stat::Comment(format!("stmt {i}")))
            .collect();
        note_stmts_pushed(&mut block, 100);
        assert!(!stmt_budget_tripped(), "budget should not trip at 100");
        assert_eq!(block.len(), 100);
        // No sentinel injected.
        assert!(!matches!(
            block.last(),
            Some(Stat::Comment(c)) if c == "-- statement budget exceeded"
        ));
    }

    #[test]
    fn c1_reset_stmt_budget_clears_trip_flag() {
        reset_stmt_budget();
        let mut block: Vec<Stat> = (0..(MAX_STMTS_PER_PROTO + 1))
            .map(|i| Stat::Comment(format!("stmt {i}")))
            .collect();
        let len = block.len();
        note_stmts_pushed(&mut block, len);
        assert!(stmt_budget_tripped());
        // A subsequent proto should start fresh.
        reset_stmt_budget();
        assert!(!stmt_budget_tripped());
    }
}
