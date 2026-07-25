//! Emit AST nodes as Luau source code with proper indentation.

use crate::ast::*;

const INDENT: &str = "    ";

/// Emit a block of statements
pub fn emit_block(out: &mut String, stmts: &[Stat], depth: usize) {
    for stmt in stmts {
        emit_stat(out, stmt, depth);
    }
}

fn indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str(INDENT);
    }
}

fn emit_stat(out: &mut String, stmt: &Stat, depth: usize) {
    match stmt {
        Stat::Local { names, values } => {
            // Phase B0.108: single-name local with non-truthy ternary →
            // expand to `local x; if cond then x = a else x = b end`.
            if names.len() == 1 && values.len() == 1 {
                if let Expr::Ternary { cond, then_expr, else_expr } = &values[0] {
                    if !is_provably_truthy(then_expr) {
                        indent(out, depth);
                        out.push_str(&format!("local {}\n", names[0]));
                        indent(out, depth);
                        out.push_str(&format!("if {} then\n", emit_expr(cond, depth)));
                        indent(out, depth + 1);
                        out.push_str(&format!("{} = {}\n", names[0], emit_expr(then_expr, depth + 1)));
                        indent(out, depth);
                        out.push_str("else\n");
                        indent(out, depth + 1);
                        out.push_str(&format!("{} = {}\n", names[0], emit_expr(else_expr, depth + 1)));
                        indent(out, depth);
                        out.push_str("end\n");
                        return;
                    }
                }
            }
            indent(out, depth);
            out.push_str("local ");
            out.push_str(&names.join(", "));
            // Phase B0.94b: strip trailing nil values from local declarations.
            // `local x = nil` → `local x`, `local x, y = 1, nil` → `local x, y = 1`.
            // Non-trailing nils are preserved: `local x, y = nil, 1` stays as-is.
            let trimmed: Vec<&Expr> = {
                let mut v: Vec<&Expr> = values.iter().collect();
                while v.last().map_or(false, |e| matches!(e, Expr::Nil)) {
                    v.pop();
                }
                v
            };
            if !trimmed.is_empty() {
                out.push_str(" = ");
                let exprs: Vec<String> = trimmed.iter().map(|e| emit_expr(e, depth)).collect();
                out.push_str(&exprs.join(", "));
            }
            out.push('\n');
        }

        Stat::Assign { targets, values } => {
            // B0.111: final safety net — reject assignments whose emitted
            // target is not a valid Luau lvalue.  Post-passes like table
            // reconstruction can inline definitions into targets, creating
            // forms like `(expr + {table})[k] = v` that aren't syntactically
            // valid.  Emit as a comment instead of broken syntax.
            if targets.len() == 1 {
                let probe = emit_expr(&targets[0], 0);
                if probe.starts_with('(') || probe.starts_with('"') || probe.starts_with('\'') {
                    indent(out, depth);
                    let trunc = if probe.len() > 80 { &probe[..80] } else { &probe };
                    let val_str = if values.len() == 1 {
                        let v = emit_expr(&values[0], 0);
                        if v.len() > 80 { v[..80].to_string() } else { v }
                    } else { "...".to_string() };
                    out.push_str(&format!("-- invalid lvalue: {} = {}\n", trunc, val_str));
                    return;
                }
            }
            // Phase B0.108: single-target assign with non-truthy ternary →
            // expand to `if cond then x = a else x = b end`.
            if targets.len() == 1 && values.len() == 1 {
                if let Expr::Ternary { cond, then_expr, else_expr } = &values[0] {
                    if !is_provably_truthy(then_expr) {
                        indent(out, depth);
                        out.push_str(&format!("if {} then\n", emit_expr(cond, depth)));
                        indent(out, depth + 1);
                        out.push_str(&format!("{} = {}\n", emit_expr(&targets[0], depth + 1), emit_expr(then_expr, depth + 1)));
                        indent(out, depth);
                        out.push_str("else\n");
                        indent(out, depth + 1);
                        out.push_str(&format!("{} = {}\n", emit_expr(&targets[0], depth + 1), emit_expr(else_expr, depth + 1)));
                        indent(out, depth);
                        out.push_str("end\n");
                        return;
                    }
                }
            }
            indent(out, depth);
            // Phase B0.98: compound assignment — x = x + 1 → x += 1
            let emitted_compound = if targets.len() == 1 && values.len() == 1 {
                if let Expr::BinOp { left, op, right } = &values[0] {
                    // Phase B0.103: only emit compound ops that Luau supports.
                    // Luau supports: +=, -=, *=, /=, %=, ^=, ..=
                    // Luau does NOT support: //= (floor div compound)
                    let compound_op = match op {
                        BinOp::Add => Some("+="),
                        BinOp::Sub => Some("-="),
                        BinOp::Mul => Some("*="),
                        BinOp::Div => Some("/="),
                        BinOp::Mod => Some("%="),
                        BinOp::Pow => Some("^="),
                        BinOp::Concat => Some("..="),
                        _ => None,
                    };
                    if let Some(cop) = compound_op {
                        let target_str = emit_expr(&targets[0], depth);
                        let left_str = emit_expr(left, depth);
                        if target_str == left_str {
                            out.push_str(&target_str);
                            out.push_str(" ");
                            out.push_str(cop);
                            out.push_str(" ");
                            out.push_str(&emit_expr(right, depth));
                            out.push('\n');
                            true
                        } else { false }
                    } else { false }
                } else { false }
            } else { false };
            if !emitted_compound {
                let lhs: Vec<String> = targets.iter().map(|e| emit_expr(e, depth)).collect();
                out.push_str(&lhs.join(", "));
                out.push_str(" = ");
                let rhs: Vec<String> = values.iter().map(|e| emit_expr(e, depth)).collect();
                out.push_str(&rhs.join(", "));
                out.push('\n');
            }
        }

        Stat::If {
            condition,
            then_body,
            elseif_clauses,
            else_body,
        } => {
            indent(out, depth);
            out.push_str(&format!("if {} then\n", emit_expr(condition, depth)));
            emit_block(out, then_body, depth + 1);
            for (cond, body) in elseif_clauses {
                indent(out, depth);
                out.push_str(&format!("elseif {} then\n", emit_expr(cond, depth)));
                emit_block(out, body, depth + 1);
            }
            if let Some(else_body) = else_body {
                indent(out, depth);
                out.push_str("else\n");
                emit_block(out, else_body, depth + 1);
            }
            indent(out, depth);
            out.push_str("end\n");
        }

        Stat::While { condition, body } => {
            indent(out, depth);
            out.push_str(&format!("while {} do\n", emit_expr(condition, depth)));
            emit_block(out, body, depth + 1);
            indent(out, depth);
            out.push_str("end\n");
        }

        Stat::Repeat { body, condition } => {
            indent(out, depth);
            out.push_str("repeat\n");
            emit_block(out, body, depth + 1);
            indent(out, depth);
            out.push_str(&format!("until {}\n", emit_expr(condition, depth)));
        }

        Stat::NumericFor {
            var,
            start,
            stop,
            step,
            body,
        } => {
            indent(out, depth);
            let step_str = step
                .as_ref()
                .map(|s| format!(", {}", emit_expr(s, depth)))
                .unwrap_or_default();
            out.push_str(&format!(
                "for {} = {}, {}{} do\n",
                var,
                emit_expr(start, depth),
                emit_expr(stop, depth),
                step_str
            ));
            emit_block(out, body, depth + 1);
            indent(out, depth);
            out.push_str("end\n");
        }

        Stat::GenericFor {
            vars,
            iterators,
            body,
        } => {
            indent(out, depth);
            let iters: Vec<String> = iterators.iter().map(|e| emit_expr(e, depth)).collect();
            out.push_str(&format!(
                "for {} in {} do\n",
                vars.join(", "),
                iters.join(", ")
            ));
            emit_block(out, body, depth + 1);
            indent(out, depth);
            out.push_str("end\n");
        }

        Stat::Return { values } => {
            indent(out, depth);
            if values.is_empty() {
                out.push_str("return\n");
            } else if values.len() == 1 {
                if let Expr::Ternary { cond, then_expr, else_expr } = &values[0] {
                    if !is_provably_truthy(then_expr) {
                        // Phase B0.107: full_moon 2.1.1 doesn't support Luau
                        // if-expressions.  When a return has a single Ternary
                        // value whose then-branch isn't provably truthy, we
                        // can't use `return cond and a or b` (semantically
                        // wrong) or `return if c then a else b` (unparseable).
                        // Expand to if-statement form instead.
                        out.push_str(&format!("if {} then\n", emit_expr(cond, depth)));
                        indent(out, depth + 1);
                        out.push_str(&format!("return {}\n", emit_expr(then_expr, depth + 1)));
                        indent(out, depth);
                        out.push_str("else\n");
                        indent(out, depth + 1);
                        out.push_str(&format!("return {}\n", emit_expr(else_expr, depth + 1)));
                        indent(out, depth);
                        out.push_str("end\n");
                        return;
                    }
                }
                out.push_str(&format!("return {}\n", emit_expr(&values[0], depth)));
            } else {
                let exprs: Vec<String> = values.iter().map(|e| emit_expr(e, depth)).collect();
                out.push_str(&format!("return {}\n", exprs.join(", ")));
            }
        }

        Stat::Break => {
            indent(out, depth);
            out.push_str("break\n");
        }

        Stat::Continue => {
            indent(out, depth);
            out.push_str("continue\n");
        }

        Stat::DoBlock { body } => {
            indent(out, depth);
            out.push_str("do\n");
            emit_block(out, body, depth + 1);
            indent(out, depth);
            out.push_str("end\n");
        }

        Stat::ExprStat(expr) => {
            indent(out, depth);
            out.push_str(&emit_expr(expr, depth));
            out.push('\n');
        }

        Stat::Comment(text) => {
            indent(out, depth);
            out.push_str(&format!("-- {}\n", text));
        }

        // Phase B0.52P10: `local function NAME(params) ... end`.
        // We only accept `func` being an `Expr::Function` — any other shape
        // is a lifter bug; defensively fall back to the assign-form rather
        // than crashing.
        Stat::LocalFunction { name, func } => {
            indent(out, depth);
            if let Expr::Function { params, is_vararg, body } = func {
                let mut param_list = params.join(", ");
                if *is_vararg {
                    if !params.is_empty() {
                        param_list.push_str(", ");
                    }
                    param_list.push_str("...");
                }
                out.push_str(&format!("local function {}({})\n", name, param_list));
                emit_block(out, body, depth + 1);
                indent(out, depth);
                out.push_str("end\n");
            } else {
                out.push_str(&format!(
                    "local {} = {}\n",
                    name,
                    emit_expr(func, depth)
                ));
            }
        }

        // Phase B0.52P10: `function obj:method(params) ... end` shorthand.
        // When `is_method` is true we expect `self` to be the FIRST formal
        // parameter of the underlying `Expr::Function` — we strip it so the
        // rendered source matches the idiomatic `:method` sugar.  When
        // false we render as `function obj.method(params) ... end`.
        Stat::MethodFunction { receiver, method, is_method, func } => {
            indent(out, depth);
            if let Expr::Function { params, is_vararg, body } = func {
                let sep = if *is_method { ":" } else { "." };
                // For `:method` sugar, skip a leading implicit `self` param
                // if present.  If the lifter produced method-function shape
                // but didn't include self as first param, we still render
                // the remaining params verbatim — this is defensive.
                let params_slice: &[String] = if *is_method && !params.is_empty() {
                    &params[1..]
                } else {
                    &params[..]
                };
                let mut param_list = params_slice.join(", ");
                if *is_vararg {
                    if !params_slice.is_empty() {
                        param_list.push_str(", ");
                    }
                    param_list.push_str("...");
                }
                out.push_str(&format!(
                    "function {}{}{}({})\n",
                    emit_prefix_expr(receiver, depth),
                    sep,
                    method,
                    param_list
                ));
                emit_block(out, body, depth + 1);
                indent(out, depth);
                out.push_str("end\n");
            } else {
                // Defensive fallback: treat as assign.
                let sep = if *is_method { ":" } else { "." };
                out.push_str(&format!(
                    "{}{}{} = {}\n",
                    emit_prefix_expr(receiver, depth),
                    sep,
                    method,
                    emit_expr(func, depth)
                ));
            }
        }
    }
}

