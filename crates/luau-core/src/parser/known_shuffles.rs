//! Hardcoded known opcode shuffle maps for Roblox bytecode v6.
//!
//! Roblox permutes Luau opcodes per client version. This module contains
//! 7 known shuffle variants extracted from real Roblox bytecode, plus a
//! function to match a partially-detected opmap against these known variants
//! and fill in gaps.
//!
//! Each map is a `[u8; 256]` where `index = shuffled_byte` and
//! `value = standard Luau opcode`. A value of 255 means unmapped.

/// Number of known shuffle variants.
const NUM_VARIANTS: usize = 7;

/// All known shuffle maps. Index into this array by variant number (0-6).
static KNOWN_SHUFFLES: [[u8; 256]; NUM_VARIANTS] = [
    make_variant_0(),
    make_variant_1(),
    make_variant_2(),
    make_variant_3(),
    make_variant_4(),
    make_variant_5(),
    make_variant_6(),
];

/// Variant 0 — observed across 7 files, 70 mapped opcodes. Most complete.
const fn make_variant_0() -> [u8; 256] {
    let mut m = [255u8; 256];
    m[0xFE] = 0;
    m[0xFD] = 1;
    m[0x03] = 2;
    m[0x02] = 3;
    m[0x09] = 4;
    m[0xFB] = 5;
    m[0xDE] = 6;
    m[0xFF] = 7;
    m[0x60] = 8;
    m[0x0E] = 9;
    m[0xA9] = 10;
    m[0x00] = 11;
    m[0xA4] = 12;
    m[0x56] = 13;
    m[0xF0] = 14;
    m[0x4D] = 15;
    m[0x30] = 16;
    m[0x87] = 17;
    m[0x73] = 18;
    m[0xD9] = 19;
    m[0xBC] = 20;
    m[0x9F] = 21;
    m[0x82] = 22;
    m[0x65] = 23;
    m[0x6E] = 24;
    m[0x52] = 25;
    m[0x6F] = 26;
    m[0xF1] = 27;
    m[0x0D] = 29;
    m[0x7D] = 30;
    m[0x9A] = 32;
    m[0x43] = 33;
    m[0x26] = 34;
    m[0x6A] = 35;
    m[0xEC] = 36;
    m[0x90] = 37;
    m[0x5B] = 38;
    m[0x95] = 39;
    m[0x04] = 40;
    m[0x05] = 41;
    m[0x78] = 42;
    m[0x3E] = 43;
    m[0x1C] = 44;
    m[0x84] = 45;
    m[0x1A] = 46;
    m[0x7E] = 47;
    m[0x08] = 48;
    m[0x2B] = 50;
    m[0xC6] = 51;
    m[0x39] = 52;
    m[0x06] = 53;
    m[0xE2] = 54;
    m[0xA8] = 56;
    m[0x8B] = 57;
    m[0x9E] = 59;
    m[0xFA] = 63;
    m[0x75] = 64;
    m[0xA3] = 65;
    m[0x8C] = 67;
    m[0xFC] = 69;
    m[0x12] = 70;
    m[0x0A] = 71;
    m[0x0F] = 72;
    m[0x4C] = 73;
    m[0x34] = 74;
    m[0x01] = 76;
    m[0x10] = 77;
    m[0xB7] = 81;
    m[0xC0] = 82;
    m[0x23] = 83;
    m
}

