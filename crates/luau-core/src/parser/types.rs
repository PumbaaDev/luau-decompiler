/// Represents a fully parsed Luau bytecode file
#[derive(Debug, Clone)]
pub struct Chunk {
    pub version: u8,
    pub types_version: u8,
    pub strings: Vec<String>,
    pub protos: Vec<Proto>,
    pub main_proto: u32,
}

/// A function prototype containing code and metadata
#[derive(Debug, Clone)]
pub struct Proto {
    pub max_stack_size: u8,
    pub num_params: u8,
    pub num_upvalues: u8,
    pub is_vararg: bool,
    pub flags: u8,
    pub typeinfo: Option<Vec<u8>>,
    pub code: Vec<u32>,
    pub constants: Vec<Constant>,
    /// Indices into the chunk's proto table for child functions
    pub child_protos: Vec<u32>,
    pub line_defined: u32,
    pub debug_name: Option<String>,
    pub line_info: Option<LineInfo>,
    pub debug_info: Option<DebugInfo>,
}

/// Constant values in the constant table
#[derive(Debug, Clone)]
pub enum Constant {
    Nil,
    Boolean(bool),
    Number(f64),
    String(String),
    /// Import reference (packed path: count << 30 | id0 << 20 | id1 << 10 | id2)
    Import(u32),
    /// Table template — a list of (key_const_idx, value_const_idx) pairs.
    /// `value_const_idx == None` means the field is initialized to nil / 0.0
    /// (the basic LBC_CONSTANT_TABLE form). A `Some(idx)` means the Luau
    /// compiler baked a compile-time constant value into the template
    /// (LBC_CONSTANT_TABLE_WITH_CONSTANTS, bytecode v7+).
    Table(Vec<(i32, Option<i32>)>),
    /// Closure (proto index)
    Closure(u32),
    /// Vector constant (x, y, z, w) - version 5+
    Vector(f32, f32, f32, f32),
}

/// Debug line information for instructions
#[derive(Debug, Clone)]
pub struct LineInfo {
    pub line_gap_log2: u8,
    /// Line number for each instruction
    pub lines: Vec<i32>,
}

/// Debug information about local variables and upvalues
#[derive(Debug, Clone)]
pub struct DebugInfo {
    pub locals: Vec<LocalVar>,
    pub upvalue_names: Vec<String>,
}

/// A local variable with its scope
#[derive(Debug, Clone)]
pub struct LocalVar {
    pub name: String,
    pub start_pc: u32,
    pub end_pc: u32,
    pub reg: u8,
}

// ── Instruction decoding helpers ──

/// Extract the opcode from an instruction word
pub fn insn_op(insn: u32) -> u8 {
    (insn & 0xFF) as u8
}

/// Extract field A (bits 8-15)
pub fn insn_a(insn: u32) -> u8 {
    ((insn >> 8) & 0xFF) as u8
}

/// Extract field B (bits 16-23)
pub fn insn_b(insn: u32) -> u8 {
    ((insn >> 16) & 0xFF) as u8
}

/// Extract field C (bits 24-31)
pub fn insn_c(insn: u32) -> u8 {
    ((insn >> 24) & 0xFF) as u8
}

/// Extract field D (bits 16-31, signed 16-bit)
pub fn insn_d(insn: u32) -> i16 {
    ((insn >> 16) & 0xFFFF) as i16
}

/// Extract field E (bits 8-31, signed 24-bit)
pub fn insn_e(insn: u32) -> i32 {
    (insn as i32) >> 8
}

/// Decode an import constant into its path components
/// Format: count << 30 | id0 << 20 | id1 << 10 | id2
pub fn decode_import(val: u32) -> Vec<u32> {
    let count = (val >> 30) as usize;
    let mut ids = Vec::with_capacity(count);
    if count >= 1 {
        ids.push((val >> 20) & 0x3FF);
    }
    if count >= 2 {
        ids.push((val >> 10) & 0x3FF);
    }
    if count >= 3 {
        ids.push(val & 0x3FF);
    }
    ids
}

impl Constant {
    /// Get a displayable representation
    pub fn display(&self, strings: &[String]) -> String {
        match self {
            Constant::Nil => "nil".to_string(),
            Constant::Boolean(b) => b.to_string(),
            Constant::Number(n) => {
                if *n == (*n as i64) as f64 && n.is_finite() {
                    format!("{}", *n as i64)
                } else {
                    format!("{}", n)
                }
            }
            Constant::String(s) => format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")),
            Constant::Import(val) => {
                let ids = decode_import(*val);
                let parts: Vec<&str> = ids
                    .iter()
                    .filter_map(|&id| strings.get(id as usize).map(|s| s.as_str()))
                    .collect();
                parts.join(".")
            }
            Constant::Table(entries) => format!("{{{} keys}}", entries.len()),
            Constant::Closure(idx) => format!("<closure {}>", idx),
            Constant::Vector(x, y, z, _w) => format!("Vector3.new({}, {}, {})", x, y, z),
        }
    }
}