fn emit_expr(expr: &Expr, depth: usize) -> String {
    match expr {
        Expr::Nil => "nil".to_string(),
        Expr::Bool(b) => b.to_string(),
        Expr::Number(n) => emit_number(*n),
        Expr::String(s) => emit_string(s),
        Expr::Varargs => "...".to_string(),
        Expr::Name(n) => {
            // Defence-in-depth: if the name is not a valid identifier, it's a
            // data string that leaked through the lifter as Expr::Name. Emit it
            // as a quoted string to avoid bare-text parse errors (e.g., apostrophes
            // in "we've" → unterminated string literal in full_moon).
            if is_valid_identifier(n) {
                n.clone()
            } else {
                emit_string(n)
            }
        }

        Expr::Field { object, field } => {
            let obj_str = emit_prefix_expr(object, depth);
            if is_valid_identifier(field) {
                format!("{}.{}", obj_str, field)
            } else {
                let s = emit_string(field);
                // Phase B0.109: avoid `[[[` ambiguity (see Index handler).
                if s.starts_with('[') {
                    format!("{}[ {}]", obj_str, s)
                } else {
                    format!("{}[{}]", obj_str, s)
                }
            }
        }

        Expr::Index { object, key } => {
            let obj_str = emit_prefix_expr(object, depth);
            // Phase C10M: flatten `obj["Ident"]` → `obj.Ident` when the key is
            // a string literal that forms a valid Luau identifier. Semantically
            // identical, far easier to read. Reserved words stay bracketed.
            if let Expr::String(s) = key.as_ref() {
                if is_valid_identifier(s) {
                    return format!("{}.{}", obj_str, s);
                }
            }
            let key_str = emit_expr(key, depth);
            // Phase B0.109: avoid `[[[` ambiguity when key is a long string.
            // `foo[[[str]]]` is parsed as `foo` + index-open `[` + long-string
            // `[[str]]` + unmatched `]`, causing a parse error.  Insert a space
            // between `[` and `[[` to break the ambiguity.
            if key_str.starts_with('[') {
                format!("{}[ {}]", obj_str, key_str)
            } else {
                format!("{}[{}]", obj_str, key_str)
            }
        }

        Expr::BinOp { left, op, right } => {
            // Phase B0.103: bitwise operators must emit as bit32.* function calls
            // for Luau compatibility. Roblox Luau doesn't support C-style `&`,
            // `|`, `~`, `<<`, `>>` operator syntax — these parse as invalid tokens.
            // The VM has native BAND/BOR/etc opcodes but the source must use the
            // bit32 library.
            match op {
                BinOp::BAnd => format!("bit32.band({}, {})", emit_expr(left, depth), emit_expr(right, depth)),
                BinOp::BOr => format!("bit32.bor({}, {})", emit_expr(left, depth), emit_expr(right, depth)),
                BinOp::BXor => format!("bit32.bxor({}, {})", emit_expr(left, depth), emit_expr(right, depth)),
                BinOp::Shl => format!("bit32.lshift({}, {})", emit_expr(left, depth), emit_expr(right, depth)),
                BinOp::Shr => format!("bit32.rshift({}, {})", emit_expr(left, depth), emit_expr(right, depth)),
                _ => {
                    let left_str = emit_expr_with_parens(left, Some(*op), true, depth);
                    let right_str = emit_expr_with_parens(right, Some(*op), false, depth);
                    format!("{} {} {}", left_str, op.as_str(), right_str)
                }
            }
        }

        Expr::UnOp { op, operand } => {
            // B0.49b: if we see `~"some_identifier"` on the string literal of a
            // known identifier, this is a lifter quirk where an upvalue-inferred
            // name (B0.43B) got materialized as an Expr::String and then a Bnot
            // opcode was applied.  Bitwise-NOT on a string literal is a hard
            // runtime error in Luau — it cannot possibly be what the source
            // said.  Strip the BNot and emit the string as a bare identifier.
            if let (UnOp::BNot, Expr::String(s)) = (op, operand.as_ref()) {
                if crate::decompiler::is_valid_luau_identifier(s) {
                    return s.clone();
                }
            }
            let needs_parens = match op {
                // `-(-x)` would emit `--x` (comment syntax!) so we must
                // parenthesise.  Similarly `-<binop>` can change meaning:
                // `-(a - b)` must NOT become `-a - b`.
                UnOp::Negate => matches!(
                    operand.as_ref(),
                    Expr::UnOp { .. } | Expr::BinOp { .. }
                ),
                // `not <binop>` needs parens: `not a or b` != `not (a or b)`.
                UnOp::Not => matches!(operand.as_ref(), Expr::BinOp { .. }),
                // `#<binop>` needs parens: `#a .. b` != `#(a .. b)`.
                UnOp::Length => matches!(operand.as_ref(), Expr::BinOp { .. }),
                // `~<binop>` is unambiguous in Luau bitwise context — no parens needed.
                UnOp::BNot => false,
            };
            let operand_str = if needs_parens {
                format!("({})", emit_expr(operand, depth))
            } else {
                emit_expr(operand, depth)
            };
            match op {
                UnOp::Not => format!("not {}", operand_str),
                UnOp::Negate => format!("-{}", operand_str),
                UnOp::Length => format!("#{}", operand_str),
                // Phase B0.103: BNot emits as bit32.bnot() for Luau compatibility.
                UnOp::BNot => format!("bit32.bnot({})", emit_expr(operand, depth)),
            }
        }

        Expr::Call { func, args } => {
            let func_str = emit_prefix_expr(func, depth);
            let args_str: Vec<String> = args.iter().map(|a| emit_expr(a, depth)).collect();
            format!("{}({})", func_str, args_str.join(", "))
        }

        Expr::MethodCall {
            object,
            method,
            args,
        } => {
            let obj_str = emit_prefix_expr(object, depth);
            let args_str: Vec<String> = args.iter().map(|a| emit_expr(a, depth)).collect();
            if is_valid_identifier(method) {
                format!(
                    "{}:{}({})",
                    obj_str,
                    method,
                    args_str.join(", ")
                )
            } else {
                // Desugar to index + call, passing self explicitly
                let s = emit_string(method);
                // Phase B0.109: avoid `[[[` ambiguity.
                if s.starts_with('[') {
                    format!("{}[ {}]({})", obj_str, s, args_str.join(", "))
                } else {
                    format!("{}[{}]({})", obj_str, s, args_str.join(", "))
                }
            }
        }

        Expr::Function {
            params,
            is_vararg,
            body,
        } => {
            let mut param_list = params.join(", ");
            if *is_vararg {
                if !params.is_empty() {
                    param_list.push_str(", ");
                }
                param_list.push_str("...");
            }

            let mut out = format!("function({})\n", param_list);
            emit_block(&mut out, body, depth + 1);
            indent(&mut out, depth);
            out.push_str("end");
            out
        }

        Expr::Table { fields } => {
            if fields.is_empty() {
                return "{}".to_string();
            }

            // Short tables (1-3 simple sequential values) can be inline
            let is_short_inline = fields.len() <= 3
                && fields.iter().all(|f| matches!(f, TableField::Sequential(e) if is_simple_literal(e)));

            if is_short_inline {
                let items: Vec<String> = fields.iter().map(|f| {
                    match f {
                        TableField::Sequential(val) => emit_expr(val, depth),
                        _ => unreachable!(),
                    }
                }).collect();
                return format!("{{{}}}", items.join(", "));
            }

            let mut out = "{\n".to_string();

            for (i, field) in fields.iter().enumerate() {
                let is_sequential = matches!(field, TableField::Sequential(_));

                indent(&mut out, depth + 1);
                match field {
                    TableField::Sequential(val) => {
                        out.push_str(&emit_expr(val, depth + 1));
                    }
                    TableField::Named(name, val) => {
                        if is_valid_identifier(name) {
                            out.push_str(&format!("{} = {}", name, emit_expr(val, depth + 1)));
                        } else {
                            let s = emit_string(name);
                            let val_s = emit_expr(val, depth + 1);
                            // Phase B0.109: avoid `[[[` ambiguity.
                            if s.starts_with('[') {
                                out.push_str(&format!("[ {}] = {}", s, val_s));
                            } else {
                                out.push_str(&format!("[{}] = {}", s, val_s));
                            }
                        }
                    }
                    TableField::Indexed(key, val) => {
                        // Phase C10M: flatten `["Ident"] = val` → `Ident = val`
                        // when the key is a string literal that forms a valid
                        // Luau identifier. Matches the Named table-field form.
                        let ident_key = if let Expr::String(s) = key {
                            if is_valid_identifier(s) { Some(s.clone()) } else { None }
                        } else { None };
                        let val_s = emit_expr(val, depth + 1);
                        if let Some(s) = ident_key {
                            out.push_str(&format!("{} = {}", s, val_s));
                        } else {
                            let key_s = emit_expr(key, depth + 1);
                            // Phase B0.109: avoid `[[[` ambiguity.
                            if key_s.starts_with('[') {
                                out.push_str(&format!("[ {}] = {}", key_s, val_s));
                            } else {
                                out.push_str(&format!("[{}] = {}", key_s, val_s));
                            }
                        }
                    }
                }
                // Use a semicolon at transitions between sequential (array) and
                // keyed (hash) sections; commas within a section.  This makes
                // the output unambiguous for the Luau parser.
                if i + 1 < fields.len() {
                    let next_is_sequential =
                        matches!(fields[i + 1], TableField::Sequential(_));
                    if is_sequential != next_is_sequential {
                        out.push(';');
                    } else {
                        out.push(',');
                    }
                }
                out.push('\n');
            }
            indent(&mut out, depth);
            out.push('}');
            out
        }

        Expr::Vector(x, y, z) => {
            // Emit common Roblox shorthands when applicable
            if *x == 0.0 && *y == 0.0 && *z == 0.0 {
                "Vector3.zero".to_string()
            } else if *x == 1.0 && *y == 1.0 && *z == 1.0 {
                "Vector3.one".to_string()
            } else if *x == 1.0 && *y == 0.0 && *z == 0.0 {
                "Vector3.xAxis".to_string()
            } else if *x == 0.0 && *y == 1.0 && *z == 0.0 {
                "Vector3.yAxis".to_string()
            } else if *x == 0.0 && *y == 0.0 && *z == 1.0 {
                "Vector3.zAxis".to_string()
            } else {
                format!(
                    "Vector3.new({}, {}, {})",
                    emit_f32(*x),
                    emit_f32(*y),
                    emit_f32(*z)
                )
            }
        }

        // Phase B0.52P10: ternary expression — pick the cleanest safe form.
        //
        // `cond and a or b` is the classical Lua ternary pattern.  It only
        // produces `a` when BOTH `cond` is truthy AND `a` is truthy; if `a`
        // evaluates to `false` or `nil` the expression silently returns `b`
        // instead, which is wrong.  We therefore only emit the `and/or`
        // form when `a` is provably truthy (known-safe literal shapes).
        // Otherwise we fall back to Luau's native if-expression, which
        // has correct ternary semantics regardless of operand values.
        //
        // The emitted expression is wrapped in parentheses when embedded in
        // a larger expression where the precedence/associativity would
        // otherwise parse ambiguously — but at the top level we prefer the
        // minimal flat form.  We delegate the parenthesisation decision to
        // `emit_prefix_expr` via the existing needs_parens rules (Ternary
        // is added to that list below).
        Expr::Ternary { cond, then_expr, else_expr } => {
            // Phase B0.107: full_moon 2.1.1 (our parse validator) doesn't
            // support Luau if-expressions (`if c then a else b` as expr).
            // Always use `cond and a or b` form.  When then_expr is provably
            // truthy this is semantically correct.  When not (e.g., Name,
            // Call results), `cond and a or b` can differ from the true
            // ternary when `a` evaluates to nil/false at runtime — but this
            // is the best we can do without if-expression support in the
            // validator.  Return-statement ternaries get special expansion
            // in the Stat::Return handler instead.
            let cond_s = emit_expr_with_parens(cond, Some(BinOp::And), true, depth);
            let a_s = emit_expr_with_parens(then_expr, Some(BinOp::And), false, depth);
            let ab_s = format!("{} and {}", cond_s, a_s);
            let b_s = emit_expr_with_parens(else_expr, Some(BinOp::Or), false, depth);
            format!("{} or {}", ab_s, b_s)
        }
    }
}

