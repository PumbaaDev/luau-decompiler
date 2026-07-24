/// Luau bytecode opcodes - supports versions 3 through 6+
///
/// Each opcode has an encoding format:
///   ABC = opcode(8) A(8) B(8) C(8)
///   AD  = opcode(8) A(8) D(16, signed)
///   E   = opcode(8) E(24, signed)
///
/// Some instructions use an AUX word (the next u32 in the instruction stream).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum LuauOpcode {
    // ── No-ops & debugging ──
    Nop = 0,
    Break = 1,

    // ── Load instructions ──
    /// A: target register; sets to nil
    LoadNil = 2,
    /// A: target register; B: value (0/1); C: jump offset (optional, 0=none)
    LoadB = 3,
    /// A: target register; D: signed integer value
    LoadN = 4,
    /// A: target register; D: constant table index
    LoadK = 5,
    /// A: target register; moves value from B to A
    Move = 6,

    // ── Global access ──
    /// A: target register; AUX: string constant index for global name
    GetGlobal = 7,
    /// A: source register; AUX: string constant index for global name
    SetGlobal = 8,

    // ── Upvalue access ──
    /// A: target register; B: upvalue index
    GetUpval = 9,
    /// A: source register; B: upvalue index
    SetUpval = 10,
    /// A: close upvalues starting from register A
    CloseUpvals = 11,

    // ── Import ──
    /// A: target register; D: constant index (import path); AUX: import ids
    GetImport = 12,

    // ── Table access ──
    /// A: target register; B: table register; C: key register
    GetTable = 13,
    /// A: source register; B: table register; C: key register
    SetTable = 14,
    /// A: target register; B: table register; AUX: key constant index (string)
    GetTableKS = 15,
    /// A: source register; B: table register; AUX: key constant index (string)
    SetTableKS = 16,
    /// A: target register; B: table register; C: index (1-256)
    GetTableN = 17,
    /// A: source register; B: table register; C: index (1-256)
    SetTableN = 18,

    // ── Closures ──
    /// A: target register; D: child proto index
    NewClosure = 19,
    /// A: method name register; B: method name constant; C: skip offset
    NameCall = 20,

    // ── Function calls ──
    /// A: function register; B: arg count + 1 (0=vararg); C: result count + 1 (0=multret)
    Call = 21,
    /// A: first result register; B: result count + 1 (0=multret)
    Return = 22,

    // ── Jumps ──
    /// D: jump offset (signed)
    Jump = 23,
    /// D: jump offset (signed), backwards jump
    JumpBack = 24,
    /// A: condition register; D: jump offset if truthy
    JumpIf = 25,
    /// A: condition register; D: jump offset if falsy
    JumpIfNot = 26,
    /// A: left register; D: jump offset; AUX: right register
    JumpIfEq = 27,
    /// A: left register; D: jump offset; AUX: right register
    JumpIfLE = 28,
    /// A: left register; D: jump offset; AUX: right register
    JumpIfLT = 29,
    /// A: left register; D: jump offset; AUX: right register
    JumpIfNotEq = 30,
    /// A: left register; D: jump offset; AUX: right register
    JumpIfNotLE = 31,
    /// A: left register; D: jump offset; AUX: right register
    JumpIfNotLT = 32,

    // ── Arithmetic (register, register) ──
    /// A: target; B: left operand; C: right operand
    Add = 33,
    Sub = 34,
    Mul = 35,
    Div = 36,
    Mod = 37,
    Pow = 38,

    // ── Arithmetic (register, constant) ──
    /// A: target; B: left operand; C: constant index
    AddK = 39,
    SubK = 40,
    MulK = 41,
    DivK = 42,
    ModK = 43,
    PowK = 44,

    // ── Logical ──
    /// A: target; B: left register; C: right register (result = left if truthy, else right)
    And = 45,
    Or = 46,
    /// A: target; B: left register; C: constant index
    AndK = 47,
    OrK = 48,

    // ── String operations ──
    /// A: target; B: first source; C: last source (concatenates B..C)
    Concat = 49,

    // ── Unary ──
    /// A: target; B: source
    Not = 50,
    Minus = 51,
    Length = 52,

    // ── Tables ──
    /// A: target register; B: array size hint; AUX: hash size hint
    NewTable = 53,
    /// A: target register; D: constant table index (template)
    DupTable = 54,
    /// A: table register; B: first value register; C: count (0=up to top); AUX: table index offset
    SetList = 55,

    // ── Numeric for loop ──
    /// A: base register; D: jump offset (skips past the matching FORNLOOP).
    /// Luau v6 layout (Phase B0.3 verified): `R(A)=limit, R(A+1)=step,
    /// R(A+2)=initial index + loop variable during the body`. Note this is
    /// NOT the Lua 5.1 layout (which put the loop var at `R(A+3)`).
    ForNPrep = 56,
    /// A: base register; D: jump offset (backward to body start).
    /// Layout matches ForNPrep: R(A)=limit, R(A+1)=step, R(A+2)=i.
    ForNLoop = 57,

    // ── Generic for loop ──
    /// A: iterator state register; D: jump offset
    ForGPrep = 58,
    /// A: iterator state register; D: jump offset; AUX: loop variable count
    ForGLoop = 59,
    /// A: iterator register; D: jump offset (for inext specialization)
    ForGPrepINext = 60,
    /// Deprecated (was ForGLoopINext). AD format, no AUX. Still participates in Roblox opcode shuffle.
    Deprecated61 = 61,
    /// A: iterator register; D: jump offset (for next specialization)
    ForGPrepNext = 62,

    // ── Native code hint ──
    NativeCall = 63,

    // ── Varargs ──
    /// A: target register; B: count + 1 (0=all)
    GetVarargs = 64,
    /// A: number of fixed params
    PrepVarargs = 65,

    // ── Extended constant load ──
    /// A: target register; AUX: constant index
    LoadKX = 66,

    // ── Extended jump ──
    /// E: jump offset (24-bit signed)
    JumpX = 67,

    // ── Fastcall (built-in function optimization) ──
    /// A: builtin id; C: jump offset to skip CALL
    FastCall = 68,
    /// A: builtin id; B: source register
    Coverage = 69,
    /// A: capture type; B: register/upvalue index
    Capture = 70,
    /// A: target; B: left; C: right (reversed sub: K - reg)
    SubRK = 71,
    /// A: target; B: left; C: right (reversed div: K / reg)
    DivRK = 72,

    /// A: builtin id; B: arg register; C: jump offset
    FastCall1 = 73,
    /// A: builtin id; B: arg1 register; C: jump offset; AUX: arg2 register
    FastCall2 = 74,
    /// A: builtin id; B: arg register; C: jump offset; AUX: constant index
    FastCall2K = 75,

    // ── v4+: Integer division ──
    /// A: target; B: left; C: right
    IDiv = 76,
    /// A: target; B: left; C: constant index
    IDivK = 77,

    // ── Extended comparison jumps (v3+) ──
    /// A: register; D: jump offset; AUX: nil comparison
    JumpXEqKNil = 78,
    /// A: register; D: jump offset; AUX: boolean constant
    JumpXEqKB = 79,
    /// A: register; D: jump offset; AUX: number constant index
    JumpXEqKN = 80,
    /// A: register; D: jump offset; AUX: string constant index
    JumpXEqKS = 81,

    // ── v5+: Closure duplication ──
    /// A: target; D: constant index (closure)
    DupClosure = 82,

    // ── v6+: Three-arg fastcall ──
    /// A: builtin id; B: arg1 register; C: jump offset; AUX: arg2 + arg3 packed
    FastCall3 = 83,

    // ── Roblox-specific extensions: native bitwise operators (Roblox Luau v4+) ──
    // These opcodes are Roblox additions beyond open-source Luau canonical 83.
    // All use ABC format (A=dest, B=left, C=right register OR constant index).
    // No AUX word — single-word instructions.
    /// A: target; B: left register; C: right register  →  R(A) = R(B) & R(C)
    Band = 84,
    /// A: target; B: left register; C: right register  →  R(A) = R(B) | R(C)
    Bor = 85,
    /// A: target; B: left register; C: right register  →  R(A) = R(B) ~ R(C) (XOR)
    Bxor = 86,
    /// A: target; B: source register; C: unused (0)   →  R(A) = ~R(B)
    Bnot = 87,
    /// A: target; B: left register; C: right register  →  R(A) = R(B) << R(C)
    Shl = 88,
    /// A: target; B: left register; C: right register  →  R(A) = R(B) >> R(C)
    Shr = 89,
    /// A: target; B: left register; C: constant index  →  R(A) = R(B) & K(C)
    Bandk = 90,
    /// A: target; B: left register; C: constant index  →  R(A) = R(B) | K(C)
    Bork = 91,

    // ── Roblox-specific extensions beyond canonical 91 ──
    // Exact semantics unknown; unary shape (A=dst, B=src, C=0) confirmed from corpus.
    // Emitted as placeholder calls in the decompiler output.
    /// A: target; B: source; C: 0  — Roblox extension, unary, exact op unknown
    RbxExt92 = 92,
    /// A: target; B: source; C: 0  — Roblox extension, unary, exact op unknown
    RbxExt93 = 93,
    /// A: target; B: source; C: 0  — Roblox extension, unary, exact op unknown
    RbxExt94 = 94,
    /// A: target; B: left; C: right  — Roblox extension, binary, exact op unknown
    RbxExt95 = 95,
    /// A: target; B: source; C: 0  — Roblox extension, unary, exact op unknown
    RbxExt96 = 96,
    /// A: target; B: source; C: 0  — Roblox extension, unary, exact op unknown
    RbxExt97 = 97,
    /// A: target; B: source; C: 0  — Roblox extension, unary, exact op unknown
    RbxExt98 = 98,
    /// A: target; B: left; C: right  — Roblox extension, binary, exact op unknown
    RbxExt99 = 99,
    /// A: target; B: left; C: right  — Roblox extension, binary, exact op unknown
    RbxExt100 = 100,
    /// A: target; B: left; C: right  — Roblox extension, binary, exact op unknown
    RbxExt101 = 101,
    /// A: target; B: left; C: right  — Roblox extension, binary, exact op unknown
    RbxExt102 = 102,
    /// A: target; B: left; C: right  — Roblox extension, binary, exact op unknown
    RbxExt103 = 103,
    /// A: target; B: left; C: right  — Roblox extension, binary, exact op unknown
    RbxExt104 = 104,
    /// A: target; B: left; C: right  — Roblox extension, binary, exact op unknown
    RbxExt105 = 105,

    // Unknown opcode
    Unknown = 255,
}

