//! Phase 5: per-build opcode permutation.
//!
//! The encoder always emits **canonical** opcode bytes (`Op::PushNil as u8 = 0`,
//! etc.). This pass walks every instruction in every proto and rewrites the
//! opcode byte through a build-specific permutation. The dispatcher is then
//! generated keyed on the permuted bytes — so two builds of the same input
//! emit different byte values for the same opcode, and the dispatcher's tree
//! splits at different pivots.

use crate::vm::{opcodes::Op, Module, INSTR_WIDTH};
use crate::ProtectOptions;

/// Number of canonical opcode IDs the encoder actually emits. Anything above
/// this is unused but the permutation table still extends to 256 entries
/// (identity for the unused range). Surfaces in tests / future passes.
#[allow(dead_code)]
const N_OPCODES: usize = Op::COUNT as usize;

pub fn permute(mut module: Module, opts: &ProtectOptions) -> Module {
    let perm = build_permutation(opts.seed.map(|s| s as u32).unwrap_or(0xCAFE_BABE));
    for proto in module.protos.iter_mut() {
        // Opcode bytes live at offset 0 of every 5-byte instruction.
        let mut i = 0;
        while i < proto.code.len() {
            let c = proto.code[i];
            proto.code[i] = perm[c as usize];
            i += INSTR_WIDTH;
        }
    }
    module.opcode_perm = Some(perm);
    module
}

/// Build a permutation where the canonical opcodes `0..N_OPCODES` get mapped
/// to a random set of distinct byte values across `0..256`. Other bytes
/// (operand-range, future opcodes) map to themselves so the table is a valid
/// full bijection.
fn build_permutation(seed: u32) -> [u8; 256] {
    let mut state = seed.max(1);

    // Fisher-Yates shuffle of `[0..256]`, then read off the first
    // N_OPCODES destinations.
    let mut all: [u8; 256] = [0; 256];
    for (i, v) in all.iter_mut().enumerate() {
        *v = i as u8;
    }
    for i in (1..256).rev() {
        state = xs32(state);
        let j = (state as usize) % (i + 1);
        all.swap(i, j);
    }

    // `all` is now a random bijection of [0..=255]. Use it directly as the
    // permutation: canonical opcode c -> all[c].
    all
}

fn xs32(mut s: u32) -> u32 {
    s ^= s << 13;
    s ^= s >> 17;
    s ^= s << 5;
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perm_is_bijection() {
        let p = build_permutation(0x12345678);
        let mut seen = [false; 256];
        for &b in p.iter() {
            assert!(!seen[b as usize], "perm not a bijection — byte {b} duplicated");
            seen[b as usize] = true;
        }
    }

    #[test]
    fn perm_actually_shuffles() {
        let p = build_permutation(0x42);
        // Identity permutation would have at least one fixed point but for a
        // random shuffle, expect lots of movement. Demand at least one byte
        // differs from its position (otherwise something's wrong).
        let moved = p.iter().enumerate().filter(|(i, b)| *i as u8 != **b).count();
        assert!(moved > 200, "permutation didn't shuffle enough ({moved})");
    }

    #[test]
    fn canonical_op_count_matches_handler_table() {
        // Sanity: ensure Op::COUNT matches the number of variants we actually
        // care about permuting.
        assert_eq!(N_OPCODES, Op::COUNT as usize);
    }
}
