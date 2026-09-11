//! Small typed IR sitting between `full_moon`'s AST and the stack VM.
//!
//! Each `Proto` is a function body (the script itself is the top-level proto).
//! Statements and expressions stay tree-shaped — flattening happens in
//! `vm::encoder` because that's where stack effects matter.

pub mod lower;

/// A complete program: ordered list of protos, with index 0 = top-level script.
#[derive(Debug, Clone, Default)]
pub struct Program {
    pub protos: Vec<Proto>,
}

/// A function body.
#[derive(Debug, Clone, Default)]
pub struct Proto {
    /// Optional name (debug only; never emitted to output).
    pub name: Option<String>,
    /// Parameter names in declaration order. `num_params` derives from
    /// this; runtime binds `args[i]` to `locals[i]` for i in 0..num_params.
    pub param_names: Vec<String>,
    /// Whether the function accepts varargs.
    pub is_vararg: bool,
    /// Statements forming the body (excluding parameter materialization —
    /// the encoder allocates parameter slots before the body runs).
    pub body: Vec<Stmt>,
    /// Names captured from the enclosing scope (lexical order, for debug).
    pub upvalues: Vec<String>,
}

impl Proto {
    pub fn num_params(&self) -> u16 {
        self.param_names.len() as u16
    }
}

#[derive(Debug, Clone)]
pub enum Stmt {
    /// `local a, b, c = e1, e2, e3` (any RHS may be missing -> nil).
    Local {
        names: Vec<String>,
        values: Vec<Expr>,
    },
    /// `targets = values` (parallel assignment).
    Assign {
        targets: Vec<LValue>,
        values: Vec<Expr>,
    },
    /// Expression evaluated for side-effects (typically a call).
    ExprStmt(Expr),
    If {
        branches: Vec<(Expr, Vec<Stmt>)>,
        else_body: Option<Vec<Stmt>>,
    },
    While {
        cond: Expr,
        body: Vec<Stmt>,
    },
    Repeat {
        body: Vec<Stmt>,
        cond: Expr,
    },
    NumericFor {
        var: String,
        start: Expr,
        stop: Expr,
        step: Option<Expr>,
        body: Vec<Stmt>,
    },
    GenericFor {
        names: Vec<String>,
        iters: Vec<Expr>,
        body: Vec<Stmt>,
    },
    Return(Vec<Expr>),
    Break,
    Continue,
    Do(Vec<Stmt>),
    /// `local function name(...)` — the local is declared *before* the body
    /// is evaluated, so recursive self-reference works.
    LocalFunction { name: String, proto_idx: usize },
}

#[derive(Debug, Clone)]
pub enum LValue {
    Local(String),
    Global(String),
    Field { obj: Box<Expr>, name: String },
    Index { obj: Box<Expr>, key: Box<Expr> },
}

#[derive(Debug, Clone)]
pub enum Expr {
    Nil,
    Bool(bool),
    Number(f64),
    String(String),
    Vararg,
    /// Bare identifier; lowering decides local vs upvalue vs global.
    Name(String),
    Field {
        obj: Box<Expr>,
        name: String,
    },
    Index {
        obj: Box<Expr>,
        key: Box<Expr>,
    },
    BinOp {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    UnOp {
        op: UnOp,
        rhs: Box<Expr>,
    },
    Call {
        func: Box<Expr>,
        args: Vec<Expr>,
    },
    MethodCall {
        obj: Box<Expr>,
        method: String,
        args: Vec<Expr>,
    },
    Function(usize),
    Table(Vec<TableField>),
}

#[derive(Debug, Clone)]
pub enum TableField {
    /// `value` (sequential).
    Array(Expr),
    /// `name = value`.
    Named { name: String, value: Expr },
    /// `[key] = value`.
    Indexed { key: Expr, value: Expr },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    Concat,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    FloorDiv,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
    Len,
}