impl LuauOpcode {
    /// One past the highest valid canonical opcode number (exclusive upper bound).
    /// Includes Roblox-specific extensions 92-99.
    pub const MAX_OPCODE: usize = 106;

    pub fn from_u8(val: u8) -> Self {
        match val {
            0 => Self::Nop,
            1 => Self::Break,
            2 => Self::LoadNil,
            3 => Self::LoadB,
            4 => Self::LoadN,
            5 => Self::LoadK,
            6 => Self::Move,
            7 => Self::GetGlobal,
            8 => Self::SetGlobal,
            9 => Self::GetUpval,
            10 => Self::SetUpval,
            11 => Self::CloseUpvals,
            12 => Self::GetImport,
            13 => Self::GetTable,
            14 => Self::SetTable,
            15 => Self::GetTableKS,
            16 => Self::SetTableKS,
            17 => Self::GetTableN,
            18 => Self::SetTableN,
            19 => Self::NewClosure,
            20 => Self::NameCall,
            21 => Self::Call,
            22 => Self::Return,
            23 => Self::Jump,
            24 => Self::JumpBack,
            25 => Self::JumpIf,
            26 => Self::JumpIfNot,
            27 => Self::JumpIfEq,
            28 => Self::JumpIfLE,
            29 => Self::JumpIfLT,
            30 => Self::JumpIfNotEq,
            31 => Self::JumpIfNotLE,
            32 => Self::JumpIfNotLT,
            33 => Self::Add,
            34 => Self::Sub,
            35 => Self::Mul,
            36 => Self::Div,
            37 => Self::Mod,
            38 => Self::Pow,
            39 => Self::AddK,
            40 => Self::SubK,
            41 => Self::MulK,
            42 => Self::DivK,
            43 => Self::ModK,
            44 => Self::PowK,
            45 => Self::And,
            46 => Self::Or,
            47 => Self::AndK,
            48 => Self::OrK,
            49 => Self::Concat,
            50 => Self::Not,
            51 => Self::Minus,
            52 => Self::Length,
            53 => Self::NewTable,
            54 => Self::DupTable,
            55 => Self::SetList,
            56 => Self::ForNPrep,
            57 => Self::ForNLoop,
            58 => Self::ForGPrep,
            59 => Self::ForGLoop,
            60 => Self::ForGPrepINext,
            61 => Self::Deprecated61,
            62 => Self::ForGPrepNext,
            63 => Self::NativeCall,
            64 => Self::GetVarargs,
            65 => Self::PrepVarargs,
            66 => Self::LoadKX,
            67 => Self::JumpX,
            68 => Self::FastCall,
            69 => Self::Coverage,
            70 => Self::Capture,
            71 => Self::SubRK,
            72 => Self::DivRK,
            73 => Self::FastCall1,
            74 => Self::FastCall2,
            75 => Self::FastCall2K,
            76 => Self::IDiv,
            77 => Self::IDivK,
            78 => Self::JumpXEqKNil,
            79 => Self::JumpXEqKB,
            80 => Self::JumpXEqKN,
            81 => Self::JumpXEqKS,
            82 => Self::DupClosure,
            83 => Self::FastCall3,
            84 => Self::Band,
            85 => Self::Bor,
            86 => Self::Bxor,
            87 => Self::Bnot,
            88 => Self::Shl,
            89 => Self::Shr,
            90 => Self::Bandk,
            91 => Self::Bork,
            92 => Self::RbxExt92,
            93 => Self::RbxExt93,
            94 => Self::RbxExt94,
            95 => Self::RbxExt95,
            96 => Self::RbxExt96,
            97 => Self::RbxExt97,
            98 => Self::RbxExt98,
            99 => Self::RbxExt99,
            100 => Self::RbxExt100,
            101 => Self::RbxExt101,
            102 => Self::RbxExt102,
            103 => Self::RbxExt103,
            104 => Self::RbxExt104,
            105 => Self::RbxExt105,
            _ => Self::Unknown,
        }
    }

