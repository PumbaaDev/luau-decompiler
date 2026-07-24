//! Lower `full_moon` AST into our IR.
//!
//! Anything we don't understand yet returns `Err(String)` so we can extend
//! coverage incrementally instead of silently miscompiling.

use full_moon::ast as fm;
use full_moon::tokenizer::TokenType;

use super::{BinOp, Expr, LValue, Proto, Program, Stmt, TableField, UnOp};

type R<T> = Result<T, String>;

/// Lower a parsed Luau AST into a [`Program`].
pub fn lower_ast(ast: &fm::Ast) -> R<Program> {
    let mut prog = Program::default();
    // Reserve slot 0 for the top-level proto so child protos can refer to it.
    prog.protos.push(Proto::default());
    let body = lower_block(&mut prog, ast.nodes())?;
    prog.protos[0] = Proto {
        name: Some("__main__".into()),
        param_names: Vec::new(),
        is_vararg: true,
        body,
        upvalues: Vec::new(),
    };
    Ok(prog)
}

fn lower_block(prog: &mut Program, block: &fm::Block) -> R<Vec<Stmt>> {
    let mut out = Vec::new();
    for stmt in block.stmts() {
        out.push(lower_stmt(prog, stmt)?);
    }
    if let Some(last) = block.last_stmt() {
        out.push(lower_last_stmt(prog, last)?);
    }
    Ok(out)
}

fn lower_stmt(prog: &mut Program, stmt: &fm::Stmt) -> R<Stmt> {
    match stmt {
        fm::Stmt::LocalAssignment(la) => lower_local_assignment(prog, la),
        fm::Stmt::Assignment(asn) => lower_assignment(prog, asn),
        fm::Stmt::FunctionCall(call) => Ok(Stmt::ExprStmt(lower_function_call(prog, call)?)),
        fm::Stmt::If(if_stmt) => lower_if(prog, if_stmt),
        fm::Stmt::While(w) => Ok(Stmt::While {
            cond: lower_expr(prog, w.condition())?,
            body: lower_block(prog, w.block())?,
        }),
        fm::Stmt::Repeat(r) => Ok(Stmt::Repeat {
            body: lower_block(prog, r.block())?,
            cond: lower_expr(prog, r.until())?,
        }),
        fm::Stmt::NumericFor(nf) => lower_numeric_for(prog, nf),
        fm::Stmt::GenericFor(gf) => lower_generic_for(prog, gf),
        fm::Stmt::Do(d) => Ok(Stmt::Do(lower_block(prog, d.block())?)),
        fm::Stmt::FunctionDeclaration(fd) => lower_function_declaration(prog, fd),
        fm::Stmt::LocalFunction(lf) => lower_local_function(prog, lf),
        other => Err(format!("unsupported statement: {:?}", std::mem::discriminant(other))),
    }
}

fn lower_last_stmt(prog: &mut Program, last: &fm::LastStmt) -> R<Stmt> {
    match last {
        fm::LastStmt::Return(ret) => {
            let vals: R<Vec<Expr>> = ret
                .returns()
                .iter()
                .map(|e| lower_expr(prog, e))
                .collect();
            Ok(Stmt::Return(vals?))
        }
        fm::LastStmt::Break(_) => Ok(Stmt::Break),
        fm::LastStmt::Continue(_) => Ok(Stmt::Continue),
        other => Err(format!("unsupported last_stmt: {:?}", std::mem::discriminant(other))),
    }
}

fn lower_local_assignment(prog: &mut Program, la: &fm::LocalAssignment) -> R<Stmt> {
    let names: Vec<String> = la.names().iter().map(token_ident).collect();
    let values: R<Vec<Expr>> = la
        .expressions()
        .iter()
        .map(|e| lower_expr(prog, e))
        .collect();
    Ok(Stmt::Local { names, values: values? })
}

fn lower_assignment(prog: &mut Program, asn: &fm::Assignment) -> R<Stmt> {
    let targets: R<Vec<LValue>> = asn
        .variables()
        .iter()
        .map(|v| lower_lvalue(prog, v))
        .collect();
    let values: R<Vec<Expr>> = asn
        .expressions()
        .iter()
        .map(|e| lower_expr(prog, e))
        .collect();
    Ok(Stmt::Assign {
        targets: targets?,
        values: values?,
    })
}