/// Variant 1 — observed across 186 files, 54 mapped opcodes.
const fn make_variant_1() -> [u8; 256] {
    let mut m = [255u8; 256];
    m[0xFE] = 0;
    m[0xFD] = 1;
    m[0x08] = 2;
    m[0x04] = 3;
    m[0x52] = 5;
    m[0x6F] = 6;
    m[0xFF] = 7;
    m[0x7D] = 8;
    m[0x02] = 9;
    m[0xC6] = 10;
    m[0x05] = 11;
    m[0xA4] = 12;
    m[0x12] = 13;
    m[0xA9] = 14;
    m[0x4D] = 15;
    m[0x30] = 16;
    m[0xBC] = 20;
    m[0x9F] = 21;
    m[0x82] = 22;
    m[0x65] = 23;
    m[0x6E] = 24;
    m[0xFB] = 25;
    m[0x0E] = 26;
    m[0xF1] = 27;
    m[0x47] = 29;
    m[0x9A] = 30;
    m[0xB7] = 32;
    m[0x87] = 33;
    m[0x11] = 39;
    m[0x8C] = 40;
    m[0x1C] = 43;
    m[0x78] = 44;
    m[0x03] = 45;
    m[0x09] = 46;
    m[0x01] = 47;
    m[0x73] = 49;
    m[0x13] = 50;
    m[0xD4] = 51;
    m[0x56] = 52;
    m[0x10] = 53;
    m[0xE2] = 54;
    m[0xD9] = 55;
    m[0xA8] = 56;
    m[0x8B] = 57;
    m[0xC5] = 59;
    m[0xFA] = 63;
    m[0x15] = 64;
    m[0xA3] = 65;
    m[0xFC] = 69;
    m[0x00] = 70;
    m[0x9E] = 73;
    m[0x34] = 74;
    m[0x60] = 81;
    m[0xC0] = 82;
    m
}

/// Variant 2 — observed across 158 files, 43 mapped opcodes.
const fn make_variant_2() -> [u8; 256] {
    let mut m = [255u8; 256];
    m[0xFE] = 0;
    m[0xFD] = 1;
    m[0xC6] = 2;
    m[0xA9] = 3;
    m[0x8C] = 5;
    m[0x6F] = 6;
    m[0x7D] = 7;
    m[0xBC] = 8;
    m[0xA4] = 12;
    m[0x87] = 13;
    m[0x6A] = 14;
    m[0x4D] = 15;
    m[0xFF] = 16;
    m[0x9F] = 17;
    m[0x05] = 19;
    m[0x6E] = 20;
    m[0x52] = 21;
    m[0x82] = 22;
    m[0x65] = 23;
    m[0x48] = 24;
    m[0x47] = 25;
    m[0x95] = 26;
    m[0xB7] = 30;
    m[0xC5] = 33;
    m[0x0E] = 39;
    m[0x00] = 40;
    m[0x02] = 41;
    m[0x01] = 42;
    m[0x1C] = 43;
    m[0x78] = 44;
    m[0x07] = 45;
    m[0x73] = 49;
    m[0x2B] = 50;
    m[0x10] = 53;
    m[0xA8] = 56;
    m[0x8B] = 57;
    m[0xFB] = 63;
    m[0x06] = 64;
    m[0xA3] = 65;
    m[0xFC] = 69;
    m[0x34] = 74;
    m[0x60] = 81;
    m[0xC0] = 82;
    m
}

/// Variant 3 — observed across 123 files, 61 mapped opcodes.
const fn make_variant_3() -> [u8; 256] {
    let mut m = [255u8; 256];
    m[0xFE] = 0;
    m[0xFD] = 1;
    m[0x08] = 2;
    m[0x06] = 3;
    m[0x04] = 4;
    m[0x6F] = 5;
    m[0x0A] = 6;
    m[0xFF] = 7;
    m[0x7D] = 8;
    m[0xFB] = 9;
    m[0x02] = 10;
    m[0x07] = 11;
    m[0xA4] = 12;
    m[0x6A] = 13;
    m[0x8C] = 14;
    m[0x4D] = 15;
    m[0x30] = 16;
    m[0x87] = 17;
    m[0xBC] = 20;
    m[0x9F] = 21;
    m[0x82] = 22;
    m[0x65] = 23;
    m[0x6E] = 24;
    m[0x0E] = 25;
    m[0x52] = 26;
    m[0x47] = 27;
    m[0xD9] = 28;
    m[0x0D] = 29;
    m[0x60] = 30;
    m[0xF1] = 31;
    m[0xB7] = 32;
    m[0x09] = 33;
    m[0xEC] = 34;
    m[0xC6] = 39;
    m[0x00] = 40;
    m[0x21] = 41;
    m[0x11] = 42;
    m[0x2B] = 43;
    m[0x78] = 44;
    m[0x0B] = 45;
    m[0x14] = 46;
    m[0x05] = 47;
    m[0x73] = 49;
    m[0x13] = 50;
    m[0x1C] = 51;
    m[0x64] = 52;
    m[0x01] = 53;
    m[0xE2] = 54;
    m[0xA8] = 56;
    m[0x8B] = 57;
    m[0xC5] = 59;
    m[0xFA] = 63;
    m[0x15] = 64;
    m[0xA3] = 65;
    m[0x4C] = 68;
    m[0xFC] = 69;
    m[0x12] = 70;
    m[0x9E] = 73;
    m[0x34] = 74;
    m[0xF0] = 81;
    m[0xC0] = 82;
    m
}