    /// Name of the opcode
    pub fn name(&self) -> &'static str {
        match self {
            Self::Nop => "NOP",
            Self::Break => "BREAK",
            Self::LoadNil => "LOADNIL",
            Self::LoadB => "LOADB",
            Self::LoadN => "LOADN",
            Self::LoadK => "LOADK",
            Self::Move => "MOVE",
            Self::GetGlobal => "GETGLOBAL",
            Self::SetGlobal => "SETGLOBAL",
            Self::GetUpval => "GETUPVAL",
            Self::SetUpval => "SETUPVAL",
            Self::CloseUpvals => "CLOSEUPVALS",
            Self::GetImport => "GETIMPORT",
            Self::GetTable => "GETTABLE",
            Self::SetTable => "SETTABLE",
            Self::GetTableKS => "GETTABLEKS",
            Self::SetTableKS => "SETTABLEKS",
            Self::GetTableN => "GETTABLEN",
            Self::SetTableN => "SETTABLEN",
            Self::NewClosure => "NEWCLOSURE",
            Self::NameCall => "NAMECALL",
            Self::Call => "CALL",
            Self::Return => "RETURN",
            Self::Jump => "JUMP",
            Self::JumpBack => "JUMPBACK",
            Self::JumpIf => "JUMPIF",
            Self::JumpIfNot => "JUMPIFNOT",
            Self::JumpIfEq => "JUMPIFEQ",
            Self::JumpIfLE => "JUMPIFLE",
            Self::JumpIfLT => "JUMPIFLT",
            Self::JumpIfNotEq => "JUMPIFNOTEQ",
            Self::JumpIfNotLE => "JUMPIFNOTLE",
            Self::JumpIfNotLT => "JUMPIFNOTLT",
            Self::Add => "ADD",
            Self::Sub => "SUB",
            Self::Mul => "MUL",
            Self::Div => "DIV",
            Self::Mod => "MOD",
            Self::Pow => "POW",
            Self::AddK => "ADDK",
            Self::SubK => "SUBK",
            Self::MulK => "MULK",
            Self::DivK => "DIVK",
            Self::ModK => "MODK",
            Self::PowK => "POWK",
            Self::And => "AND",
            Self::Or => "OR",
            Self::AndK => "ANDK",
            Self::OrK => "ORK",
            Self::Concat => "CONCAT",
            Self::Not => "NOT",
            Self::Minus => "MINUS",
            Self::Length => "LENGTH",
            Self::NewTable => "NEWTABLE",
            Self::DupTable => "DUPTABLE",
            Self::SetList => "SETLIST",
            Self::ForNPrep => "FORNPREP",
            Self::ForNLoop => "FORNLOOP",
            Self::ForGPrep => "FORGPREP",
            Self::ForGLoop => "FORGLOOP",
            Self::ForGPrepINext => "FORGPREP_INEXT",
            Self::Deprecated61 => "DEPRECATED_61",
            Self::ForGPrepNext => "FORGPREP_NEXT",
            Self::NativeCall => "NATIVECALL",
            Self::GetVarargs => "GETVARARGS",
            Self::PrepVarargs => "PREPVARARGS",
            Self::LoadKX => "LOADKX",
            Self::JumpX => "JUMPX",
            Self::FastCall => "FASTCALL",
            Self::Coverage => "COVERAGE",
            Self::Capture => "CAPTURE",
            Self::SubRK => "SUBRK",
            Self::DivRK => "DIVRK",
            Self::FastCall1 => "FASTCALL1",
            Self::FastCall2 => "FASTCALL2",
            Self::FastCall2K => "FASTCALL2K",
            Self::IDiv => "IDIV",
            Self::IDivK => "IDIVK",
            Self::JumpXEqKNil => "JUMPXEQKNIL",
            Self::JumpXEqKB => "JUMPXEQKB",
            Self::JumpXEqKN => "JUMPXEQKN",
            Self::JumpXEqKS => "JUMPXEQKS",
            Self::DupClosure => "DUPCLOSURE",
            Self::FastCall3 => "FASTCALL3",
            Self::Band => "BAND",
            Self::Bor => "BOR",
            Self::Bxor => "BXOR",
            Self::Bnot => "BNOT",
            Self::Shl => "SHL",
            Self::Shr => "SHR",
            Self::Bandk => "BANDK",
            Self::Bork => "BORK",
            Self::RbxExt92 => "RBX_EXT_92",
            Self::RbxExt93 => "RBX_EXT_93",
            Self::RbxExt94 => "RBX_EXT_94",
            Self::RbxExt95 => "RBX_EXT_95",
            Self::RbxExt96 => "RBX_EXT_96",
            Self::RbxExt97 => "RBX_EXT_97",
            Self::RbxExt98 => "RBX_EXT_98",
            Self::RbxExt99 => "RBX_EXT_99",
            Self::RbxExt100 => "RBX_EXT_100",
            Self::RbxExt101 => "RBX_EXT_101",
            Self::RbxExt102 => "RBX_EXT_102",
            Self::RbxExt103 => "RBX_EXT_103",
            Self::RbxExt104 => "RBX_EXT_104",
            Self::RbxExt105 => "RBX_EXT_105",
            Self::Unknown => "UNKNOWN",
        }
    }

    /// Does this instruction have an AUX word?
    pub fn has_aux(&self) -> bool {
        matches!(
            self,
            Self::GetGlobal
                | Self::SetGlobal
                | Self::GetImport
                | Self::GetTableKS
                | Self::SetTableKS
                | Self::NewTable
                | Self::SetList
                | Self::ForGLoop
                | Self::LoadKX
                | Self::FastCall2
                | Self::FastCall2K
                | Self::FastCall3
                | Self::JumpIfEq
                | Self::JumpIfLE
                | Self::JumpIfLT
                | Self::JumpIfNotEq
                | Self::JumpIfNotLE
                | Self::JumpIfNotLT
                | Self::JumpXEqKNil
                | Self::JumpXEqKB
                | Self::JumpXEqKN
                | Self::JumpXEqKS
                | Self::NameCall
                // Roblox extension: RBX_EXT_101 is observed to have an AUX word
                // (confirmed via debug-disasm: UNKNOWN 0x00000800 always follows it).
                | Self::RbxExt101
        )
    }
}

