//! Phase 4: decoy constants.
//!
//! Inject a handful of random byte strings into the constant pool. They're
//! never referenced by any emitted instruction, so they're inert at runtime —
//! but because they contribute to the encrypted-byte hash (Phase 2's tamper
//! protection), an attacker can't strip them without breaking decryption.

use crate::vm::{Const, Module, StringState};
use crate::ProtectOptions;

pub fn inject(mut module: Module, opts: &ProtectOptions) -> Module {
    let mut state = opts
        .seed
        .map(|s| s as u32)
        .unwrap_or(0xDEAD_BEEF)
        ^ 0xA5A5_A5A5;
    if state == 0 {
        state = 1;
    }
    state = xs32(state);

    let n_decoys = 3 + ((state >> 8) & 7) as usize; // 3..=10
    for _ in 0..n_decoys {
        state = xs32(state);
        let len = 4 + (state & 31) as usize; // 4..=35 bytes
        let mut bytes = Vec::with_capacity(len);
        for _ in 0..len {
            state = xs32(state);
            bytes.push((state & 0xFF) as u8);
        }
        module.constants.push(Const::String(bytes));
        module.const_states.push(StringState::Plain);
    }
    module
}

fn xs32(mut s: u32) -> u32 {
    s ^= s << 13;
    s ^= s >> 17;
    s ^= s << 5;
    s
}
