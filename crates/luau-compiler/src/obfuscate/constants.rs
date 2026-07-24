//! Phase 2: XOR-encrypt bytecode + string constants with a per-build seed.
//!
//! Keystream = xorshift32 driven from a single master seed. Encryption is
//! sequential across every encrypted item, in the same order the runtime
//! decryption bootstrap walks them: each proto's code, then each string
//! constant. Numeric constants stay as literals — numbers don't carry the
//! semantic signal that strings + bytecode do.

use crate::vm::{Const, Module, StringState};
use crate::ProtectOptions;

pub fn encrypt(mut module: Module, opts: &ProtectOptions) -> Module {
    let seed = opts.seed.map(|s| s as u32).unwrap_or_else(default_seed);
    let mut state = seed;
    if state == 0 {
        state = 1; // xorshift32 cannot start from zero
    }

    for (i, proto) in module.protos.iter_mut().enumerate() {
        for b in proto.code.iter_mut() {
            state = xs32(state);
            *b ^= (state & 0xFF) as u8;
        }
        module.code_states[i] = StringState::Encrypted;
    }

    if opts.lazy_strings {
        // Phase 7C: each string gets its own keystream so it can be decrypted
        // on demand at dispatch time without running every previous string's
        // decryption first.
        for (i, c) in module.constants.iter_mut().enumerate() {
            if let Const::String(bytes) = c {
                let mut s_state = xs32(seed ^ (i as u32));
                if s_state == 0 {
                    s_state = 1;
                }
                for b in bytes.iter_mut() {
                    s_state = xs32(s_state);
                    *b ^= (s_state & 0xFF) as u8;
                }
                module.const_states[i] = StringState::Encrypted;
            }
        }
    } else {
        for (i, c) in module.constants.iter_mut().enumerate() {
            if let Const::String(bytes) = c {
                for b in bytes.iter_mut() {
                    state = xs32(state);
                    *b ^= (state & 0xFF) as u8;
                }
                module.const_states[i] = StringState::Encrypted;
            }
        }
    }

    // Tamper protection: the runtime recomputes the hash of every encrypted
    // byte and XORs with the embedded "obfuscated seed" to recover the real
    // seed. Any byte modified after build = wrong recovered seed = garbage
    // decryption = execution fails. The check is implicit in execution
    // correctness, so an attacker can't simply strip a `if hash ~= ... then`
    // branch.
    let hash = ciphertext_hash(&module);
    let obfuscated_seed = seed ^ hash;
    module.encryption_seed = Some(obfuscated_seed);
    module
}

/// Stable hash of every encrypted byte in the module, in the same order the
/// runtime decryption walks them. Must match `_hash_module` in `runtime.rs`
/// exactly — they're a pair.
fn ciphertext_hash(module: &Module) -> u32 {
    let mut h: u32 = 0;
    for proto in &module.protos {
        for &b in &proto.code {
            h ^= b as u32;
            h = xs32(h.wrapping_add(1));
        }
    }
    for c in &module.constants {
        if let Const::String(bytes) = c {
            for &b in bytes {
                h ^= b as u32;
                h = xs32(h.wrapping_add(1));
            }
        }
    }
    h
}

fn xs32(mut s: u32) -> u32 {
    s ^= s << 13;
    s ^= s >> 17;
    s ^= s << 5;
    s
}

/// Derive a seed from the current time + a small process-local nonce. Good
/// enough for build-time entropy; consumers who want reproducible output
/// pass `opts.seed = Some(_)`.
fn default_seed() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static NONCE: AtomicU32 = AtomicU32::new(0);
    let nonce = NONCE.fetch_add(1, Ordering::Relaxed);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u32)
        .unwrap_or(0xCAFE_BABE);
    let s = now ^ nonce.wrapping_mul(0x9E37_79B9);
    if s == 0 {
        0xDEAD_BEEF
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encrypt then decrypt-by-replay reverses to the original bytes.
    /// Using `encrypt` is symmetric (it's pure XOR), so applying it twice
    /// reproduces the input.
    #[test]
    fn xor_is_symmetric() {
        let mut data = vec![0x01u8, 0x02, 0x03, 0xff, 0xab, 0xcd];
        let mut state = 0xDEADBEEFu32;
        for b in data.iter_mut() {
            state = xs32(state);
            *b ^= (state & 0xFF) as u8;
        }
        let mut state = 0xDEADBEEFu32;
        for b in data.iter_mut() {
            state = xs32(state);
            *b ^= (state & 0xFF) as u8;
        }
        assert_eq!(data, vec![0x01, 0x02, 0x03, 0xff, 0xab, 0xcd]);
    }
}
