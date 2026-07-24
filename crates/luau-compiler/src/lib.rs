//! Luau protector: source -> custom stack-VM bytecode -> Luau wrapper script.
//!
//! Pipeline:
//!   1. Parse Luau source with `full_moon`.
//!   2. Lower the AST into a small typed IR (`ir`).
//!   3. Encode the IR into a custom stack-VM byte stream (`vm`).
//!   4. (Phases 2+) Apply obfuscation passes on the byte stream + constants.
//!   5. Emit a self-contained Luau script that decrypts and interprets it (`emit`).
//!
//! The resulting Luau script runs in any Roblox executor that allows
//! `loadstring`/raw script execution; from a decompiler's viewpoint it looks
//! like an interpreter loop dispatching over an opaque byte string.

pub mod ir;
pub mod vm;
pub mod obfuscate;
pub mod emit;

use thiserror::Error;

/// Compiler version, injected from Cargo.toml at build time.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Options that control how aggressive each phase is.
///
/// Phases 2-5 land progressively; flags exist now so the CLI surface is
/// stable.
#[derive(Debug, Clone)]
pub struct ProtectOptions {
    /// Encrypt the bytecode + constant blob (Phase 2). Default: true.
    pub encrypt_constants: bool,
    /// Flatten control flow into a state machine (Phase 3). Default: false.
    pub flatten_control_flow: bool,
    /// Inject junk / decoy instructions (Phase 4). Default: false.
    pub inject_junk: bool,
    /// Permute opcode IDs per build (Phase 5). Default: true once Phase 5 lands.
    pub permute_opcodes: bool,
    /// Render numeric constants as `bit32.bxor(A, B)` expressions instead of
    /// literals (Phase 7A). Closes the literal-number leak path.
    pub obfuscate_numbers: bool,
    /// XOR-scramble instruction operand bytes with a per-proto position-mixed
    /// key (Phase 7B). After Phase 2 decryption, operands remain garbled
    /// until the dispatcher re-applies the key at decode time.
    pub encrypt_operands: bool,
    /// Decrypt string constants lazily on first use instead of at startup
    /// (Phase 7C). Frustrates memory-dump attacks that scan for plaintext.
    pub lazy_strings: bool,
    /// Optional fixed RNG seed for reproducible builds. None = OS entropy.
    pub seed: Option<u64>,
}

impl Default for ProtectOptions {
    fn default() -> Self {
        Self {
            encrypt_constants: true,
            flatten_control_flow: false,
            inject_junk: false,
            permute_opcodes: false,
            obfuscate_numbers: false,
            encrypt_operands: false,
            lazy_strings: false,
            seed: None,
        }
    }
}

#[derive(Debug, Error)]
pub enum ProtectError {
    #[error("parse error: {0}")]
    Parse(String),
    #[error("lowering error: {0}")]
    Lower(String),
    #[error("encoding error: {0}")]
    Encode(String),
}

/// Compile a Luau source string into a protected, self-contained Luau script.
///
/// The returned string is valid Luau and can be executed directly by any
/// runtime that supports the standard library subset used by the runtime
/// shim emitted by [`emit`].
pub fn protect(source: &str, opts: &ProtectOptions) -> Result<String, ProtectError> {
    let ast = full_moon::parse(source).map_err(|errs| {
        let msgs: Vec<String> = errs.iter().map(|e| format!("{e}")).collect();
        ProtectError::Parse(msgs.join("; "))
    })?;

    let program = ir::lower::lower_ast(&ast).map_err(ProtectError::Lower)?;
    let module = vm::encoder::encode(&program).map_err(ProtectError::Encode)?;
    let module = obfuscate::apply(module, opts);
    Ok(emit::render(&module, opts))
}