fn lower_if(prog: &mut Program, if_stmt: &fm::If) -> R<Stmt> {
    let mut branches = Vec::new();
    branches.push((
        lower_expr(prog, if_stmt.condition())?,
        lower_block(prog, if_stmt.block())?,
    ));
    if let Some(elseifs) = if_stmt.else_if() {
        for e in elseifs {
            branches.push((
                lower_expr(prog, e.condition())?,
                lower_block(prog, e.block())?,
            ));
        }
    }
    let else_body = if let Some(else_block) = if_stmt.else_block() {
        Some(lower_block(prog, else_block)?)
    } else {
        None
    };
    Ok(Stmt::If { branches, else_body })
}

fn lower_numeric_for(prog: &mut Program, nf: &fm::NumericFor) -> R<Stmt> {
    let var = token_ident(nf.index_variable());
    let start = lower_expr(prog, nf.start())?;
    let stop = lower_expr(prog, nf.end())?;
    let step = nf.step().map(|e| lower_expr(prog, e)).transpose()?;
    let body = lower_block(prog, nf.block())?;
    Ok(Stmt::NumericFor {
        var,
        start,
        stop,
        step,
        body,
    })
}

fn lower_generic_for(prog: &mut Program, gf: &fm::GenericFor) -> R<Stmt> {
    let names: Vec<String> = gf.names().iter().map(token_ident).collect();
    let iters: R<Vec<Expr>> = gf
        .expressions()
        .iter()
        .map(|e| lower_expr(prog, e))
        .collect();
    Ok(Stmt::GenericFor {
        names,
        iters: iters?,
        body: lower_block(prog, gf.block())?,
    })
}

fn lower_function_declaration(prog: &mut Program, fd: &fm::FunctionDeclaration) -> R<Stmt> {
    // Convert dotted path / :method into nested field assignment.
    let proto_idx = lower_function_body(prog, fd.body(), None)?;
    let name = fd.name();
    // Build the LValue chain from the name path.
    let names: Vec<&full_moon::tokenizer::TokenReference> = name.names().iter().collect();
    let method = name.method_name();
    // We need `names` in &TokenReference form; `Punctuated::iter` yields the
    // payload type directly.
    if names.is_empty() {
        return Err("function declaration has no name".into());
    }
    let mut chain_expr = Expr::Name(token_ident(names[0]));
    for nref in &names[1..] {
        chain_expr = Expr::Field {
            obj: Box::new(chain_expr),
            name: token_ident(nref),
        };
    }
    let final_target = if let Some(m) = method {
        // foo:bar(...) — declared with implicit `self` parameter.
        let method_name = token_ident(m);
        if let Some(p) = prog.protos.get_mut(proto_idx) {
            p.param_names.insert(0, "self".into());
        }
        LValue::Field {
            obj: Box::new(chain_expr),
            name: method_name,
        }
    } else {
        match chain_expr {
            Expr::Name(n) => LValue::Global(n),
            Expr::Field { obj, name } => LValue::Field { obj, name },
            _ => return Err("malformed function decl target".into()),
        }
    };

    Ok(Stmt::Assign {
        targets: vec![final_target],
        values: vec![Expr::Function(proto_idx)],
    })
}

fn lower_local_function(prog: &mut Program, lf: &fm::LocalFunction) -> R<Stmt> {
    let name = token_ident(lf.name());
    let proto_idx = lower_function_body(prog, lf.body(), Some(name.clone()))?;
    Ok(Stmt::LocalFunction { name, proto_idx })
}

fn lower_function_body(
    prog: &mut Program,
    body: &fm::FunctionBody,
    name_hint: Option<String>,
) -> R<usize> {
    let mut params: Vec<String> = Vec::new();
    let mut is_vararg = false;
    for p in body.parameters() {
        match p {
            fm::Parameter::Name(tok) => params.push(token_ident(tok)),
            fm::Parameter::Ellipsis(_) => is_vararg = true,
            _ => return Err("unsupported parameter shape".into()),
        }
    }

    // Reserve a slot first so nested protos can reference us if needed.
    let idx = prog.protos.len();
    prog.protos.push(Proto::default());

    let block_stmts = lower_block(prog, body.block())?;

    prog.protos[idx] = Proto {
        name: name_hint,
        param_names: params,
        is_vararg,
        body: block_stmts,
        upvalues: Vec::new(),
    };
    Ok(idx)
}