/// Phase B0.52P10: is this expression provably truthy at runtime?
///
/// Used by the ternary emitter to decide between the compact
/// `cond and a or b` form (requires `a` to be truthy) and the safe
/// `if cond then a else b` form.
///
/// In Lua/Luau semantics, ONLY `false` and `nil` are falsy.  Everything
/// else — including 0, "", NaN — is truthy.  We return `true` for shapes
/// that CANNOT be `false` or `nil`:
///  - `true` literal
///  - numeric literals (incl. 0, NaN — all truthy in Lua)
///  - string literals (incl. "" — truthy in Lua)
///  - table literals (always truthy)
///  - function literals (always truthy)
///  - vector literals (always truthy)
///
/// Any expression whose value depends on runtime state (Name, Call, Index,
/// BinOp, UnOp, etc.) is NOT provably truthy and forces the safe form.
fn is_provably_truthy(e: &Expr) -> bool {
    matches!(
        e,
        Expr::Bool(true)
            | Expr::Number(_)
            | Expr::String(_)
            | Expr::Table { .. }
            | Expr::Function { .. }
            | Expr::Vector(..)
    )
}

fn emit_number(n: f64) -> String {
    if n.is_nan() {
        return "0/0".to_string();
    }
    if n.is_infinite() {
        return if n > 0.0 {
            "math.huge".to_string()
        } else {
            "-math.huge".to_string()
        };
    }
    // Negative zero must stay negative so `-0 == 0` semantics are preserved
    // in contexts where the sign matters (e.g. `1 / -0` == `-inf`).
    if n == 0.0 && n.is_sign_negative() {
        return "-0".to_string();
    }
    // Use integer form only when the value round-trips exactly and fits i64.
    // Values beyond 2^53 cannot be represented exactly as f64 integers, so
    // we fall through to float formatting for those.
    let as_i64 = n as i64;
    if n == as_i64 as f64 && n.abs() <= 9_007_199_254_740_992.0 {
        // Heuristic: positive integers that look like bitmasks render as hex
        // for readability — power-of-two flags, low/high anchored masks, and
        // sparse bit patterns.  Conservative: ordinary numbers stay decimal.
        if as_i64 > 8 && as_i64 <= u32::MAX as i64 {
            let u = as_i64 as u32;
            if looks_like_bitmask(u) {
                return format!("0x{:X}", u);
            }
        }
        return format!("{}", as_i64);
    }
    // Use Rust's Debug format which guarantees round-trip fidelity (prints
    // enough decimal digits so that parsing the string back yields the exact
    // same f64 value).  This avoids silent precision loss from Display.
    let s = format!("{:?}", n);
    // Rust Debug may emit e.g. "1.5e-10" which Luau accepts, or "0.3" —
    // both are fine.  However it always includes a decimal point or 'e',
    // so there is no risk of confusion with an integer literal.
    s
}