/// Variant 4 — observed across 75 files, 35 mapped opcodes.
const fn make_variant_4() -> [u8; 256] {
    let mut m = [255u8; 256];
    m[0xFE] = 0;
    m[0xFD] = 1;
    m[0x08] = 2;
    m[0xA9] = 3;
    m[0x87] = 4;
    m[0x8C] = 5;
    m[0x03] = 6;
    m[0x35] = 7;
    m[0x4D] = 8;
    m[0x30] = 15;
    m[0xBC] = 16;
    m[0x9F] = 17;
    m[0x05] = 19;
    m[0x18] = 20;
    m[0x52] = 21;
    m[0x82] = 22;
    m[0xF0] = 25;
    m[0xA4] = 26;
    m[0x6E] = 27;
    m[0xB7] = 30;
    m[0x6A] = 34;
    m[0x07] = 45;
    m[0x95] = 46;
    m[0x02] = 47;
    m[0x73] = 49;
    m[0x1C] = 50;
    m[0xFF] = 51;
    m[0x00] = 53;
    m[0xA8] = 56;
    m[0x8B] = 57;
    m[0xFB] = 63;
    m[0x01] = 64;
    m[0xA3] = 65;
    m[0xFC] = 69;
    m[0x60] = 81;
    m[0xC0] = 82;
    m
}

/// Variant 5 — observed across 35 files, 63 mapped opcodes.
const fn make_variant_5() -> [u8; 256] {
    let mut m = [255u8; 256];
    m[0xFE] = 0;
    m[0xFD] = 1;
    m[0x04] = 2;
    m[0x02] = 3;
    m[0x0B] = 4;
    m[0xFB] = 5;
    m[0xC6] = 6;
    m[0x6E] = 7;
    m[0xFF] = 8;
    m[0x01] = 9;
    m[0x00] = 10;
    m[0x16] = 11;
    m[0xA4] = 12;
    m[0xA9] = 13;
    m[0x09] = 14;
    m[0x4D] = 15;
    m[0x30] = 16;
    m[0x87] = 17;
    m[0xC5] = 18;
    m[0xD9] = 19;
    m[0xBC] = 20;
    m[0x9F] = 21;
    m[0x82] = 22;
    m[0x65] = 23;
    m[0x52] = 25;
    m[0x6F] = 26;
    m[0xF0] = 27;
    m[0xB7] = 28;
    m[0xF1] = 29;
    m[0x47] = 30;
    m[0x0D] = 32;
    m[0x43] = 33;
    m[0x78] = 34;
    m[0x26] = 35;
    m[0xDE] = 39;
    m[0x21] = 40;
    m[0x95] = 41;
    m[0x11] = 42;
    m[0x8E] = 45;
    m[0x6B] = 46;
    m[0x06] = 47;
    m[0x7B] = 48;
    m[0xEC] = 49;
    m[0x2B] = 50;
    m[0x64] = 51;
    m[0x3E] = 52;
    m[0x0E] = 53;
    m[0xE2] = 54;
    m[0xA8] = 56;
    m[0x8B] = 57;
    m[0xFA] = 63;
    m[0x62] = 64;
    m[0xA3] = 65;
    m[0x8C] = 67;
    m[0x4C] = 68;
    m[0xFC] = 69;
    m[0x12] = 70;
    m[0xBB] = 73;
    m[0x34] = 74;
    m[0x60] = 79;
    m[0x9A] = 81;
    m[0xC0] = 82;
    m[0x3B] = 83;
    m
}

