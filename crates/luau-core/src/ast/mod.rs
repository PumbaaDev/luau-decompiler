/// AST nodes representing Luau source code.
/// This is the intermediate representation between bytecode analysis
/// and final source code emission.

#[derive(Debug, Clone)]
pub enum Stat {
    /// local x, y, z = expr, expr, expr
    Local {
        names: Vec<String>,
        values: Vec<Expr>,
    },
    /// x, y = expr, expr
    Assign {
        targets: Vec<Expr>,
        values: Vec<Expr>,
    },
    /// if cond then ... elseif ... else ... end
    If {
        condition: Expr,
        then_body: Vec<Stat>,
        elseif_clauses: Vec<(Expr, Vec<Stat>)>,
        else_body: Option<Vec<Stat>>,
    },
    /// while cond do ... end
    While {
        condition: Expr,
        body: Vec<Stat>,
    },
    /// repeat ... until cond
    Repeat {
        body: Vec<Stat>,
        condition: Expr,
    },
    /// for i = start, stop [, step] do ... end
    NumericFor {
        var: String,
        start: Expr,
        stop: Expr,
        step: Option<Expr>,
        body: Vec<Stat>,
    },
    /// for k, v in iter do ... end
    GenericFor {
        vars: Vec<String>,
        iterators: Vec<Expr>,
        body: Vec<Stat>,
    },
    /// return expr, expr, ...
    Return {
        values: Vec<Expr>,
    },
    /// break
    Break,
    /// continue
    Continue,
    /// do ... end
    DoBlock {
        body: Vec<Stat>,
    },
    /// expr (function call as statement)
    ExprStat(Expr),
    /// Raw comment for readability
    Comment(String),
    /// `local function NAME(params) ... end`
    ///
    /// Phase B0.52P10: dedicated variant for the local-function shorthand.
    /// Previously this pattern was emitted as
    /// `Local { names: vec![name], values: vec![Expr::Function {...}] }`
    /// which produced `local NAME = function(...) ... end`.  The shorthand
    /// form is preferred by Luau style and — unlike the assign-form — allows
    /// the function body to refer to `NAME` recursively (the name is in
    /// scope during its own body).
    LocalFunction {
        name: String,
        func: Expr,
    },
    /// `function obj:method(params) ... end`
    /// or `function obj.field(params) ... end` when `is_method` is false.
    ///
    /// Phase B0.52P10: dedicated variant for the method-function statement
    /// shorthand.  `receiver` is the dotted/indexed prefix expression
    /// (e.g. `obj`, `tbl.sub`, `M.Class`), `method` is the final name
    /// (appearing after `:` when `is_method` is true, after `.` otherwise).
    /// When `is_method` is true, the implicit `self` parameter is part of
    /// `func.params` (the emitter skips the leading "self" to reproduce the
    /// `:` shorthand); when false, `func.params` is used verbatim.
    MethodFunction {
        receiver: Expr,
        method: String,
        is_method: bool,
        func: Expr,
    },
}

#[derive(Debug, Clone)]
pub enum Expr {
    /// nil
    Nil,
    /// true / false
    Bool(bool),
    /// Numeric literal
    Number(f64),
    /// String literal
    String(String),
    /// ...
    Varargs,
    /// Variable name (local or global)
    Name(String),
    /// table.field
    Field {
        object: Box<Expr>,
        field: String,
    },
    /// table[key]
    Index {
        object: Box<Expr>,
        key: Box<Expr>,
    },
    /// Binary operation
    BinOp {
        left: Box<Expr>,
        op: BinOp,
        right: Box<Expr>,
    },
    /// Unary operation
    UnOp {
        op: UnOp,
        operand: Box<Expr>,
    },
    /// Function call: func(args)
    Call {
        func: Box<Expr>,
        args: Vec<Expr>,
    },
    /// Method call: obj:method(args)
    MethodCall {
        object: Box<Expr>,
        method: String,
        args: Vec<Expr>,
    },
    /// Function expression: function(params) ... end
    Function {
        params: Vec<String>,
        is_vararg: bool,
        body: Vec<Stat>,
    },
    /// Table constructor: { [key] = value, ... }
    Table {
        fields: Vec<TableField>,
    },
    /// Vector3.new(x, y, z)
    Vector(f32, f32, f32),
    /// Ternary: `if cond then a else b` (Luau native) or `cond and a or b`
    /// fallback depending on safety of the operands.
    ///
    /// Phase B0.52P10: dedicated variant for ternary recovery.  The emitter
    /// picks the cleaner form at render time:
    ///  - `cond and a or b` when `a` is provably non-false/non-nil
    ///    (e.g. a non-empty string literal, a non-zero numeric literal,
    ///    `true`, a table literal, a function literal — all of which are
    ///    truthy).  This form is compact and universally supported.
    ///  - `if cond then a else b` (Luau native if-expression syntax)
    ///    otherwise — this form is ALWAYS safe regardless of the truthiness
    ///    of `a`, but is syntactically heavier.
    Ternary {
        cond: Box<Expr>,
        then_expr: Box<Expr>,
        else_expr: Box<Expr>,
    },
}

#[derive(Debug, Clone)]
pub enum TableField {
    /// { expr, expr, ... } (sequential/array part)
    Sequential(Expr),
    /// { name = expr }
    Named(String, Expr),
    /// { [expr] = expr }
    Indexed(Expr, Expr),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    IDiv,
    Mod,
    Pow,
    Concat,
    Eq,
    NotEq,
    LT,
    LE,
    GT,
    GE,
    And,
    Or,
    // Luau native bitwise operators (Roblox Luau v4+)
    BAnd,
    BOr,
    BXor,
    Shl,
    Shr,
}

impl BinOp {
    pub fn as_str(&self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::IDiv => "//",
            BinOp::Mod => "%",
            BinOp::Pow => "^",
            BinOp::Concat => "..",
            BinOp::Eq => "==",
            BinOp::NotEq => "~=",
            BinOp::LT => "<",
            BinOp::LE => "<=",
            BinOp::GT => ">",
            BinOp::GE => ">=",
            BinOp::And => "and",
            BinOp::Or => "or",
            BinOp::BAnd => "&",
            BinOp::BOr => "|",
            BinOp::BXor => "~",
            BinOp::Shl => "<<",
            BinOp::Shr => ">>",
        }
    }

    pub fn precedence(&self) -> u8 {
        match self {
            BinOp::Or => 1,
            BinOp::And => 2,
            BinOp::Eq | BinOp::NotEq | BinOp::LT | BinOp::LE | BinOp::GT | BinOp::GE => 3,
            BinOp::BOr => 4,
            BinOp::BXor => 5,
            BinOp::BAnd => 6,
            BinOp::Shl | BinOp::Shr => 7,
            BinOp::Concat => 8,
            BinOp::Add | BinOp::Sub => 9,
            BinOp::Mul | BinOp::Div | BinOp::IDiv | BinOp::Mod => 10,
            BinOp::Pow => 12,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Negate,
    Not,
    Length,
    BNot,
}

impl UnOp {
    pub fn as_str(&self) -> &'static str {
        match self {
            UnOp::Negate => "-",
            UnOp::Not => "not ",
            UnOp::Length => "#",
            UnOp::BNot => "~",
        }
    }
}
