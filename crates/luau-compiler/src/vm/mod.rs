//! Stack-VM opcode definitions, encoded module type, and the IR encoder.
//!
//! The VM is intentionally small (~30 ops) and stack-based. Each instruction
//! is a fixed 5 bytes wide: `op:u8, a:i16_le, b:i16_le`. Operand semantics
//! are op-specific.

pub mod opcodes;
pub mod encoder;

use opcodes::Op;

/// An encoded program ready for emission.
#[derive(Debug, Clone, Default)]
pub struct Module {
    pub protos: Vec<EncodedProto>,
    pub constants: Vec<Const>,
    /// Whether each proto's `code` is plaintext or has been XOR-encrypted by
    /// the obfuscate pass. Same length as `protos`.
    pub code_states: Vec<StringState>,
    /// Whether each string constant is plaintext or encrypted. Aligned with
    /// `constants` (Number entries get a `Plain` placeholder).
    pub const_states: Vec<StringState>,
    /// Master encryption seed, if the encrypt pass ran. XORed with the
    /// runtime ciphertext hash to derive the actual keystream seed
    /// (Phase 4 tamper protection).
    pub encryption_seed: Option<u32>,
    /// Canonical-opcode-byte to emitted-opcode-byte map. Identity in slots
    /// the encoder doesn't use (since perm only meaningfully covers the
    /// emitted op set). When `Some`, the bytecode has already been remapped
    /// and the dispatcher must dispatch on the emitted bytes.
    pub opcode_perm: Option<[u8; 256]>,
}

/// One encoded function.
#[derive(Debug, Clone, Default)]
pub struct EncodedProto {
    /// Number of declared parameters.
    pub num_params: u16,
    /// Number of named locals (params + declared `local`s).
    pub num_locals: u16,
    /// Number of upvalues this proto captures from its parent. Upvalue
    /// resolution at runtime walks the closure environment chain.
    pub num_upvalues: u16,
    /// Whether the proto accepts varargs.
    pub is_vararg: bool,
    /// Instruction stream (5 bytes per instruction).
    pub code: Vec<u8>,
    /// Upvalue specs: (kind, index). kind 0 = local of parent, kind 1 =
    /// upvalue of parent. Read by the parent when it emits CLOSURE.
    pub upvalue_specs: Vec<(u8, u16)>,
    /// Phase 7B per-proto operand scramble key. Zero means "operands not
    /// scrambled" — the dispatcher only XORs operand bytes when this is
    /// non-zero (saves a few cycles on plain builds).
    pub operand_key: u32,
}

/// A constant in the program-wide pool. Strings are byte sequences; this lets
/// the encryption pass replace plaintext with arbitrary ciphertext without
/// running into UTF-8 invariants.
#[derive(Debug, Clone, PartialEq)]
pub enum Const {
    Number(f64),
    String(Vec<u8>),
}

impl Const {
    pub fn as_key(&self) -> ConstKey {
        match self {
            Const::Number(n) => ConstKey::Number(n.to_bits()),
            Const::String(b) => ConstKey::String(b.clone()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ConstKey {
    Number(u64),
    String(Vec<u8>),
}

/// Whether a string-typed constant is plaintext or already encrypted. Built
/// up alongside `Module::constants` so the emit module knows what to render.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StringState {
    #[default]
    Plain,
    Encrypted,
}

/// Emit a single 5-byte instruction into `out`.
pub fn emit_instr(out: &mut Vec<u8>, op: Op, a: i16, b: i16) {
    out.push(op as u8);
    out.extend_from_slice(&a.to_le_bytes());
    out.extend_from_slice(&b.to_le_bytes());
}

/// Patch the operand A of the instruction at byte-offset `at` to `value`.
pub fn patch_a(buf: &mut [u8], at: usize, value: i16) {
    let bytes = value.to_le_bytes();
    buf[at + 1] = bytes[0];
    buf[at + 2] = bytes[1];
}

/// Width of a single instruction in bytes.
pub const INSTR_WIDTH: usize = 5;