/// Variant 6 — observed across 30 files, 23 mapped opcodes. Least complete.
const fn make_variant_6() -> [u8; 256] {
    let mut m = [255u8; 256];
    m[0xFE] = 0;
    m[0xFD] = 1;
    m[0x01] = 2;
    m[0x87] = 4;
    m[0x52] = 5;
    m[0x08] = 6;
    m[0x4D] = 15;
    m[0xFF] = 16;
    m[0x9F] = 17;
    m[0x05] = 19;
    m[0x8C] = 21;
    m[0x82] = 22;
    m[0xA4] = 26;
    m[0x78] = 33;
    m[0x6A] = 34;
    m[0x95] = 45;
    m[0x73] = 49;
    m[0x00] = 53;
    m[0xA8] = 56;
    m[0x8B] = 57;
    m[0xFB] = 63;
    m[0xA3] = 65;
    m[0xFC] = 69;
    m
}

/// Given a partially-detected opmap, find the best matching known shuffle
/// variant and return a merged map that fills in the gaps.
///
/// For each known variant, every byte that is mapped in *both* the partial map
/// and the known variant must agree (otherwise the variant is incompatible).
/// Among compatible variants, the one that contributes the most additional
/// mappings (bytes that are unmapped in the partial map but mapped in the
/// known variant) wins.
///
/// The returned map is the partial map with gaps filled from the best variant.
/// Returns `None` if no known variant is compatible or none would add mappings.
pub fn find_best_known_shuffle(partial_map: &[u8; 256]) -> Option<[u8; 256]> {
    let mut best: Option<(i64, [u8; 256])> = None;

    for known in KNOWN_SHUFFLES.iter() {
        let mut conflicts = 0usize;
        let mut extra = 0usize;

        for i in 0..256 {
            if partial_map[i] != 255 && known[i] != 255 {
                if partial_map[i] != known[i] {
                    conflicts += 1;
                }
            } else if known[i] != 255 && partial_map[i] == 255 {
                extra += 1;
            }
        }

        // Allow up to 5 conflicts (heuristic detector noise). Score by net
        // value: each extra mapping is +1, each conflict is -10 penalty.
        let score = extra as i64 - conflicts as i64 * 10;
        if conflicts <= 5 && score > 0 {
            if best.is_none() || score > best.as_ref().unwrap().0 {
                // Merge: start from the partial map, fill gaps from the known variant.
                // Skip conflicting bytes — keep the heuristic's value for those.
                let mut merged = *partial_map;
                let mut assigned = [false; 256];
                for &v in merged.iter() {
                    if v != 255 {
                        assigned[v as usize] = true;
                    }
                }
                for i in 0..256 {
                    if known[i] != 255 && merged[i] == 255 && !assigned[known[i] as usize] {
                        merged[i] = known[i];
                        assigned[known[i] as usize] = true;
                    }
                }
                best = Some((score, merged));
            }
        }
    }

    best.map(|(_, map)| map)
}