fn lower_function_call(prog: &mut Program, call: &fm::FunctionCall) -> R<Expr> {
    // full_moon represents foo.bar:baz(x)(y) as a prefix with a chain of
    // "suffixes". Build it up left-to-right.
    let mut current = lower_prefix(prog, call.prefix())?;
    for suffix in call.suffixes() {
        current = apply_suffix(prog, current, suffix)?;
    }
    // The final expression must be a call. Anything else is a parse error.
    match current {
        Expr::Call { .. } | Expr::MethodCall { .. } => Ok(current),
        _ => Err("function-call statement did not end in a call".into()),
    }
}

fn lower_prefix(prog: &mut Program, prefix: &fm::Prefix) -> R<Expr> {
    match prefix {
        fm::Prefix::Name(tok) => Ok(Expr::Name(token_ident(tok))),
        fm::Prefix::Expression(expr) => lower_expr(prog, expr),
        other => Err(format!("unsupported prefix: {:?}", std::mem::discriminant(other))),
    }
}

fn apply_suffix(prog: &mut Program, base: Expr, suffix: &fm::Suffix) -> R<Expr> {
    match suffix {
        fm::Suffix::Index(idx) => match idx {
            fm::Index::Dot { name, .. } => Ok(Expr::Field {
                obj: Box::new(base),
                name: token_ident(name),
            }),
            fm::Index::Brackets { expression, .. } => Ok(Expr::Index {
                obj: Box::new(base),
                key: Box::new(lower_expr(prog, expression)?),
            }),
            other => Err(format!("unsupported index: {:?}", std::mem::discriminant(other))),
        },
        fm::Suffix::Call(c) => match c {
            fm::Call::AnonymousCall(args) => Ok(Expr::Call {
                func: Box::new(base),
                args: lower_call_args(prog, args)?,
            }),
            fm::Call::MethodCall(m) => Ok(Expr::MethodCall {
                obj: Box::new(base),
                method: token_ident(m.name()),
                args: lower_call_args(prog, m.args())?,
            }),
            other => Err(format!("unsupported call: {:?}", std::mem::discriminant(other))),
        },
        other => Err(format!("unsupported suffix: {:?}", std::mem::discriminant(other))),
    }
}

fn lower_call_args(prog: &mut Program, args: &fm::FunctionArgs) -> R<Vec<Expr>> {
    match args {
        fm::FunctionArgs::Parentheses { arguments, .. } => {
            arguments.iter().map(|e| lower_expr(prog, e)).collect()
        }
        fm::FunctionArgs::String(tok) => Ok(vec![lower_string_token(tok)?]),
        fm::FunctionArgs::TableConstructor(tc) => Ok(vec![lower_table_constructor(prog, tc)?]),
        other => Err(format!(
            "unsupported call args: {:?}",
            std::mem::discriminant(other)
        )),
    }
}

fn lower_lvalue(prog: &mut Program, v: &fm::Var) -> R<LValue> {
    match v {
        fm::Var::Name(tok) => Ok(LValue::Global(token_ident(tok))),
        fm::Var::Expression(ve) => {
            // Walk prefix + suffixes, but the final suffix must be an index.
            let mut current = lower_prefix(prog, ve.prefix())?;
            let suffixes: Vec<&fm::Suffix> = ve.suffixes().collect();
            if suffixes.is_empty() {
                return Err("empty var expression".into());
            }
            for s in &suffixes[..suffixes.len() - 1] {
                current = apply_suffix(prog, current, s)?;
            }
            match suffixes.last().unwrap() {
                fm::Suffix::Index(fm::Index::Dot { name, .. }) => Ok(LValue::Field {
                    obj: Box::new(current),
                    name: token_ident(name),
                }),
                fm::Suffix::Index(fm::Index::Brackets { expression, .. }) => Ok(LValue::Index {
                    obj: Box::new(current),
                    key: Box::new(lower_expr(prog, expression)?),
                }),
                _ => Err("var expression must end in an index".into()),
            }
        }
        other => Err(format!("unsupported var: {:?}", std::mem::discriminant(other))),
    }
}