/// Heuristic: does this u32 look like a bitmask?  Used to decide between
/// decimal and hex literal formatting.
///
/// Conservative: ordinary numbers like 10, 100, 1000, 12345 must stay
/// decimal.  Small values < 16 ALWAYS stay decimal regardless of shape.
/// We only flag values that have an obvious bitmask shape:
///   - powers of two >= 16
///   - low-anchored full masks (0xFF, 0xFFFF, 0xFFFFFFFF)
///   - high-anchored masks (0xFF00, 0xFF000000, etc.)
///   - sparse bit patterns (<= 4 bits set) for VALUES >= 256 only —
///     small sparse values like 10 = 0b1010 are usually counts, not flags.
///
/// Defense-in-depth: every branch below requires `u >= 16`.  Even if a
/// future caller forgets the outer guard, small values are pinned to
/// decimal output.  The `>= 16` floor also rules out 12 (= 0xC, would
/// otherwise hit the high-anchored-mask branch), 14 (= 0xE, high-anchor),
/// and 15 (= 0xF, low-anchor) — all should be decimal per spec.
fn looks_like_bitmask(u: u32) -> bool {
    // Small values are NEVER hex — counts/indices/sizes dominate this range.
    // This is a hard floor applied to every branch below.  16 = 0x10 is the
    // smallest value worth rendering as hex (first two-hex-digit literal).
    if u < 16 {
        return false;
    }
    // Power of two — classic single-flag mask.  Smallest power-of-two we
    // accept is 16 (u >= 16 above rules out 1, 2, 4, 8).
    if u.is_power_of_two() {
        return true;
    }
    // Low-anchored full mask: 0x...FF style — checked_add prevents overflow.
    // u >= 16 above rules out 3, 7, 15 (which are also `(2^k) - 1` shapes).
    if let Some(plus_one) = u.checked_add(1) {
        if plus_one.is_power_of_two() {
            return true;
        }
    } else {
        // u == u32::MAX (== 0xFFFFFFFF) — definitely a mask.
        return true;
    }
    // High-anchored mask: 0xFF00, 0xFFFF0000, 0xFF000000 etc.  The trailing
    // zeros can be cleared and the result is still a low-anchored full mask.
    // u >= 16 rules out 12, 14 (which otherwise hit this branch).
    let tz = u.trailing_zeros();
    if tz > 0 && tz < 32 {
        let shifted = u >> tz;
        if let Some(plus_one) = shifted.checked_add(1) {
            if plus_one.is_power_of_two() {
                return true;
            }
        }
    }
    // Sparse bit pattern — at most 4 bits set, likely a flag combination.
    // Restricted to values >= 256 so that small "ordinary" numbers
    // (10, 12, 100, 1000) stay decimal — they happen to have few bits set
    // but are clearly not bitmasks in practice.
    if u >= 256 && u.count_ones() <= 4 {
        return true;
    }
    false
}

/// Format an f32 value for use inside Vector3.new().
/// Mirrors emit_number logic but operates on f32 precision:
///  - NaN  -> 0/0
///  - Inf  -> math.huge / -math.huge
///  - Exact integers (within f32 range) -> integer form (e.g. 3, -10)
///  - Otherwise -> shortest round-trip representation via Debug format
fn emit_f32(n: f32) -> String {
    if n.is_nan() {
        return "0/0".to_string();
    }
    if n.is_infinite() {
        return if n > 0.0 {
            "math.huge".to_string()
        } else {
            "-math.huge".to_string()
        };
    }
    if n == 0.0 && n.is_sign_negative() {
        return "-0".to_string();
    }
    // Use integer form when the value is an exact integer within f32 range.
    // f32 can represent integers exactly up to 2^24 = 16777216.
    let as_i32 = n as i32;
    if n == as_i32 as f32 && n.abs() <= 16_777_216.0 {
        return format!("{}", as_i32);
    }
    // Use Debug format for round-trip fidelity (e.g. "0.3", "1.5e-10").
    format!("{:?}", n)
}

fn emit_string(s: &str) -> String {
    // Try long-string form for multi-line / heavily-escaped content.
    if let Some(long) = try_emit_long_string(s) {
        return long;
    }
    let mut escaped = String::with_capacity(s.len() + 2);
    escaped.push('"');
    for b in s.bytes() {
        match b {
            b'\\' => escaped.push_str("\\\\"),
            b'"'  => escaped.push_str("\\\""),
            b'\n' => escaped.push_str("\\n"),
            b'\r' => escaped.push_str("\\r"),
            b'\t' => escaped.push_str("\\t"),
            0     => escaped.push_str("\\000"),
            // Remaining control characters and non-ASCII: use zero-padded
            // 3-digit decimal escape so a following digit character is never
            // absorbed into the escape sequence (e.g. "\0055" vs "\55").
            b if b < 0x20 || b >= 0x80 => {
                escaped.push_str(&format!("\\{:03}", b));
            }
            b => escaped.push(b as char),
        }
    }
    escaped.push('"');
    escaped
}

/// Try to render `s` as a Luau long-string literal `[[...]]` (or `[=[...]=]`
/// etc., picking the shortest level that doesn't appear inside).  Returns
/// `None` if long-string form is unsuitable for this content.
fn try_emit_long_string(s: &str) -> Option<String> {
    // Skip if leading newline — Luau long-string syntax eats a leading `\n`.
    if s.starts_with('\n') {
        return None;
    }
    // Long-strings cannot represent NUL or arbitrary control characters
    // verbatim (they would emit literally and produce invalid source / lossy
    // output).  Allow only printable ASCII plus \n \r \t.
    for b in s.bytes() {
        let ok = matches!(b, b'\n' | b'\r' | b'\t') || (0x20..=0x7E).contains(&b);
        if !ok {
            return None;
        }
    }
    // Decide whether long-string form is *worth* it.  Trigger if the string
    // contains an embedded literal newline, or if a quoted form would need
    // more than two backslash escapes.
    let has_newline = s.contains('\n');
    let escape_count = s.bytes().filter(|&b| matches!(b, b'\\' | b'"')).count();
    if !has_newline && escape_count <= 2 {
        return None;
    }
    // Pick the shortest level (number of `=` signs) such that neither
    // `]<eq>]` nor a closing-bracket-of-shorter-level appears inside.
    // We start at 0 and bump until we find a free level.
    for level in 0..=8 {
        let eq: String = "=".repeat(level);
        let close = format!("]{}]", eq);
        if !s.contains(&close) {
            // Insert a leading newline only if user content begins with `]`,
            // which would otherwise be ambiguous; not strictly required.
            return Some(format!("[{}[{}]{}]", eq, s, eq));
        }
    }
    // Couldn't find a free delimiter level — fall back to quoted form.
    None
}

/// Returns true if `name` is a valid Luau identifier that can be used bare
/// in dot-access or as a plain table key (starts with letter/underscore,
/// contains only alphanumeric/underscore, and is not a reserved word).
fn is_valid_identifier(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return false;
    }
    !matches!(
        name,
        "and" | "break" | "continue" | "do" | "else" | "elseif" | "end"
            | "false" | "for" | "function" | "if" | "in" | "local" | "nil"
            | "not" | "or" | "repeat" | "return" | "then" | "true" | "until"
            | "while"
        // NOTE: `type` and `export` are CONTEXTUAL keywords in Luau, not
        // reserved words — `local type = 5` and `t.export` are both legal.
        // Listing them here turned the stdlib global `type` into the string
        // literal `("type")`, which is not callable.
    )
}

/// Emit an expression that appears in "prefix" position (the object of a
/// field access, index, call, or method call).  In Luau only Names, field
/// accesses, index accesses, and calls are valid prefix expressions; anything
/// else (function literal, table literal, binop, unop, number, string, etc.)
/// must be wrapped in parentheses to avoid a syntax error.
fn emit_prefix_expr(expr: &Expr, depth: usize) -> String {
    let needs_parens = match expr {
        Expr::Function { .. }
            | Expr::Table { .. }
            | Expr::Number(_)
            | Expr::String(_)
            | Expr::Bool(_)
            | Expr::Nil
            | Expr::BinOp { .. }
            | Expr::UnOp { .. }
            | Expr::Varargs
            | Expr::Vector(..)
            | Expr::Ternary { .. } => true,
        // Non-identifier names are emitted as quoted strings by emit_expr,
        // so they need parens in prefix position (e.g., ("string"):Method()).
        Expr::Name(n) if !is_valid_identifier(n) => true,
        _ => false,
    };
    if needs_parens {
        format!("({})", emit_expr(expr, depth))
    } else {
        emit_expr(expr, depth)
    }
}

fn emit_expr_with_parens(expr: &Expr, parent_op: Option<BinOp>, is_left: bool, depth: usize) -> String {
    let s = emit_expr(expr, depth);
    if let Expr::BinOp { op, .. } = expr {
        if let Some(parent) = parent_op {
            let child_prec = op.precedence();
            let parent_prec = parent.precedence();
            let parent_right_assoc = matches!(parent, BinOp::Pow | BinOp::Concat);
            let needs_parens = if child_prec < parent_prec {
                // Strictly lower precedence — always parenthesize.
                true
            } else if child_prec == parent_prec {
                // Equal precedence — depends on associativity of the parent.
                // RIGHT-associative parent (^, ..): wrap LEFT operand only.
                //   `a^b^c` = `a^(b^c)`, so `(a^b)^c` needs parens on left;
                //   `a^(b^c)` does NOT — emit as `a^b^c` flat.
                // LEFT-associative parent (everything else): wrap RIGHT only.
                //   `a-b-c` = `(a-b)-c`, so `a-(b-c)` needs parens on right;
                //   `(a-b)-c` does NOT — emit as `a-b-c` flat.
                if parent_right_assoc {
                    is_left
                } else {
                    !is_left
                }
            } else {
                // Strictly higher precedence — never need parens.
                false
            };
            if needs_parens {
                return format!("({})", s);
            }
        }
    }
    // Phase B0.107: Ternary emits as `cond and X or Y`, effective
    // precedence = Or.  When nested inside And (higher prec), wrap.
    if matches!(expr, Expr::Ternary { .. }) {
        if let Some(parent) = parent_op {
            if BinOp::Or.precedence() < parent.precedence() {
                return format!("({})", s);
            }
        }
    }
    s
}