/// Returns `Some(shuffled_byte)` iff every known shuffle variant that contains
/// `std_op` maps it to the same shuffled byte. Returns `None` if:
///   - no variant contains `std_op`, or
///   - two or more variants disagree on which byte maps to `std_op`.
///
/// This exposes unanimous multi-variant agreement as a first-class evidence
/// signal. Unanimous agreement across all 7 hand-curated variants is stronger
/// than any single-script heuristic detector can produce, so callers can use
/// it to override more conservative revert guards when the augmenter picks
/// a unanimously-agreed byte for a structural-required opcode.
pub fn all_variants_that_map(std_op: u8) -> Option<u8> {
    let mut agreed: Option<u8> = None;
    for known in KNOWN_SHUFFLES.iter() {
        // Find the shuffled byte (if any) that this variant maps to std_op.
        let mut this_variant: Option<u8> = None;
        for i in 0..256 {
            if known[i] == std_op {
                this_variant = Some(i as u8);
                break;
            }
        }
        if let Some(b) = this_variant {
            match agreed {
                None => agreed = Some(b),
                Some(prev) if prev != b => return None,
                Some(_) => {}
            }
        }
    }
    agreed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_shuffles_no_duplicate_targets() {
        // Each variant must not map two different shuffled bytes to the same
        // standard opcode.
        for (idx, map) in KNOWN_SHUFFLES.iter().enumerate() {
            let mut seen = [false; 256];
            for i in 0..256 {
                if map[i] != 255 {
                    assert!(
                        !seen[map[i] as usize],
                        "variant {} maps two bytes to opcode {}",
                        idx,
                        map[i]
                    );
                    seen[map[i] as usize] = true;
                }
            }
        }
    }

    #[test]
    fn all_variants_share_nop_and_break() {
        // 0xFE -> 0 (NOP) and 0xFD -> 1 (BREAK) are common across all variants.
        for (idx, map) in KNOWN_SHUFFLES.iter().enumerate() {
            assert_eq!(map[0xFE], 0, "variant {} missing NOP", idx);
            assert_eq!(map[0xFD], 1, "variant {} missing BREAK", idx);
        }
    }

    #[test]
    fn find_best_empty_partial() {
        // An all-255 partial map should match some variant (the one with most mappings).
        let empty = [255u8; 256];
        let result = find_best_known_shuffle(&empty);
        assert!(result.is_some());
        // Variant 0 has 70 opcodes, should be the winner.
        let merged = result.unwrap();
        assert_eq!(merged[0xFE], 0);
        assert_eq!(merged[0xFD], 1);
    }

    #[test]
    fn all_variants_unanimous_on_fornprep() {
        // FORNPREP (std opcode 56): every variant maps it to 0xA8.
        // This is the unanimity that Phase A Patch 2 relies on to let the
        // augmenter commit 0xA8 → FORNPREP past the structural-required revert.
        assert_eq!(all_variants_that_map(56), Some(0xA8));
    }

    #[test]
    fn all_variants_unanimous_on_fornloop() {
        // FORNLOOP (std opcode 57): every variant maps it to 0x8B.
        assert_eq!(all_variants_that_map(57), Some(0x8B));
    }

    #[test]
    fn all_variants_unanimous_on_nop_and_break() {
        // NOP=0 → 0xFE, BREAK=1 → 0xFD across all variants.
        assert_eq!(all_variants_that_map(0), Some(0xFE));
        assert_eq!(all_variants_that_map(1), Some(0xFD));
    }

    #[test]
    fn all_variants_disagree_returns_none() {
        // LOADB (std opcode 2) is mapped to different bytes across variants:
        //   variant 0: 0x03, variant 1: 0x08, variant 2: 0xC6, variant 3: 0x08,
        //   variant 4: 0x08, variant 5: 0x04, variant 6: 0x01.
        // Multiple distinct bytes → not unanimous.
        assert_eq!(all_variants_that_map(2), None);
    }

    #[test]
    fn all_variants_missing_opcode_returns_none() {
        // Deprecated61 appears in no variant; should return None, not a stale byte.
        assert_eq!(all_variants_that_map(61), None);
    }

    #[test]
    fn find_best_incompatible() {
        // A partial map where every known variant has 4+ conflicts should return None.
        // We pick bytes that are mapped in ALL 7 variants and set them to wrong values.
        // 0xFE=0 and 0xFD=1 are shared by all. We also set several more bytes
        // that are mapped in most variants to wrong values to exceed the 3-conflict limit.
        let mut bad = [255u8; 256];
        // Set 10 common bytes to wrong values — guarantees >3 conflicts per variant
        for i in 0..10 {
            bad[i] = 200 + i as u8;
        }
        bad[0xFE] = 99;
        bad[0xFD] = 98;
        let result = find_best_known_shuffle(&bad);
        assert!(result.is_none());
    }
}