fn lower_expr(prog: &mut Program, expr: &fm::Expression) -> R<Expr> {
    match expr {
        fm::Expression::Number(tok) => Ok(Expr::Number(parse_number(tok)?)),
        fm::Expression::String(tok) => lower_string_token(tok),
        fm::Expression::Symbol(tok) => {
            // nil / true / false / ...
            match tok.token().token_type() {
                TokenType::Symbol { symbol } => {
                    let s = format!("{symbol}");
                    match s.as_str() {
                        "nil" => Ok(Expr::Nil),
                        "..." => Ok(Expr::Vararg),
                        "true" => Ok(Expr::Bool(true)),
                        "false" => Ok(Expr::Bool(false)),
                        other => Err(format!("unsupported symbol expression: {other}")),
                    }
                }
                _ => Err("expected symbol token".into()),
            }
        }
        fm::Expression::Var(v) => match v {
            fm::Var::Name(tok) => Ok(Expr::Name(token_ident(tok))),
            fm::Var::Expression(ve) => {
                let mut current = lower_prefix(prog, ve.prefix())?;
                for s in ve.suffixes() {
                    current = apply_suffix(prog, current, s)?;
                }
                Ok(current)
            }
            other => Err(format!("unsupported var expr: {:?}", std::mem::discriminant(other))),
        },
        fm::Expression::FunctionCall(call) => lower_function_call(prog, call),
        fm::Expression::BinaryOperator { lhs, binop, rhs } => Ok(Expr::BinOp {
            op: lower_binop(binop)?,
            lhs: Box::new(lower_expr(prog, lhs)?),
            rhs: Box::new(lower_expr(prog, rhs)?),
        }),
        fm::Expression::UnaryOperator { unop, expression } => Ok(Expr::UnOp {
            op: lower_unop(unop)?,
            rhs: Box::new(lower_expr(prog, expression)?),
        }),
        fm::Expression::Parentheses { expression, .. } => lower_expr(prog, expression),
        fm::Expression::TableConstructor(tc) => lower_table_constructor(prog, tc),
        fm::Expression::Function(anon) => {
            let idx = lower_function_body(prog, anon.body(), None)?;
            Ok(Expr::Function(idx))
        }
        fm::Expression::IfExpression(ie) => lower_if_expression(prog, ie),
        other => Err(format!(
            "unsupported expression: {:?}",
            std::mem::discriminant(other)
        )),
    }
}

fn lower_if_expression(
    prog: &mut Program,
    ie: &full_moon::ast::luau::IfExpression,
) -> R<Expr> {
    // For Phase 1 we keep it simple: nested ternary via and/or, accepting the
    // standard Lua truthiness caveat.
    let cond = lower_expr(prog, ie.condition())?;
    let then_v = lower_expr(prog, ie.if_expression())?;
    let else_v = lower_expr(prog, ie.else_expression())?;
    if let Some(elseifs) = ie.else_if_expressions() {
        // Build right-associative chain: c1 and v1 or c2 and v2 or ... else_v
        let mut chain: Expr = else_v;
        for ei in elseifs.iter().rev() {
            let ec = lower_expr(prog, ei.condition())?;
            let ev = lower_expr(prog, ei.expression())?;
            chain = Expr::BinOp {
                op: BinOp::Or,
                lhs: Box::new(Expr::BinOp {
                    op: BinOp::And,
                    lhs: Box::new(ec),
                    rhs: Box::new(ev),
                }),
                rhs: Box::new(chain),
            };
        }
        Ok(Expr::BinOp {
            op: BinOp::Or,
            lhs: Box::new(Expr::BinOp {
                op: BinOp::And,
                lhs: Box::new(cond),
                rhs: Box::new(then_v),
            }),
            rhs: Box::new(chain),
        })
    } else {
        Ok(Expr::BinOp {
            op: BinOp::Or,
            lhs: Box::new(Expr::BinOp {
                op: BinOp::And,
                lhs: Box::new(cond),
                rhs: Box::new(then_v),
            }),
            rhs: Box::new(else_v),
        })
    }
}

