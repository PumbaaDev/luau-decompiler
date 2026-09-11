//! Obfuscation passes applied to an encoded [`crate::vm::Module`].
//!
//! Phase 1 (current): no-op pass-through.
//! Phase 2: encrypt constants + bytecode blob.
//! Phase 3: control flow flattening.
//! Phase 4: junk injection + anti-tamper.
//! Phase 5: per-build opcode permutation.

use crate::vm::Module;
use crate::ProtectOptions;

pub mod constants;
pub mod flatten;
pub mod junk;
pub mod operands;
pub mod rename;

/// Apply every enabled obfuscation pass to `module`, in the order that's
/// safe — e.g. junk injection runs before flattening so the flattener treats
/// junk basic blocks as first-class states.
pub fn apply(module: Module, opts: &ProtectOptions) -> Module {
    let mut m = module;
    // Flatten FIRST: it rewrites each proto's bytecode while opcode bytes
    // are still canonical and operands are still plain. Adds state-ID
    // constants to the pool that downstream passes happily consume.
    if opts.flatten_control_flow {
        m = flatten::flatten(m, opts);
    }
    if opts.inject_junk {
        m = junk::inject(m, opts);
    }
    // Permutation must run BEFORE encryption: the encoder produces canonical
    // opcode bytes, permute rewrites them in-place, and encryption then XORs
    // every byte uniformly so the permuted bytes are still hidden by the
    // keystream.
    if opts.permute_opcodes {
        m = rename::permute(m, opts);
    }
    // Operand scrambling must also run before bulk encryption — the goal is
    // to keep operands garbled even after Phase 2 decryption.
    if opts.encrypt_operands {
        m = operands::scramble(m, opts);
    }
    if opts.encrypt_constants {
        m = constants::encrypt(m, opts);
    }
    m
}