/// Names of built-in functions used by FASTCALL
pub fn builtin_name(id: u8) -> &'static str {
    match id {
        0 => "none",
        1 => "assert",
        2 => "math.abs",
        3 => "math.acos",
        4 => "math.asin",
        5 => "math.atan2",
        6 => "math.atan",
        7 => "math.ceil",
        8 => "math.cosh",
        9 => "math.cos",
        10 => "math.deg",
        11 => "math.exp",
        12 => "math.floor",
        13 => "math.fmod",
        14 => "math.frexp",
        15 => "math.ldexp",
        16 => "math.log10",
        17 => "math.log",
        18 => "math.max",
        19 => "math.min",
        20 => "math.modf",
        21 => "math.pow",
        22 => "math.rad",
        23 => "math.sinh",
        24 => "math.sin",
        25 => "math.sqrt",
        26 => "math.tanh",
        27 => "math.tan",
        28 => "type",
        29 => "string.byte",
        30 => "string.char",
        31 => "string.len",
        32 => "typeof",
        33 => "string.sub",
        34 => "math.clamp",
        35 => "math.sign",
        36 => "math.round",
        37 => "rawset",
        38 => "rawget",
        39 => "rawequal",
        40 => "table.insert",
        41 => "table.unpack",
        42 => "Vector3.new",
        43 => "bit32.arshift",
        44 => "bit32.band",
        45 => "bit32.bnot",
        46 => "bit32.bor",
        47 => "bit32.bxor",
        48 => "bit32.btest",
        49 => "bit32.extract",
        50 => "bit32.lrotate",
        51 => "bit32.lshift",
        52 => "bit32.replace",
        53 => "bit32.rrotate",
        54 => "bit32.rshift",
        55 => "select",
        56 => "rawlen",
        57 => "bit32.extractk",
        58 => "getmetatable",
        59 => "setmetatable",
        60 => "tonumber",
        61 => "tostring",
        62 => "bit32.countlz",
        63 => "bit32.countrz",
        64 => "table.find",
        65 => "string.format",
        66 => "table.create",
        67 => "table.move",
        68 => "table.concat",
        69 => "table.sort",
        70 => "string.match",
        71 => "string.gmatch",
        72 => "string.find",
        73 => "string.gsub",
        74 => "string.rep",
        75 => "string.reverse",
        76 => "string.upper",
        77 => "string.lower",
        78 => "string.split",
        79 => "table.pack",
        80 => "table.freeze",
        81 => "table.isfrozen",
        82 => "table.clone",
        83 => "coroutine.yield",
        84 => "bit32.byteswap",
        85 => "buffer.readi8",
        86 => "buffer.readu8",
        87 => "buffer.writei8",
        88 => "buffer.writeu8",
        89 => "buffer.readi16",
        90 => "buffer.readu16",
        91 => "buffer.writei16",
        92 => "buffer.writeu16",
        93 => "buffer.readi32",
        94 => "buffer.readu32",
        95 => "buffer.writei32",
        96 => "buffer.writeu32",
        97 => "buffer.readf32",
        98 => "buffer.writef32",
        99 => "buffer.readf64",
        100 => "buffer.writef64",
        101 => "buffer.readstring",
        102 => "buffer.writestring",
        103 => "buffer.len",
        104 => "buffer.copy",
        105 => "buffer.fill",
        106 => "buffer.create",
        107 => "table.getn",
        108 => "table.clear",
        109 => "pcall",
        110 => "xpcall",
        111 => "math.map",
        112 => "math.lerp",
        _ => "unknown_builtin",
    }
}
