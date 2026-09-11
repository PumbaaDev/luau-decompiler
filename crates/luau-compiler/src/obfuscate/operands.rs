//! Phase 7B: XOR-scramble instruction operand bytes with a per-proto
//! key mixed with the instruction's byte offset.
//!
//! After the encoder produces canonical bytecode, each instruction's four
//! operand bytes (positions +1..+5 of the instruction) get XORed with a
//! per-instruction key derived from `(proto_key, byte_offset)`. The Phase 2
//! global keystream then XORs every byte uniformly on top, so the operand
//! scrambling sits *inside* the encryption layer — even after Phase 2
//! decryption, operand values remain garbled until the dispatcher reverses
//! the scramble at decode time.
//!
//! Opcode bytes (offset +0) are NOT touched here — Phase 5 already permutes
//! them and we don't want to interfere with that mapping.

use crate::vm::{Module, INSTR_WIDTH};
use crate::ProtectOptions;

pub fn scramble(mut module: Module, opts: &ProtectOptions) -> Module {
    let mut state = opts
        .seed
        .map(|s| s as u32)
        .unwrap_or(0x5EED_F00D)
        ^ 0x33CC_99AA;
    if state == 0 {
        state = 1;
    }

    for proto in module.protos.iter_mut() {
        // Derive a per-proto key. Ensure non-zero so the runtime "skip when
        // zero" optimization doesn't accidentally short-circuit.
        state = xs32(state);
        let proto_key = if state == 0 { 1 } else { state };
        proto.operand_key = proto_key;

        let mut off = 0;
        while off < proto.code.len() {
            let instr_key = derive_instr_key(proto_key, off);
            let key_lo = (instr_key & 0xFF) as u8;
            let key_hi = ((instr_key >> 8) & 0xFF) as u8;
            // Operand A bytes at off+1, off+2; operand B at off+3, off+4.
            proto.code[off + 1] ^= key_lo;
            proto.code[off + 2] ^= key_hi;
            proto.code[off + 3] ^= key_lo;
            proto.code[off + 4] ^= key_hi;
            off += INSTR_WIDTH;
        }
    }
    module
}

/// Per-instruction operand key. Must mirror the Luau-side
/// `_instr_key(base, byte_offset)` in the runtime so encode/decode match.
fn derive_instr_key(proto_key: u32, byte_offset: usize) -> u32 {
    let mixed = xs32(byte_offset as u32).wrapping_add(0x9E37_79B9);
    mixed ^ proto_key
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
    fn round_trip_xor_recovers_original() {
        let proto_key = 0xA1B2_C3D4;
        let pos = 25;
        let key = derive_instr_key(proto_key, pos);
        let key_lo = (key & 0xFF) as u8;
        let key_hi = ((key >> 8) & 0xFF) as u8;
        let original = [0x12u8, 0x34, 0xAB, 0xCD];
        let scrambled = [
            original[0] ^ key_lo,
            original[1] ^ key_hi,
            original[2] ^ key_lo,
            original[3] ^ key_hi,
        ];
        let recovered = [
            scrambled[0] ^ key_lo,
            scrambled[1] ^ key_hi,
            scrambled[2] ^ key_lo,
            scrambled[3] ^ key_hi,
        ];
        assert_eq!(original, recovered);
    }
}
