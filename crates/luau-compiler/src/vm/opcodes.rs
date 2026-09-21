//! Custom stack-VM opcode set.
//!
//! Discriminants are intentionally **canonical** (assigned in declaration
//! order). Per-build permutation (Phase 5) remaps them through a translation
//! table so the emitted Luau dispatcher sees a different byte for each op
//! every build, while the encoder always works in canonical form.

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    // --- stack manipulation ---
    PushNil = 0,
    PushTrue = 1,
    PushFalse = 2,
    PushConst = 3,  // A = const index
    Pop = 4,        // A = count
    Dup = 5,

    // --- locals & upvalues ---
    LoadLocal = 6,   // A = slot
    StoreLocal = 7,  // A = slot
    LoadUpval = 8,   // A = upvalue index
    StoreUpval = 9,
    LoadGlobal = 10, // A = const index of name string
    StoreGlobal = 11,

    // --- tables ---
    NewTable = 12,    // A = array hint, B = hash hint
    GetField = 13,    // A = const index of key string
    SetField = 14,
    GetIndex = 15,
    SetIndex = 16,
    AppendArray = 17, // pop v, table stays; table[#table+1] = v
    SetListIndex = 18, // A = numeric index (1-based); pop v, table stays

    // --- arithmetic / logic ---
    BinOp = 19, // A = sub-op (see BinSubOp)
    UnOp = 20,  // A = sub-op (see UnSubOp)

    // --- control flow (A = signed instruction offset relative to next pc) ---
    Jump = 21,
    JumpIfFalse = 22,
    JumpIfTrue = 23,
    JumpIfFalseKeep = 24, // for `and` short-circuit
    JumpIfTrueKeep = 25,  // for `or` short-circuit

    // --- calls ---
    Call = 26,       // A = nargs, B = nret (-1 = multret)
    MethodPrep = 27, // A = const index of method name; pop obj, push obj[method], push obj
    Return = 28,     // A = nret (-1 = vararg)

    // --- closures & functions ---
    Closure = 29,  // A = proto index. Followed by num_upvalues ClosureUpval entries.
    ClosureUpval = 30, // pseudo-instruction inside Closure: A = kind, B = index

    // --- varargs ---
    Vararg = 31, // A = expected count (-1 = all)

    // --- numeric for loop helpers ---
    ForNumPrep = 32, // A = exit jump offset; consumes start, stop, step from stack -> registers
    ForNumLoop = 33, // A = loop-top jump offset

    // --- generic for loop helpers ---
    ForGenPrep = 34, // A = exit jump offset
    ForGenLoop = 35, // A = loop-top jump offset; B = number of loop variables

    // --- length operator handled by UnOp ---
    // --- concat handled by BinOp ---
    // --- bitwise ops handled by BinOp / UnOp ---
}

impl Op {
    /// Canonical opcode count. Anything above this is invalid.
    pub const COUNT: u8 = 36;

    pub fn name(self) -> &'static str {
        match self {
            Op::PushNil => "PushNil",
            Op::PushTrue => "PushTrue",
            Op::PushFalse => "PushFalse",
            Op::PushConst => "PushConst",
            Op::Pop => "Pop",
            Op::Dup => "Dup",
            Op::LoadLocal => "LoadLocal",
            Op::StoreLocal => "StoreLocal",
            Op::LoadUpval => "LoadUpval",
            Op::StoreUpval => "StoreUpval",
            Op::LoadGlobal => "LoadGlobal",
            Op::StoreGlobal => "StoreGlobal",
            Op::NewTable => "NewTable",
            Op::GetField => "GetField",
            Op::SetField => "SetField",
            Op::GetIndex => "GetIndex",
            Op::SetIndex => "SetIndex",
            Op::AppendArray => "AppendArray",
            Op::SetListIndex => "SetListIndex",
            Op::BinOp => "BinOp",
            Op::UnOp => "UnOp",
            Op::Jump => "Jump",
            Op::JumpIfFalse => "JumpIfFalse",
            Op::JumpIfTrue => "JumpIfTrue",
            Op::JumpIfFalseKeep => "JumpIfFalseKeep",
            Op::JumpIfTrueKeep => "JumpIfTrueKeep",
            Op::Call => "Call",
            Op::MethodPrep => "MethodPrep",
            Op::Return => "Return",
            Op::Closure => "Closure",
            Op::ClosureUpval => "ClosureUpval",
            Op::Vararg => "Vararg",
            Op::ForNumPrep => "ForNumPrep",
            Op::ForNumLoop => "ForNumLoop",
            Op::ForGenPrep => "ForGenPrep",
            Op::ForGenLoop => "ForGenLoop",
        }
    }
}

/// Sub-ops for [`Op::BinOp`].
#[repr(i16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinSubOp {
    Add = 0,
    Sub = 1,
    Mul = 2,
    Div = 3,
    Mod = 4,
    Pow = 5,
    Concat = 6,
    Eq = 7,
    Ne = 8,
    Lt = 9,
    Le = 10,
    Gt = 11,
    Ge = 12,
    FloorDiv = 13,
}

/// Sub-ops for [`Op::UnOp`].
#[repr(i16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnSubOp {
    Neg = 0,
    Not = 1,
    Len = 2,
}