/// Check if an expression is a simple literal (for inline table formatting)
fn is_simple_literal(expr: &Expr) -> bool {
    matches!(expr, Expr::Nil | Expr::Bool(_) | Expr::Number(_) | Expr::String(_) | Expr::Name(_))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binop(left: Expr, op: BinOp, right: Expr) -> Expr {
        Expr::BinOp {
            left: Box::new(left),
            op,
            right: Box::new(right),
        }
    }
    fn name(s: &str) -> Expr {
        Expr::Name(s.to_string())
    }

    // ---------- Hex literal output ----------

    #[test]
    fn hex_emits_for_power_of_two_above_threshold() {
        // 16 is power-of-two, >= 16 → hex
        assert_eq!(emit_number(16.0), "0x10");
        assert_eq!(emit_number(256.0), "0x100");
        assert_eq!(emit_number(65536.0), "0x10000");
        // B0.44C spec examples
        assert_eq!(emit_number(1024.0), "0x400");
        assert_eq!(emit_number(4096.0), "0x1000");
    }

    #[test]
    fn hex_emits_for_low_anchored_full_mask() {
        // 0xFF, 0xFFFF, 0xFFFFFFFF — classic masks
        assert_eq!(emit_number(255.0), "0xFF");
        assert_eq!(emit_number(65535.0), "0xFFFF");
        assert_eq!(emit_number(4294967295.0), "0xFFFFFFFF");
    }

    #[test]
    fn hex_emits_for_high_anchored_mask() {
        // 0xFF00, 0xFFFF0000
        assert_eq!(emit_number(0xFF00 as f64), "0xFF00");
        assert_eq!(emit_number(0xFFFF0000u32 as f64), "0xFFFF0000");
        assert_eq!(emit_number(0xFF000000u32 as f64), "0xFF000000");
    }

    #[test]
    fn hex_emits_for_sparse_bit_pattern() {
        // 0x101 = 257 has 2 bits set, >= 256 → hex
        assert_eq!(emit_number(257.0), "0x101");
        // 0x808 = 2056 has 2 bits set, >= 256 → hex
        assert_eq!(emit_number(2056.0), "0x808");
        // Below the >= 256 threshold, sparse small values stay decimal:
        // 0x88 = 136 has 2 bits set BUT < 256 → decimal (could be a count)
        assert_eq!(emit_number(136.0), "136");
        // 10 (existing lifter test value) — sparse but small, stays decimal
        assert_eq!(emit_number(10.0), "10");
    }

    #[test]
    fn hex_does_not_emit_for_ordinary_integers() {
        // Ordinary numbers stay decimal
        assert_eq!(emit_number(100.0), "100");
        assert_eq!(emit_number(1000.0), "1000");
        assert_eq!(emit_number(12345.0), "12345");
        assert_eq!(emit_number(42.0), "42");
        // Threshold: <= 8 stays decimal even if power-of-two
        assert_eq!(emit_number(8.0), "8");
        assert_eq!(emit_number(4.0), "4");
        assert_eq!(emit_number(1.0), "1");
        assert_eq!(emit_number(0.0), "0");
    }

    // B0.44C — pin down small-value-stays-decimal for every value 0..=8.
    // Previously corpus showed values 0-8 slipping through as 0x00-0x08 via
    // sparse/power-of-two/low-mask branches.  The `u <= 8 => false` floor
    // inside looks_like_bitmask plus the outer caller guard guarantee that
    // no branch of the heuristic leaks through.
    #[test]
    fn hex_never_fires_for_zero() {
        assert_eq!(emit_number(0.0), "0");
        assert_ne!(emit_number(0.0), "0x00");
        assert_ne!(emit_number(0.0), "0x0");
    }

    #[test]
    fn hex_never_fires_for_small_powers_of_two() {
        // 1, 2, 4, 8 are powers of two — must stay decimal despite shape.
        assert_eq!(emit_number(1.0), "1");
        assert_eq!(emit_number(2.0), "2");
        assert_eq!(emit_number(4.0), "4");
        assert_eq!(emit_number(8.0), "8");
        for v in [1.0_f64, 2.0, 4.0, 8.0] {
            assert!(
                !emit_number(v).starts_with("0x"),
                "value {v} incorrectly rendered as hex: {}",
                emit_number(v)
            );
        }
    }

    #[test]
    fn hex_never_fires_for_small_low_anchored_masks() {
        // 1 (0b1), 3 (0b11), 7 (0b111) are `(2^k)-1` shapes that would
        // otherwise hit the low-anchored-mask branch.  All stay decimal.
        assert_eq!(emit_number(3.0), "3");
        assert_eq!(emit_number(5.0), "5");
        assert_eq!(emit_number(6.0), "6");
        assert_eq!(emit_number(7.0), "7");
        for v in [3.0_f64, 5.0, 6.0, 7.0] {
            assert!(
                !emit_number(v).starts_with("0x"),
                "value {v} incorrectly rendered as hex: {}",
                emit_number(v)
            );
        }
    }

    #[test]
    fn hex_never_fires_for_any_value_0_to_8_inclusive() {
        // Loop over every value 0..=8 to pin down the full range.
        for i in 0_i64..=8 {
            let s = emit_number(i as f64);
            assert_eq!(s, format!("{i}"),
                "value {i} rendered wrong (got {s:?}, expected decimal)");
            assert!(!s.starts_with("0x"),
                "value {i} leaked as hex: {s:?}");
        }
    }

    #[test]
    fn looks_like_bitmask_rejects_small_values_directly() {
        // Direct unit-test of the heuristic — every value 0..=8 must
        // return false from every branch.  Defense-in-depth check so
        // future refactors don't silently reintroduce the over-fire.
        for u in 0_u32..=8 {
            assert!(!looks_like_bitmask(u),
                "looks_like_bitmask({u}) returned true — must be false for small values");
        }
        // But 16 (first legal power-of-two) should still fire:
        assert!(looks_like_bitmask(16));
        // And 9..=15 should stay decimal too (too small for readability win).
        for u in 9_u32..=15 {
            assert!(!looks_like_bitmask(u),
                "looks_like_bitmask({u}) fires — values 9..=15 should stay decimal");
        }
    }

    #[test]
    fn hex_does_not_emit_for_negatives_or_floats() {
        // Negatives stay decimal
        assert_eq!(emit_number(-256.0), "-256");
        assert_eq!(emit_number(-255.0), "-255");
        // Additional negative values per B0.44C spec
        assert_eq!(emit_number(-1.0), "-1");
        assert_eq!(emit_number(-100.0), "-100");
        assert_eq!(emit_number(-1000.0), "-1000");
        // Negative bitmask-shaped values — must still stay decimal
        assert_eq!(emit_number(-255.0), "-255");
        assert_eq!(emit_number(-4096.0), "-4096");
        for v in [-1.0_f64, -100.0, -1000.0, -255.0, -4096.0] {
            assert!(
                !emit_number(v).starts_with("0x") && !emit_number(v).starts_with("-0x"),
                "negative value {v} incorrectly rendered as hex: {}",
                emit_number(v)
            );
        }
        // Floats stay decimal — must contain '.' or 'e'
        let s = emit_number(1.5);
        assert!(s.contains('.') || s.contains('e'));
        let s2 = emit_number(0.5);
        assert!(s2.contains('.') || s2.contains('e'));
    }

    #[test]
    fn hex_handles_u32_max_without_overflow() {
        // u32::MAX must not panic on (u + 1) overflow
        assert_eq!(emit_number(u32::MAX as f64), "0xFFFFFFFF");
        // Above u32 — fall back to decimal (not a u32 mask)
        let big = (u32::MAX as f64) + 1.0;
        assert_eq!(emit_number(big), "4294967296");
    }

    // ---------- Long-string literals ----------

    #[test]
    fn long_string_used_for_embedded_newline() {
        let out = emit_string("line1\nline2");
        assert!(out.starts_with("[["), "expected long-string, got: {out}");
        assert!(out.ends_with("]]"));
        assert!(out.contains("line1\nline2"));
    }

    #[test]
    fn long_string_used_for_many_escapes() {
        // Three quotes triggers long-string
        let out = emit_string(r#"a"b"c"d"#);
        assert!(out.starts_with("[["), "expected long-string, got: {out}");
        assert!(out.contains(r#"a"b"c"d"#));
    }

    #[test]
    fn long_string_picks_higher_level_when_brackets_inside() {
        // String contains `]]` → must use `[=[ ... ]=]`
        let out = emit_string("foo\n]]bar");
        assert!(out.starts_with("[=["), "expected level-1, got: {out}");
        assert!(out.ends_with("]=]"));
    }

    #[test]
    fn long_string_skipped_for_leading_newline() {
        // Luau eats leading newline of long-string → fall back to quoted form
        let out = emit_string("\nhello");
        assert!(out.starts_with('"'), "expected quoted, got: {out}");
        assert!(out.contains("\\n"));
    }

    #[test]
    fn long_string_skipped_for_non_ascii_or_control() {
        // NUL → must stay quoted
        let out = emit_string("foo\nbar\0baz");
        assert!(out.starts_with('"'), "expected quoted, got: {out}");
        // Non-ASCII byte → must stay quoted
        let out2 = emit_string("foo\nbar\u{00FF}");
        assert!(out2.starts_with('"'), "expected quoted, got: {out2}");
        // Other control char → must stay quoted
        let out3 = emit_string("foo\nbar\x01baz");
        assert!(out3.starts_with('"'), "expected quoted, got: {out3}");
    }

    #[test]
    fn long_string_skipped_for_short_clean_strings() {
        // Plain short string stays quoted
        assert_eq!(emit_string("hello"), "\"hello\"");
        // One backslash, no newline — stays quoted
        let out = emit_string("a\\b");
        assert!(out.starts_with('"'));
    }

    // ---------- Paren reduction ----------

    #[test]
    fn paren_pow_right_assoc_flat() {
        // a^b^c should emit flat (not a^(b^c))
        let e = binop(name("a"), BinOp::Pow, binop(name("b"), BinOp::Pow, name("c")));
        assert_eq!(emit_expr(&e, 0), "a ^ b ^ c");
    }

    #[test]
    fn paren_pow_left_grouping_keeps_parens() {
        // (a^b)^c needs parens on left
        let e = binop(binop(name("a"), BinOp::Pow, name("b")), BinOp::Pow, name("c"));
        assert_eq!(emit_expr(&e, 0), "(a ^ b) ^ c");
    }

    #[test]
    fn paren_concat_right_assoc_flat() {
        // a..b..c should emit flat
        let e = binop(name("a"), BinOp::Concat, binop(name("b"), BinOp::Concat, name("c")));
        assert_eq!(emit_expr(&e, 0), "a .. b .. c");
    }

    #[test]
    fn paren_sub_left_assoc_keeps_right_parens() {
        // a-(b-c) MUST keep parens — left-assoc, equal-prec on right
        let e = binop(name("a"), BinOp::Sub, binop(name("b"), BinOp::Sub, name("c")));
        assert_eq!(emit_expr(&e, 0), "a - (b - c)");
    }

    #[test]
    fn paren_sub_left_assoc_left_flat() {
        // (a-b)-c should be flat
        let e = binop(binop(name("a"), BinOp::Sub, name("b")), BinOp::Sub, name("c"));
        assert_eq!(emit_expr(&e, 0), "a - b - c");
    }

    #[test]
    fn paren_lower_prec_child_keeps_parens() {
        // (a + b) * c — `+` has lower prec than `*`
        let e = binop(binop(name("a"), BinOp::Add, name("b")), BinOp::Mul, name("c"));
        assert_eq!(emit_expr(&e, 0), "(a + b) * c");
        // a * (b + c)
        let e2 = binop(name("a"), BinOp::Mul, binop(name("b"), BinOp::Add, name("c")));
        assert_eq!(emit_expr(&e2, 0), "a * (b + c)");
    }

    #[test]
    fn paren_higher_prec_child_no_parens() {
        // a + b * c — `*` has higher prec
        let e = binop(name("a"), BinOp::Add, binop(name("b"), BinOp::Mul, name("c")));
        assert_eq!(emit_expr(&e, 0), "a + b * c");
        // a * b + c
        let e2 = binop(binop(name("a"), BinOp::Mul, name("b")), BinOp::Add, name("c"));
        assert_eq!(emit_expr(&e2, 0), "a * b + c");
    }

    #[test]
    fn paren_add_left_assoc_flat() {
        // a+b+c should be flat (left-assoc, equal-prec on left = no parens)
        let e = binop(binop(name("a"), BinOp::Add, name("b")), BinOp::Add, name("c"));
        assert_eq!(emit_expr(&e, 0), "a + b + c");
    }

    #[test]
    fn paren_add_left_assoc_right_grouping_keeps_parens() {
        // a + (b + c) — left-assoc, equal-prec on right needs parens
        let e = binop(name("a"), BinOp::Add, binop(name("b"), BinOp::Add, name("c")));
        assert_eq!(emit_expr(&e, 0), "a + (b + c)");
    }

    #[test]
    fn paren_concat_left_grouping_keeps_parens() {
        // (a..b)..c — right-assoc parent, equal-prec on left needs parens
        let e = binop(binop(name("a"), BinOp::Concat, name("b")), BinOp::Concat, name("c"));
        assert_eq!(emit_expr(&e, 0), "(a .. b) .. c");
    }

    // ---------- Phase B0.52P10: LocalFunction / MethodFunction / Ternary ----------

    fn func(params: Vec<&str>, is_vararg: bool, body: Vec<Stat>) -> Expr {
        Expr::Function {
            params: params.iter().map(|s| s.to_string()).collect(),
            is_vararg,
            body,
        }
    }

    fn emit_stmt_str(s: &Stat) -> String {
        let mut out = String::new();
        emit_stat(&mut out, s, 0);
        out
    }

    #[test]
    fn local_function_emits_shorthand() {
        // local function foo() end
        let s = Stat::LocalFunction {
            name: "foo".to_string(),
            func: func(vec![], false, vec![]),
        };
        let out = emit_stmt_str(&s);
        assert_eq!(out, "local function foo()\nend\n");
    }

    #[test]
    fn local_function_with_params_and_body() {
        // local function add(a, b) return a + b end
        let s = Stat::LocalFunction {
            name: "add".to_string(),
            func: func(
                vec!["a", "b"],
                false,
                vec![Stat::Return {
                    values: vec![binop(name("a"), BinOp::Add, name("b"))],
                }],
            ),
        };
        let out = emit_stmt_str(&s);
        assert_eq!(out, "local function add(a, b)\n    return a + b\nend\n");
    }

    #[test]
    fn local_function_with_vararg() {
        // local function pack(...) end
        let s = Stat::LocalFunction {
            name: "pack".to_string(),
            func: func(vec![], true, vec![]),
        };
        let out = emit_stmt_str(&s);
        assert_eq!(out, "local function pack(...)\nend\n");
        // mixed named + vararg
        let s2 = Stat::LocalFunction {
            name: "log".to_string(),
            func: func(vec!["level"], true, vec![]),
        };
        let out2 = emit_stmt_str(&s2);
        assert_eq!(out2, "local function log(level, ...)\nend\n");
    }

    #[test]
    fn local_function_defensive_fallback_on_non_function_expr() {
        // If `func` field is somehow not Expr::Function, fall back to assign
        // form rather than panicking.
        let s = Stat::LocalFunction {
            name: "x".to_string(),
            func: Expr::Number(42.0),
        };
        let out = emit_stmt_str(&s);
        assert_eq!(out, "local x = 42\n");
    }

    #[test]
    fn method_function_emits_colon_shorthand() {
        // function Obj:method(a) end  — `self` is implicit first param
        let s = Stat::MethodFunction {
            receiver: name("Obj"),
            method: "method".to_string(),
            is_method: true,
            func: func(vec!["self", "a"], false, vec![]),
        };
        let out = emit_stmt_str(&s);
        assert_eq!(out, "function Obj:method(a)\nend\n");
    }

    #[test]
    fn method_function_emits_dot_shorthand() {
        // function Obj.method(a) end  — no `self` param
        let s = Stat::MethodFunction {
            receiver: name("Obj"),
            method: "method".to_string(),
            is_method: false,
            func: func(vec!["a"], false, vec![]),
        };
        let out = emit_stmt_str(&s);
        assert_eq!(out, "function Obj.method(a)\nend\n");
    }

    #[test]
    fn method_function_with_field_receiver() {
        // function M.Class:init() end — receiver is a field expr
        let s = Stat::MethodFunction {
            receiver: Expr::Field {
                object: Box::new(name("M")),
                field: "Class".to_string(),
            },
            method: "init".to_string(),
            is_method: true,
            func: func(vec!["self"], false, vec![]),
        };
        let out = emit_stmt_str(&s);
        assert_eq!(out, "function M.Class:init()\nend\n");
    }

    #[test]
    fn method_function_with_vararg() {
        // function Obj:log(...) end
        let s = Stat::MethodFunction {
            receiver: name("Obj"),
            method: "log".to_string(),
            is_method: true,
            func: func(vec!["self"], true, vec![]),
        };
        let out = emit_stmt_str(&s);
        assert_eq!(out, "function Obj:log(...)\nend\n");
    }

    #[test]
    fn method_function_with_body() {
        // function Obj:greet(who) return who end
        let s = Stat::MethodFunction {
            receiver: name("Obj"),
            method: "greet".to_string(),
            is_method: true,
            func: func(
                vec!["self", "who"],
                false,
                vec![Stat::Return {
                    values: vec![name("who")],
                }],
            ),
        };
        let out = emit_stmt_str(&s);
        assert_eq!(out, "function Obj:greet(who)\n    return who\nend\n");
    }

    #[test]
    fn method_function_defensive_fallback_on_non_function_expr() {
        // If `func` is not Expr::Function, fall back to assign form
        let s = Stat::MethodFunction {
            receiver: name("Obj"),
            method: "x".to_string(),
            is_method: false,
            func: Expr::Number(5.0),
        };
        let out = emit_stmt_str(&s);
        assert_eq!(out, "Obj.x = 5\n");
    }

    #[test]
    fn ternary_uses_and_or_form_when_then_is_provably_truthy() {
        // cond and "yes" or "no"  — "yes" is a string literal (truthy)
        let e = Expr::Ternary {
            cond: Box::new(name("cond")),
            then_expr: Box::new(Expr::String("yes".to_string())),
            else_expr: Box::new(Expr::String("no".to_string())),
        };
        assert_eq!(emit_expr(&e, 0), r#"cond and "yes" or "no""#);
    }

    #[test]
    fn ternary_uses_and_or_form_for_table_then() {
        // cond and {1,2} or {} — table literal is always truthy
        let e = Expr::Ternary {
            cond: Box::new(name("cond")),
            then_expr: Box::new(Expr::Table {
                fields: vec![
                    TableField::Sequential(Expr::Number(1.0)),
                    TableField::Sequential(Expr::Number(2.0)),
                ],
            }),
            else_expr: Box::new(Expr::Table { fields: vec![] }),
        };
        let out = emit_expr(&e, 0);
        assert!(out.contains(" and "));
        assert!(out.contains(" or "));
        assert!(out.starts_with("cond"));
    }

    #[test]
    fn ternary_uses_and_or_for_non_truthy_then() {
        // Phase B0.107: always use and/or form since full_moon doesn't
        // support if-expressions.  x is Name (not provably truthy) but
        // we use and/or anyway — return-statement ternaries get expanded
        // to if-statement form in the Stat::Return handler.
        let e = Expr::Ternary {
            cond: Box::new(name("cond")),
            then_expr: Box::new(name("x")),
            else_expr: Box::new(name("y")),
        };
        assert_eq!(emit_expr(&e, 0), "cond and x or y");
    }

    #[test]
    fn ternary_uses_and_or_for_call_then() {
        // Phase B0.107: always and/or in expression context
        let e = Expr::Ternary {
            cond: Box::new(name("cond")),
            then_expr: Box::new(Expr::Call {
                func: Box::new(name("f")),
                args: vec![],
            }),
            else_expr: Box::new(Expr::Number(0.0)),
        };
        assert_eq!(emit_expr(&e, 0), "cond and f() or 0");
    }

    #[test]
    fn ternary_uses_and_or_for_bool_false_then() {
        // Phase B0.107: even Bool(false) uses and/or form
        let e = Expr::Ternary {
            cond: Box::new(name("cond")),
            then_expr: Box::new(Expr::Bool(false)),
            else_expr: Box::new(Expr::Bool(true)),
        };
        assert_eq!(emit_expr(&e, 0), "cond and false or true");
    }

    #[test]
    fn ternary_uses_and_or_for_number_then() {
        // In Lua 0 is truthy — numeric literal is always safe
        let e = Expr::Ternary {
            cond: Box::new(name("cond")),
            then_expr: Box::new(Expr::Number(0.0)),
            else_expr: Box::new(Expr::Number(1.0)),
        };
        assert_eq!(emit_expr(&e, 0), "cond and 0 or 1");
    }

    #[test]
    fn ternary_parenthesised_when_used_as_prefix() {
        // (cond and "a" or "b").field  — ternary in prefix position
        let t = Expr::Ternary {
            cond: Box::new(name("cond")),
            then_expr: Box::new(Expr::String("a".to_string())),
            else_expr: Box::new(Expr::String("b".to_string())),
        };
        let e = Expr::Field {
            object: Box::new(t),
            field: "x".to_string(),
        };
        let out = emit_expr(&e, 0);
        assert!(out.starts_with('('), "expected parens, got: {out}");
        assert!(out.contains(").x"));
    }

    // ---------- Backwards-compat: existing shapes unchanged ----------

    #[test]
    fn backcompat_local_assign_still_works() {
        // Old shape: local foo = function() end should still emit correctly.
        let s = Stat::Local {
            names: vec!["foo".to_string()],
            values: vec![func(vec![], false, vec![])],
        };
        let out = emit_stmt_str(&s);
        assert_eq!(out, "local foo = function()\nend\n");
    }

    #[test]
    fn backcompat_assign_to_method_slot_still_works() {
        // Obj.method = function(self) end — old shape without MethodFunction
        let s = Stat::Assign {
            targets: vec![Expr::Field {
                object: Box::new(name("Obj")),
                field: "method".to_string(),
            }],
            values: vec![func(vec!["self"], false, vec![])],
        };
        let out = emit_stmt_str(&s);
        assert_eq!(out, "Obj.method = function(self)\nend\n");
    }

    #[test]
    fn backcompat_short_circuit_stays_binop() {
        // Explicit a and b written via BinOp must still work — Ternary is
        // opt-in only.
        let e = binop(name("cond"), BinOp::And, name("a"));
        assert_eq!(emit_expr(&e, 0), "cond and a");
        let e2 = binop(binop(name("cond"), BinOp::And, name("a")), BinOp::Or, name("b"));
        assert_eq!(emit_expr(&e2, 0), "cond and a or b");
    }

    // ---------- Round-trip via Display-like path ----------

    #[test]
    fn roundtrip_local_function_compiles_shape() {
        // emit_block should handle LocalFunction inside a normal body.
        let stmts = vec![
            Stat::LocalFunction {
                name: "helper".to_string(),
                func: func(
                    vec!["x"],
                    false,
                    vec![Stat::Return {
                        values: vec![name("x")],
                    }],
                ),
            },
            Stat::Return {
                values: vec![Expr::Call {
                    func: Box::new(name("helper")),
                    args: vec![Expr::Number(1.0)],
                }],
            },
        ];
        let mut out = String::new();
        emit_block(&mut out, &stmts, 0);
        assert!(out.contains("local function helper(x)\n"));
        assert!(out.contains("    return x\n"));
        assert!(out.ends_with("return helper(1)\n"));
    }

    #[test]
    fn roundtrip_method_function_in_block() {
        // emit_block should handle MethodFunction alongside other statements.
        let stmts = vec![
            Stat::Local {
                names: vec!["Obj".to_string()],
                values: vec![Expr::Table { fields: vec![] }],
            },
            Stat::MethodFunction {
                receiver: name("Obj"),
                method: "init".to_string(),
                is_method: true,
                func: func(vec!["self"], false, vec![]),
            },
        ];
        let mut out = String::new();
        emit_block(&mut out, &stmts, 0);
        assert!(out.contains("local Obj = {}\n"));
        assert!(out.contains("function Obj:init()\n"));
        assert!(out.contains("end\n"));
    }

    #[test]
    fn roundtrip_ternary_inside_assignment() {
        // local x = cond and 1 or 2
        let stmts = vec![Stat::Local {
            names: vec!["x".to_string()],
            values: vec![Expr::Ternary {
                cond: Box::new(name("cond")),
                then_expr: Box::new(Expr::Number(1.0)),
                else_expr: Box::new(Expr::Number(2.0)),
            }],
        }];
        let mut out = String::new();
        emit_block(&mut out, &stmts, 0);
        assert_eq!(out, "local x = cond and 1 or 2\n");
    }

    #[test]
    fn return_ternary_non_truthy_expands_to_if_statement() {
        // Phase B0.107: return with non-truthy ternary expands to if-statement
        let stmts = vec![Stat::Return {
            values: vec![Expr::Ternary {
                cond: Box::new(name("cond")),
                then_expr: Box::new(name("x")),
                else_expr: Box::new(name("y")),
            }],
        }];
        let mut out = String::new();
        emit_block(&mut out, &stmts, 0);
        assert_eq!(out, "if cond then\n    return x\nelse\n    return y\nend\n");
    }

    #[test]
    fn return_ternary_truthy_stays_inline() {
        // return cond and "yes" or "no" — string is provably truthy
        let stmts = vec![Stat::Return {
            values: vec![Expr::Ternary {
                cond: Box::new(name("cond")),
                then_expr: Box::new(Expr::String("yes".to_string())),
                else_expr: Box::new(Expr::String("no".to_string())),
            }],
        }];
        let mut out = String::new();
        emit_block(&mut out, &stmts, 0);
        assert_eq!(out, "return cond and \"yes\" or \"no\"\n");
    }

    #[test]
    fn local_ternary_non_truthy_expands_to_if_block() {
        // Phase B0.108: local x = <ternary with non-truthy then> →
        // local x; if cond then x = a else x = b end
        let stmts = vec![Stat::Local {
            names: vec!["x".to_string()],
            values: vec![Expr::Ternary {
                cond: Box::new(name("cond")),
                then_expr: Box::new(name("a")),
                else_expr: Box::new(name("b")),
            }],
        }];
        let mut out = String::new();
        emit_block(&mut out, &stmts, 0);
        assert_eq!(out, "local x\nif cond then\n    x = a\nelse\n    x = b\nend\n");
    }

    #[test]
    fn local_ternary_truthy_stays_inline() {
        // Phase B0.108: local x = cond and "yes" or "no" — provably truthy
        let stmts = vec![Stat::Local {
            names: vec!["x".to_string()],
            values: vec![Expr::Ternary {
                cond: Box::new(name("cond")),
                then_expr: Box::new(Expr::String("yes".to_string())),
                else_expr: Box::new(Expr::String("no".to_string())),
            }],
        }];
        let mut out = String::new();
        emit_block(&mut out, &stmts, 0);
        assert_eq!(out, "local x = cond and \"yes\" or \"no\"\n");
    }

    #[test]
    fn assign_ternary_non_truthy_expands_to_if_block() {
        // Phase B0.108: x = <ternary with non-truthy then> →
        // if cond then x = a else x = b end
        let stmts = vec![Stat::Assign {
            targets: vec![name("x")],
            values: vec![Expr::Ternary {
                cond: Box::new(name("cond")),
                then_expr: Box::new(name("a")),
                else_expr: Box::new(name("b")),
            }],
        }];
        let mut out = String::new();
        emit_block(&mut out, &stmts, 0);
        assert_eq!(out, "if cond then\n    x = a\nelse\n    x = b\nend\n");
    }

    #[test]
    fn assign_ternary_truthy_stays_inline() {
        // Phase B0.108: x = cond and 42 or 0 — number is provably truthy
        let stmts = vec![Stat::Assign {
            targets: vec![name("x")],
            values: vec![Expr::Ternary {
                cond: Box::new(name("cond")),
                then_expr: Box::new(Expr::Number(42.0)),
                else_expr: Box::new(Expr::Number(0.0)),
            }],
        }];
        let mut out = String::new();
        emit_block(&mut out, &stmts, 0);
        assert_eq!(out, "x = cond and 42 or 0\n");
    }

    #[test]
    fn roundtrip_nested_ternary_uses_and_or() {
        // Phase B0.107: nested ternary always uses and/or form
        let inner = Expr::Ternary {
            cond: Box::new(name("c2")),
            then_expr: Box::new(name("a")),
            else_expr: Box::new(name("b")),
        };
        let outer = Expr::Ternary {
            cond: Box::new(name("c1")),
            then_expr: Box::new(inner),
            else_expr: Box::new(name("d")),
        };
        // Both levels use and/or; inner is parenthesized because Ternary
        // appears in parent-op context.
        let out = emit_expr(&outer, 0);
        assert_eq!(out, "c1 and (c2 and a or b) or d");
    }

    #[test]
    fn index_long_string_key_avoids_bracket_ambiguity() {
        // Phase B0.109: obj[[[str]]] → obj[ [[str]]] to avoid parse error
        let e = Expr::Index {
            object: Box::new(name("v0")),
            key: Box::new(Expr::String("foo\nbar".to_string())),
        };
        let out = emit_expr(&e, 0);
        // The string "foo\nbar" should render as long string [[foo\nbar]]
        // and be spaced from the opening [ to avoid [[[
        assert!(out.starts_with("v0[ "), "expected space after `[`, got: {out}");
        assert!(!out.contains("[[["), "must not contain [[[ ambiguity: {out}");
    }

    #[test]
    fn index_normal_key_no_extra_space() {
        // Normal index should NOT have extra space
        let e = Expr::Index {
            object: Box::new(name("t")),
            key: Box::new(Expr::Number(1.0)),
        };
        let out = emit_expr(&e, 0);
        assert_eq!(out, "t[1]");
    }

    // Phase B0.52P10: helper function tests.

    #[test]
    fn is_provably_truthy_accepts_known_truthy_literals() {
        assert!(is_provably_truthy(&Expr::Bool(true)));
        assert!(is_provably_truthy(&Expr::Number(0.0)));
        assert!(is_provably_truthy(&Expr::Number(1.0)));
        assert!(is_provably_truthy(&Expr::Number(-1.0)));
        assert!(is_provably_truthy(&Expr::String("".to_string())));
        assert!(is_provably_truthy(&Expr::String("x".to_string())));
        assert!(is_provably_truthy(&Expr::Table { fields: vec![] }));
        assert!(is_provably_truthy(&func(vec![], false, vec![])));
        assert!(is_provably_truthy(&Expr::Vector(0.0, 0.0, 0.0)));
    }

    #[test]
    fn is_provably_truthy_rejects_falsy_and_runtime_shapes() {
        assert!(!is_provably_truthy(&Expr::Bool(false)));
        assert!(!is_provably_truthy(&Expr::Nil));
        assert!(!is_provably_truthy(&name("x")));
        assert!(!is_provably_truthy(&Expr::Call {
            func: Box::new(name("f")),
            args: vec![],
        }));
        assert!(!is_provably_truthy(&Expr::Field {
            object: Box::new(name("t")),
            field: "k".to_string(),
        }));
        assert!(!is_provably_truthy(&binop(name("a"), BinOp::Add, name("b"))));
        assert!(!is_provably_truthy(&Expr::UnOp {
            op: UnOp::Negate,
            operand: Box::new(name("x")),
        }));
        // Varargs can be nil
        assert!(!is_provably_truthy(&Expr::Varargs));
    }

    // ---------- Phase B0.94b: trailing nil strip in local declarations ----------

    #[test]
    fn b094b_local_nil_strips_to_bare_local() {
        // local x = nil → local x
        let mut out = String::new();
        emit_stat(&mut out, &Stat::Local {
            names: vec!["x".into()],
            values: vec![Expr::Nil],
        }, 0);
        assert_eq!(out.trim(), "local x");
    }

    #[test]
    fn b094b_local_trailing_nils_stripped() {
        // local x, y = 1, nil → local x, y = 1
        let mut out = String::new();
        emit_stat(&mut out, &Stat::Local {
            names: vec!["x".into(), "y".into()],
            values: vec![Expr::Number(1.0), Expr::Nil],
        }, 0);
        assert_eq!(out.trim(), "local x, y = 1");
    }

    #[test]
    fn b094b_local_all_nils_stripped() {
        // local x, y = nil, nil → local x, y
        let mut out = String::new();
        emit_stat(&mut out, &Stat::Local {
            names: vec!["x".into(), "y".into()],
            values: vec![Expr::Nil, Expr::Nil],
        }, 0);
        assert_eq!(out.trim(), "local x, y");
    }

    #[test]
    fn b094b_local_non_trailing_nil_preserved() {
        // local x, y = nil, 1 → stays as local x, y = nil, 1
        let mut out = String::new();
        emit_stat(&mut out, &Stat::Local {
            names: vec!["x".into(), "y".into()],
            values: vec![Expr::Nil, Expr::Number(1.0)],
        }, 0);
        assert_eq!(out.trim(), "local x, y = nil, 1");
    }

    #[test]
    fn b094b_local_non_nil_preserved() {
        // local x = 42 → stays as local x = 42
        let mut out = String::new();
        emit_stat(&mut out, &Stat::Local {
            names: vec!["x".into()],
            values: vec![Expr::Number(42.0)],
        }, 0);
        assert_eq!(out.trim(), "local x = 42");
    }

    // ---------- Phase B0.98: compound assignment ----------

    #[test]
    fn b098_add_compound_assignment() {
        // x = x + 1 → x += 1
        let s = Stat::Assign {
            targets: vec![name("x")],
            values: vec![binop(name("x"), BinOp::Add, Expr::Number(1.0))],
        };
        assert_eq!(emit_stmt_str(&s), "x += 1\n");
    }

    #[test]
    fn b098_sub_compound_assignment() {
        // count = count - 1 → count -= 1
        let s = Stat::Assign {
            targets: vec![name("count")],
            values: vec![binop(name("count"), BinOp::Sub, Expr::Number(1.0))],
        };
        assert_eq!(emit_stmt_str(&s), "count -= 1\n");
    }

    #[test]
    fn b098_mul_compound_assignment() {
        // scale = scale * 2 → scale *= 2
        let s = Stat::Assign {
            targets: vec![name("scale")],
            values: vec![binop(name("scale"), BinOp::Mul, Expr::Number(2.0))],
        };
        assert_eq!(emit_stmt_str(&s), "scale *= 2\n");
    }

    #[test]
    fn b098_div_compound_assignment() {
        // x = x / y → x /= y
        let s = Stat::Assign {
            targets: vec![name("x")],
            values: vec![binop(name("x"), BinOp::Div, name("y"))],
        };
        assert_eq!(emit_stmt_str(&s), "x /= y\n");
    }

    #[test]
    fn b098_idiv_no_compound_assignment() {
        // x = x // 2 stays as-is (Luau doesn't support //=)
        let s = Stat::Assign {
            targets: vec![name("x")],
            values: vec![binop(name("x"), BinOp::IDiv, Expr::Number(2.0))],
        };
        assert_eq!(emit_stmt_str(&s), "x = x // 2\n");
    }

    #[test]
    fn b098_mod_compound_assignment() {
        // x = x % 10 → x %= 10
        let s = Stat::Assign {
            targets: vec![name("x")],
            values: vec![binop(name("x"), BinOp::Mod, Expr::Number(10.0))],
        };
        assert_eq!(emit_stmt_str(&s), "x %= 10\n");
    }

    #[test]
    fn b098_pow_compound_assignment() {
        // x = x ^ 2 → x ^= 2
        let s = Stat::Assign {
            targets: vec![name("x")],
            values: vec![binop(name("x"), BinOp::Pow, Expr::Number(2.0))],
        };
        assert_eq!(emit_stmt_str(&s), "x ^= 2\n");
    }

    #[test]
    fn b098_concat_compound_assignment() {
        // s = s .. "!" → s ..= "!"
        let s = Stat::Assign {
            targets: vec![name("s")],
            values: vec![binop(name("s"), BinOp::Concat, Expr::String("!".into()))],
        };
        assert_eq!(emit_stmt_str(&s), "s ..= \"!\"\n");
    }

    #[test]
    fn b098_field_compound_assignment() {
        // self.x = self.x + delta → self.x += delta
        let field = Expr::Field {
            object: Box::new(name("self")),
            field: "x".to_string(),
        };
        let s = Stat::Assign {
            targets: vec![field.clone()],
            values: vec![binop(field, BinOp::Add, name("delta"))],
        };
        assert_eq!(emit_stmt_str(&s), "self.x += delta\n");
    }

    #[test]
    fn b098_no_compound_when_lhs_differs() {
        // x = y + 1  (target x ≠ left y) — normal assignment
        let s = Stat::Assign {
            targets: vec![name("x")],
            values: vec![binop(name("y"), BinOp::Add, Expr::Number(1.0))],
        };
        assert_eq!(emit_stmt_str(&s), "x = y + 1\n");
    }

    #[test]
    fn b098_no_compound_for_comparison_ops() {
        // x = x == y — comparison, no compound form
        let s = Stat::Assign {
            targets: vec![name("x")],
            values: vec![binop(name("x"), BinOp::Eq, name("y"))],
        };
        assert_eq!(emit_stmt_str(&s), "x = x == y\n");
    }

    #[test]
    fn b098_no_compound_for_logical_ops() {
        // x = x and y — logical, no compound form
        let s = Stat::Assign {
            targets: vec![name("x")],
            values: vec![binop(name("x"), BinOp::And, name("y"))],
        };
        assert_eq!(emit_stmt_str(&s), "x = x and y\n");
    }

    #[test]
    fn b098_no_compound_for_multi_assign() {
        // x, y = x + 1, y + 1 — multi-assignment stays normal
        let s = Stat::Assign {
            targets: vec![name("x"), name("y")],
            values: vec![
                binop(name("x"), BinOp::Add, Expr::Number(1.0)),
                binop(name("y"), BinOp::Add, Expr::Number(1.0)),
            ],
        };
        assert_eq!(emit_stmt_str(&s), "x, y = x + 1, y + 1\n");
    }

    #[test]
    fn b098_no_compound_for_non_binop_value() {
        // x = f() — value is a call, not a binop
        let s = Stat::Assign {
            targets: vec![name("x")],
            values: vec![Expr::Call {
                func: Box::new(name("f")),
                args: vec![],
            }],
        };
        assert_eq!(emit_stmt_str(&s), "x = f()\n");
    }
}