fn lower_table_constructor(prog: &mut Program, tc: &fm::TableConstructor) -> R<Expr> {
    let mut fields = Vec::new();
    for f in tc.fields() {
        let lowered = match f {
            fm::Field::NoKey(e) => TableField::Array(lower_expr(prog, e)?),
            fm::Field::NameKey { key, value, .. } => TableField::Named {
                name: token_ident(key),
                value: lower_expr(prog, value)?,
            },
            fm::Field::ExpressionKey { key, value, .. } => TableField::Indexed {
                key: lower_expr(prog, key)?,
                value: lower_expr(prog, value)?,
            },
            other => return Err(format!("unsupported field: {:?}", std::mem::discriminant(other))),
        };
        fields.push(lowered);
    }
    Ok(Expr::Table(fields))
}

fn lower_binop(b: &fm::BinOp) -> R<BinOp> {
    use full_moon::ast::BinOp as B;
    Ok(match b {
        B::Plus(_) => BinOp::Add,
        B::Minus(_) => BinOp::Sub,
        B::Star(_) => BinOp::Mul,
        B::Slash(_) => BinOp::Div,
        B::Percent(_) => BinOp::Mod,
        B::Caret(_) => BinOp::Pow,
        B::TwoDots(_) => BinOp::Concat,
        B::TwoEqual(_) => BinOp::Eq,
        B::TildeEqual(_) => BinOp::Ne,
        B::LessThan(_) => BinOp::Lt,
        B::LessThanEqual(_) => BinOp::Le,
        B::GreaterThan(_) => BinOp::Gt,
        B::GreaterThanEqual(_) => BinOp::Ge,
        B::And(_) => BinOp::And,
        B::Or(_) => BinOp::Or,
        B::DoubleSlash(_) => BinOp::FloorDiv,
        other => return Err(format!("unsupported binop: {:?}", std::mem::discriminant(other))),
    })
}

fn lower_unop(u: &fm::UnOp) -> R<UnOp> {
    use full_moon::ast::UnOp as U;
    Ok(match u {
        U::Minus(_) => UnOp::Neg,
        U::Not(_) => UnOp::Not,
        U::Hash(_) => UnOp::Len,
        other => return Err(format!("unsupported unop: {:?}", std::mem::discriminant(other))),
    })
}

fn lower_string_token(tok: &full_moon::tokenizer::TokenReference) -> R<Expr> {
    match tok.token().token_type() {
        TokenType::StringLiteral { literal, .. } => Ok(Expr::String(literal.to_string())),
        _ => Err("expected string token".into()),
    }
}

fn token_ident(tok: &full_moon::tokenizer::TokenReference) -> String {
    match tok.token().token_type() {
        TokenType::Identifier { identifier } => identifier.to_string(),
        TokenType::Symbol { symbol } => format!("{symbol}"),
        other => format!("{other:?}"),
    }
}

fn parse_number(tok: &full_moon::tokenizer::TokenReference) -> R<f64> {
    match tok.token().token_type() {
        TokenType::Number { text } => {
            let s = text.to_string();
            let cleaned = s.replace('_', "");
            if let Some(rest) = cleaned.strip_prefix("0x").or_else(|| cleaned.strip_prefix("0X")) {
                u64::from_str_radix(rest, 16)
                    .map(|v| v as f64)
                    .map_err(|e| format!("bad hex number {s}: {e}"))
            } else if let Some(rest) = cleaned.strip_prefix("0b").or_else(|| cleaned.strip_prefix("0B")) {
                u64::from_str_radix(rest, 2)
                    .map(|v| v as f64)
                    .map_err(|e| format!("bad binary number {s}: {e}"))
            } else {
                cleaned
                    .parse::<f64>()
                    .map_err(|e| format!("bad number {s}: {e}"))
            }
        }
        _ => Err("expected number token".into()),
    }
}
