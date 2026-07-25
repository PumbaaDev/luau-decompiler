//! Post-processing AST passes that polish the emitted statements.
//!
//! Extracted from `lifter.rs` as part of Phase B0.52P6 (part 3/3).  These
//! functions run AFTER instruction lifting and region structuring to clean
//! up the raw output:
//!
//! * `collapse_elseif_chains`             — nested `else if` -> `elseif`.
//! * `collapse_short_circuit_assignments` — `if X then a = X else a = B end`
//!                                          -> `a = X or B` and friends.
//! * `try_collapse_if_to_short_circuit`   — one-shot collapse helper.
//! * `single_name_assign`                 — detects `Name = expr` singletons.
//! * `inline_single_use_temps`            — folds `local t = expr; f(t)` into
//!                                           `f(expr)` when `t` has exactly
//!                                           one reader.
//! * `is_inlinable_literal`               — trivial-constant gate for inlining.
//! * `stmt_writes_name_recursive`         — detects writes that disqualify a
//!                                           multi-use inline candidate.
//! * `inline_pure_literals`               — Phase B0.51B: fold pure literal
//!                                           assignments into ALL their later
//!                                           reads until the name is
//!                                           reassigned.
//!
//! Helpers that stay in `lifter/mod.rs` (and are imported via `super::`):
//! `collect_names_in_expr`, `count_name_reads_in_stmt`, `expr_contains_call`,
//! `is_pure_expr`, `is_side_effect_call`, `read_is_inside_loop`,
//! `replace_name_in_stmt`, `stmt_reads_name`, `stmts_reassign_name`.

use crate::ast::{BinOp, Expr, Stat, TableField, UnOp};

use super::{
    collect_names_in_expr,
    count_name_reads_in_stmt,
    expr_contains_call,
    expr_references_name,
    is_pure_expr,
    is_side_effect_call,
    read_is_inside_loop,
    replace_name_in_stmt,
    stmt_has_observable_side_effect,
    stmt_reads_name,
    stmt_reads_name_deep,
    stmt_writes_name,
    stmts_reassign_name,
};

/// Recursively collapse nested if-else chains into elseif clauses.
///
/// Transforms:
///   if A then ... else if B then ... else ... end end
/// Into:
///   if A then ... elseif B then ... else ... end
pub(super) fn collapse_elseif_chains(stmts: &mut Vec<Stat>) {
    for stmt in stmts.iter_mut() {
        match stmt {
            Stat::If { then_body, elseif_clauses, else_body, .. } => {
                // Recurse into then/elseif/else bodies
                collapse_elseif_chains(then_body);
                for (_, body) in elseif_clauses.iter_mut() {
                    collapse_elseif_chains(body);
                }
                if let Some(ref mut eb) = else_body {
                    collapse_elseif_chains(eb);
                }

                // Now flatten: if else_body is a single If statement, pull it up as elseif
                loop {
                    let should_flatten = else_body.as_ref().map_or(false, |eb| {
                        eb.len() == 1 && matches!(&eb[0], Stat::If { .. })
                    });
                    if !should_flatten {
                        break;
                    }
                    let mut eb = else_body.take().unwrap();
                    if let Stat::If {
                        condition: inner_cond,
                        then_body: inner_then,
                        elseif_clauses: mut inner_elseifs,
                        else_body: inner_else,
                    } = eb.remove(0) {
                        elseif_clauses.push((inner_cond, inner_then));
                        elseif_clauses.append(&mut inner_elseifs);
                        *else_body = inner_else;
                    }
                }
            }
            Stat::While { body, .. } | Stat::Repeat { body, .. }
            | Stat::DoBlock { body } => {
                collapse_elseif_chains(body);
            }
            Stat::NumericFor { body, .. } | Stat::GenericFor { body, .. } => {
                collapse_elseif_chains(body);
            }
            _ => {}
        }
    }
}

/// Phase B0.42: Collapse short-circuit `and`/`or` assignment patterns.
///
/// Detects three AST shapes that the lifter emits as verbose `if/else`
/// statements when the original Luau source was actually a short-circuit
/// expression, and rewrites them to the original expression form.
///
/// Patterns recognised (single-statement bodies only — multi-statement
/// branches retain their if/else form):
///
///   1. AND  — `if X then X = a end` → `X = X and a`
///             (also handles `local X = ...; if X then X = a end`)
///   2. OR   — `if not X then X = a end` → `X = X or a`
///   3. TER  — `if cond then X = a else X = b end` → `X = cond and a or b`
///             (no elseif; same target on both branches)
///
/// Only collapse when:
///   - the then/else bodies contain exactly ONE statement each;
///   - that statement is an `Assign` to a single `Name` target;
///   - for AND/OR, the condition is exactly `Name(X)` or `not Name(X)`
///     (so we know X was the cond reg);
///   - there are no elseif clauses (those keep their full form).
///
/// We are intentionally conservative: anything that does not cleanly match
/// the patterns above is left alone.
pub(super) fn collapse_short_circuit_assignments(stmts: &mut Vec<Stat>) {
    // First, recurse into nested blocks so we collapse innermost shapes
    // before considering an outer wrapper.
    for stmt in stmts.iter_mut() {
        match stmt {
            Stat::If { then_body, elseif_clauses, else_body, .. } => {
                collapse_short_circuit_assignments(then_body);
                for (_, body) in elseif_clauses.iter_mut() {
                    collapse_short_circuit_assignments(body);
                }
                if let Some(eb) = else_body {
                    collapse_short_circuit_assignments(eb);
                }
            }
            Stat::While { body, .. }
            | Stat::Repeat { body, .. }
            | Stat::DoBlock { body }
            | Stat::NumericFor { body, .. }
            | Stat::GenericFor { body, .. } => {
                collapse_short_circuit_assignments(body);
            }
            _ => {}
        }
    }

    // Now scan this level and replace matching If statements.
    let mut i = 0;
    while i < stmts.len() {
        if let Some(replacement) = try_collapse_if_to_short_circuit(&stmts[i]) {
            stmts[i] = replacement;
        }
        i += 1;
    }
}

/// Returns Some(new_assign_stat) if the given statement matches one of the
/// three short-circuit shapes, otherwise None.
pub(super) fn try_collapse_if_to_short_circuit(stmt: &Stat) -> Option<Stat> {
    let (condition, then_body, elseif_clauses, else_body) = match stmt {
        Stat::If { condition, then_body, elseif_clauses, else_body } => {
            (condition, then_body, elseif_clauses, else_body)
        }
        _ => return None,
    };

    // No elseif — these always retain their full form.
    if !elseif_clauses.is_empty() {
        return None;
    }

    // The then-body must be exactly one Assign to a single Name target.
    let (then_name, then_value) = single_name_assign(then_body)?;

    match else_body {
        // Pattern TER (ternary): both branches assign the same name.
        // Phase B0.86: use Expr::Ternary instead of `cond and a or b`.
        // The and/or form is semantically wrong when `a` is falsy
        // (nil/false) — the or would short-circuit to `b` instead.
        // Ternary delegates the choice to the emitter, which uses
        // `cond and a or b` only when `a` is provably truthy, falling
        // back to Luau's native `if cond then a else b` otherwise.
        Some(eb) => {
            let (else_name, else_value) = single_name_assign(eb)?;
            if else_name != then_name {
                return None;
            }
            let ternary = Expr::Ternary {
                cond: Box::new(condition.clone()),
                then_expr: Box::new(then_value.clone()),
                else_expr: Box::new(else_value.clone()),
            };
            Some(Stat::Assign {
                targets: vec![Expr::Name(then_name.clone())],
                values: vec![ternary],
            })
        }
        // No else — must match AND or OR shape.
        None => {
            // Pattern AND: condition is exactly Name(then_name).
            if let Expr::Name(cond_name) = condition {
                if cond_name == &then_name {
                    let new_val = Expr::BinOp {
                        left: Box::new(Expr::Name(then_name.clone())),
                        op: BinOp::And,
                        right: Box::new(then_value.clone()),
                    };
                    return Some(Stat::Assign {
                        targets: vec![Expr::Name(then_name)],
                        values: vec![new_val],
                    });
                }
            }
            // Pattern OR: condition is exactly `not Name(then_name)`.
            if let Expr::UnOp { op: UnOp::Not, operand } = condition {
                if let Expr::Name(cond_name) = operand.as_ref() {
                    if cond_name == &then_name {
                        let new_val = Expr::BinOp {
                            left: Box::new(Expr::Name(then_name.clone())),
                            op: BinOp::Or,
                            right: Box::new(then_value.clone()),
                        };
                        return Some(Stat::Assign {
                            targets: vec![Expr::Name(then_name)],
                            values: vec![new_val],
                        });
                    }
                }
            }
            None
        }
    }
}

/// If `body` is exactly `[Stat::Assign { targets: [Name(n)], values: [v] }]`,
/// return Some((n.clone(), v.clone())). Otherwise None.
///
/// Local declarations (`local x = a`) are intentionally NOT matched: the
/// resulting short-circuit assignment requires `x` to already be a live
/// variable, and collapsing a `local` would change scoping semantics.
pub(super) fn single_name_assign(body: &[Stat]) -> Option<(String, Expr)> {
    if body.len() != 1 {
        return None;
    }
    if let Stat::Assign { targets, values } = &body[0] {
        if targets.len() == 1 && values.len() == 1 {
            if let Expr::Name(n) = &targets[0] {
                return Some((n.clone(), values[0].clone()));
            }
        }
    }
    None
}

/// Phase B0.87 + B0.88: Collapse `local x = <init>` followed by a conditional
/// assignment into a single declaration with a ternary value.
///
/// Recognises three shapes:
///
///   Pattern A (both branches assign — init is dead):
///     `local x = <any>; if cond then x = a else x = b end`
///     → `local x = if cond then a else b`
///
///   Pattern B (no else, nil init — B0.87 original):
///     `local x = nil; if cond then x = a end`
///     → `local x = if cond then a else nil`
///
///   Pattern C (no else, literal init — B0.88):
///     `local x = <literal>; if cond then x = a end`
///     → `local x = if cond then a else <literal>`
///     where `<literal>` is Nil/Bool/Number/String (pure, re-evaluable).
///
/// In pattern A the init is dead because all branches overwrite `x`, so ANY
/// init expression is safe to discard.  In patterns B/C the init becomes the
/// else-branch, so it must be pure (no side effects from re-evaluation).
///
/// Constraints:
///   - No elseif clauses (those keep their full form).
///   - The condition and branch values must NOT reference `x` — otherwise
///     the rewrite changes the observable value of `x` at evaluation time
///     (the `local` declaration is moved past the condition evaluation).
///   - Both branches must be single-statement `Assign` to `Name(x)`.
///
/// This pass MUST run AFTER `collapse_short_circuit_assignments` which
/// handles the `if x then x = a end` self-referencing shape separately.
pub(super) fn collapse_nil_init_conditional(stmts: &mut Vec<Stat>) {
    // Recurse into nested blocks first.
    for stmt in stmts.iter_mut() {
        match stmt {
            Stat::If { then_body, elseif_clauses, else_body, .. } => {
                collapse_nil_init_conditional(then_body);
                for (_, body) in elseif_clauses.iter_mut() {
                    collapse_nil_init_conditional(body);
                }
                if let Some(eb) = else_body {
                    collapse_nil_init_conditional(eb);
                }
            }
            Stat::While { body, .. }
            | Stat::Repeat { body, .. }
            | Stat::DoBlock { body }
            | Stat::NumericFor { body, .. }
            | Stat::GenericFor { body, .. } => {
                collapse_nil_init_conditional(body);
            }
            _ => {}
        }
    }

    let mut i = 0;
    while i + 1 < stmts.len() {
        // Step 1: stmt[i] must be `local x = <init>` (single binding).
        let (local_name, init_expr) = match &stmts[i] {
            Stat::Local { names, values }
                if names.len() == 1 && values.len() == 1 =>
            {
                (names[0].clone(), values[0].clone())
            }
            _ => { i += 1; continue; }
        };

        // Step 2: stmt[i+1] must be an if/else (no elseif) assigning local_name.
        let new_value = match &stmts[i + 1] {
            Stat::If { condition, then_body, elseif_clauses, else_body }
                if elseif_clauses.is_empty() =>
            {
                // Safety: cond must not reference the local.
                if expr_references_name(condition, &local_name) {
                    i += 1; continue;
                }

                // Then branch must be a single `x = a` assign.
                let (then_name, then_value) = match single_name_assign(then_body) {
                    Some(pair) => pair,
                    None => { i += 1; continue; }
                };
                if then_name != local_name { i += 1; continue; }
                if expr_references_name(&then_value, &local_name) { i += 1; continue; }

                match else_body {
                    // Pattern A: `if cond then x = a else x = b end`
                    // Init is dead — any init works.
                    Some(eb) => {
                        let (else_name, else_value) = match single_name_assign(eb) {
                            Some(pair) => pair,
                            None => { i += 1; continue; }
                        };
                        if else_name != local_name { i += 1; continue; }
                        if expr_references_name(&else_value, &local_name) { i += 1; continue; }
                        Expr::Ternary {
                            cond: Box::new(condition.clone()),
                            then_expr: Box::new(then_value),
                            else_expr: Box::new(else_value),
                        }
                    }
                    // Patterns B/C: `if cond then x = a end` (no else)
                    // Init becomes the else-branch — must be pure literal.
                    None => {
                        if !is_inlinable_literal(&init_expr) {
                            i += 1; continue;
                        }
                        Expr::Ternary {
                            cond: Box::new(condition.clone()),
                            then_expr: Box::new(then_value),
                            else_expr: Box::new(init_expr),
                        }
                    }
                }
            }
            _ => { i += 1; continue; }
        };

        // Merge: replace the Local's value, remove the If.
        stmts[i] = Stat::Local {
            names: vec![local_name],
            values: vec![new_value],
        };
        stmts.remove(i + 1);
        i += 1;
    }
}

/// Phase B0.89: Merge `local x = nil; x = expr` into `local x = expr`.
///
/// The lifter emits `local x = nil` for LOADNIL, then later `x = expr` when
/// the register gets a real value. If the two are consecutive (no intervening
/// statements that read x), the nil initialization is dead and the pair can
/// be merged into a single `local x = expr`.
///
/// Also merges pure-literal inits: `local x = 0; x = expr` → `local x = expr`
/// when the literal value is never read.
///
/// Constraints:
///   - The `Assign` target must be `Name(x)` matching the `Local` name.
///   - The `Assign` RHS must NOT reference x (otherwise it reads the init).
///   - Must be consecutive (i, i+1) — intervening statements break the merge.
pub(super) fn merge_dead_init_with_assignment(stmts: &mut Vec<Stat>) {
    // Recurse into nested blocks first.
    for stmt in stmts.iter_mut() {
        match stmt {
            Stat::If { then_body, elseif_clauses, else_body, .. } => {
                merge_dead_init_with_assignment(then_body);
                for (_, body) in elseif_clauses.iter_mut() {
                    merge_dead_init_with_assignment(body);
                }
                if let Some(eb) = else_body {
                    merge_dead_init_with_assignment(eb);
                }
            }
            Stat::While { body, .. }
            | Stat::Repeat { body, .. }
            | Stat::DoBlock { body }
            | Stat::NumericFor { body, .. }
            | Stat::GenericFor { body, .. } => {
                merge_dead_init_with_assignment(body);
            }
            _ => {}
        }
    }

    let mut i = 0;
    while i + 1 < stmts.len() {
        // Step 1: stmt[i] must be `local x = <literal>` (dead init).
        let local_name = match &stmts[i] {
            Stat::Local { names, values }
                if names.len() == 1 && values.len() == 1
                    && is_inlinable_literal(&values[0]) =>
            {
                names[0].clone()
            }
            _ => { i += 1; continue; }
        };

        // Step 2: stmt[i+1] must be `x = expr` (reassignment).
        let new_value = match &stmts[i + 1] {
            Stat::Assign { targets, values }
                if targets.len() == 1 && values.len() == 1 =>
            {
                if let Expr::Name(n) = &targets[0] {
                    if n != &local_name { i += 1; continue; }
                    // RHS must not reference the local (it would read the init).
                    if expr_references_name(&values[0], &local_name) {
                        i += 1; continue;
                    }
                    values[0].clone()
                } else { i += 1; continue; }
            }
            _ => { i += 1; continue; }
        };

        // Merge: update Local's value, remove the Assign.
        stmts[i] = Stat::Local {
            names: vec![local_name],
            values: vec![new_value],
        };
        stmts.remove(i + 1);
        i += 1;
    }
}

/// Phase B0.79: Check if a name is a decompiler-generated function name (`fn\d+`).
/// These are anonymous closures that were assigned to temporary registers;
/// they are safe to inline back into their single use site.
fn is_generated_fn_name(name: &str) -> bool {
    if !name.starts_with("fn") { return false; }
    let rest = &name[2..];
    !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit())
}

/// B0.117: Check if an expression is a valid root for an assignment target.
/// Valid roots are Name identifiers or Field/Index chains ending in a Name.
fn is_valid_lvalue_root(expr: &Expr) -> bool {
    match expr {
        Expr::Name(n) => {
            let mut chars = n.chars();
            match chars.next() {
                Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
                _ => return false,
            }
            chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        Expr::Field { object, .. } | Expr::Index { object, .. } => {
            is_valid_lvalue_root(object)
        }
        _ => false,
    }
}

/// B0.117: Check if a statement (or any nested statement) uses `name` as the
/// root of an assignment target. Recurses into if/while/for/repeat/do blocks.
fn stmt_uses_name_as_target_root(stmt: &Stat, name: &str) -> bool {
    match stmt {
        Stat::Assign { targets, .. } => {
            for t in targets {
                if target_has_name_root(t, name) {
                    return true;
                }
            }
            false
        }
        Stat::If { then_body, elseif_clauses, else_body, .. } => {
            then_body.iter().any(|s| stmt_uses_name_as_target_root(s, name))
            || elseif_clauses.iter().any(|(_, body)| body.iter().any(|s| stmt_uses_name_as_target_root(s, name)))
            || else_body.as_ref().map_or(false, |eb| eb.iter().any(|s| stmt_uses_name_as_target_root(s, name)))
        }
        Stat::While { body, .. } | Stat::Repeat { body, .. }
        | Stat::NumericFor { body, .. } | Stat::GenericFor { body, .. }
        | Stat::DoBlock { body } => {
            body.iter().any(|s| stmt_uses_name_as_target_root(s, name))
        }
        _ => false,
    }
}

/// B0.120: Check if an expression uses `name` as an expression base — i.e.,
/// as the object of Field/Index/MethodCall or the function of Call.
/// When a Table literal is inlined into such a position, it produces garbled
/// output like `({}).field`, `({}):Method()`, `({})("arg")`.
fn expr_uses_name_as_base(expr: &Expr, name: &str) -> bool {
    match expr {
        Expr::Field { object, .. } | Expr::Index { object, .. } => {
            // name is the object being accessed
            matches!(object.as_ref(), Expr::Name(n) if n == name)
            || expr_uses_name_as_base(object, name)
        }
        Expr::MethodCall { object, args, .. } => {
            // name is the object receiving the method call
            matches!(object.as_ref(), Expr::Name(n) if n == name)
            || expr_uses_name_as_base(object, name)
            // B0.123: recurse into args — nested `name(x)` inside
            // `f(name(x))` still produces `({})(x)` when inlined
            || args.iter().any(|a| expr_uses_name_as_base(a, name))
        }
        Expr::Call { func, args } => {
            // name is being called as a function
            matches!(func.as_ref(), Expr::Name(n) if n == name)
            || expr_uses_name_as_base(func, name)
            // B0.123: recurse into args for nested base usage
            || args.iter().any(|a| expr_uses_name_as_base(a, name))
        }
        Expr::BinOp { left, right, .. } => {
            expr_uses_name_as_base(left, name)
            || expr_uses_name_as_base(right, name)
        }
        Expr::UnOp { operand, .. } => expr_uses_name_as_base(operand, name),
        // B0.123b: recurse into Table constructor values — catches
        // `{ ["key"] = name.field }` patterns that produce `({}).field`
        Expr::Table { fields } => {
            fields.iter().any(|f| match f {
                TableField::Sequential(e) => expr_uses_name_as_base(e, name),
                TableField::Named(_, e) => expr_uses_name_as_base(e, name),
                TableField::Indexed(k, v) => {
                    expr_uses_name_as_base(k, name)
                    || expr_uses_name_as_base(v, name)
                }
            })
        }
        // B0.123b: recurse into Function body statements
        Expr::Function { body, .. } => {
            body.iter().any(|s| stmt_uses_name_as_expr_base(s, name))
        }
        _ => false,
    }
}

/// B0.120: Check if any expression in a statement uses `name` as a base.
fn stmt_uses_name_as_expr_base(stmt: &Stat, name: &str) -> bool {
    match stmt {
        Stat::Local { values, .. } => values.iter().any(|e| expr_uses_name_as_base(e, name)),
        Stat::Assign { targets, values } => {
            targets.iter().any(|e| expr_uses_name_as_base(e, name))
            || values.iter().any(|e| expr_uses_name_as_base(e, name))
        }
        Stat::ExprStat(e) => expr_uses_name_as_base(e, name),
        Stat::Return { values } => values.iter().any(|e| expr_uses_name_as_base(e, name)),
        Stat::If { condition, then_body, elseif_clauses, else_body } => {
            expr_uses_name_as_base(condition, name)
            || then_body.iter().any(|s| stmt_uses_name_as_expr_base(s, name))
            || elseif_clauses.iter().any(|(c, body)| expr_uses_name_as_base(c, name) || body.iter().any(|s| stmt_uses_name_as_expr_base(s, name)))
            || else_body.as_ref().map_or(false, |eb| eb.iter().any(|s| stmt_uses_name_as_expr_base(s, name)))
        }
        Stat::While { condition, body } => {
            expr_uses_name_as_base(condition, name)
            || body.iter().any(|s| stmt_uses_name_as_expr_base(s, name))
        }
        Stat::Repeat { body, condition } => {
            body.iter().any(|s| stmt_uses_name_as_expr_base(s, name))
            || expr_uses_name_as_base(condition, name)
        }
        // B0.123: check iterators/start/stop/step — not just body
        Stat::GenericFor { iterators, body, .. } => {
            iterators.iter().any(|e| expr_uses_name_as_base(e, name))
            || body.iter().any(|s| stmt_uses_name_as_expr_base(s, name))
        }
        Stat::NumericFor { start, stop, step, body, .. } => {
            expr_uses_name_as_base(start, name)
            || expr_uses_name_as_base(stop, name)
            || step.as_ref().map_or(false, |s| expr_uses_name_as_base(s, name))
            || body.iter().any(|s| stmt_uses_name_as_expr_base(s, name))
        }
        Stat::DoBlock { body } => {
            body.iter().any(|s| stmt_uses_name_as_expr_base(s, name))
        }
        _ => false,
    }
}

/// Walk down Field/Index chain to see if the root is Name(name).
fn target_has_name_root(expr: &Expr, name: &str) -> bool {
    match expr {
        Expr::Name(n) => n == name,
        Expr::Field { object, .. } | Expr::Index { object, .. } => {
            target_has_name_root(object, name)
        }
        _ => false,
    }
}

/// Does `Name(name)` appear anywhere in `expr` in a slot that expands a
/// multi-result call — the last argument of a call, or the last array element
/// of a table constructor?
fn expr_uses_name_in_tail_position(expr: &Expr, name: &str) -> bool {
    let is_target = |e: &Expr| matches!(e, Expr::Name(n) if n == name);
    match expr {
        Expr::Call { func, args } => {
            args.last().map_or(false, &is_target)
                || expr_uses_name_in_tail_position(func, name)
                || args.iter().any(|a| expr_uses_name_in_tail_position(a, name))
        }
        Expr::MethodCall { object, args, .. } => {
            args.last().map_or(false, &is_target)
                || expr_uses_name_in_tail_position(object, name)
                || args.iter().any(|a| expr_uses_name_in_tail_position(a, name))
        }
        Expr::Table { fields } => {
            let tail_is_target = matches!(
                fields.last(),
                Some(TableField::Sequential(e)) if is_target(e)
            );
            tail_is_target
                || fields.iter().any(|f| match f {
                    TableField::Sequential(e) => expr_uses_name_in_tail_position(e, name),
                    TableField::Named(_, e) => expr_uses_name_in_tail_position(e, name),
                    TableField::Indexed(k, v) => {
                        expr_uses_name_in_tail_position(k, name)
                            || expr_uses_name_in_tail_position(v, name)
                    }
                })
        }
        Expr::Field { object, .. } => expr_uses_name_in_tail_position(object, name),
        Expr::Index { object, key } => {
            expr_uses_name_in_tail_position(object, name)
                || expr_uses_name_in_tail_position(key, name)
        }
        Expr::BinOp { left, right, .. } => {
            expr_uses_name_in_tail_position(left, name)
                || expr_uses_name_in_tail_position(right, name)
        }
        Expr::UnOp { operand, .. } => expr_uses_name_in_tail_position(operand, name),
        Expr::Ternary { cond, then_expr, else_expr } => {
            expr_uses_name_in_tail_position(cond, name)
                || expr_uses_name_in_tail_position(then_expr, name)
                || expr_uses_name_in_tail_position(else_expr, name)
        }
        _ => false,
    }
}

/// Statement-level wrapper for [`expr_uses_name_in_tail_position`]; also treats
/// the final value of a `return` and the final value of a multi-value
/// assignment as expanding slots.
fn stmt_uses_name_in_tail_position(stmt: &Stat, name: &str) -> bool {
    let is_target = |e: &Expr| matches!(e, Expr::Name(n) if n == name);
    let tail_of = |vals: &Vec<Expr>| vals.last().map_or(false, &is_target);
    match stmt {
        Stat::Return { values } => {
            tail_of(values) || values.iter().any(|v| expr_uses_name_in_tail_position(v, name))
        }
        Stat::Local { values, .. } => {
            tail_of(values) || values.iter().any(|v| expr_uses_name_in_tail_position(v, name))
        }
        Stat::Assign { targets, values } => {
            tail_of(values)
                || values.iter().any(|v| expr_uses_name_in_tail_position(v, name))
                || targets.iter().any(|t| expr_uses_name_in_tail_position(t, name))
        }
        Stat::ExprStat(e) => expr_uses_name_in_tail_position(e, name),
        _ => false,
    }
}

/// Convenience entry point with no arity-pinned temps. Used by the unit tests,
/// which build ASTs directly and therefore have no bytecode-level arity info.
#[cfg(test)]
pub(super) fn inline_single_use_temps(stmts: &mut Vec<Stat>) {
    inline_single_use_temps_pinned(stmts, &std::collections::HashSet::new());
}

/// As [`inline_single_use_temps`], but refuses to inline a temp listed in
/// `arity_pinned` into a slot where a multi-result call would re-expand.
pub(super) fn inline_single_use_temps_pinned(
    stmts: &mut Vec<Stat>,
    arity_pinned: &std::collections::HashSet<String>,
) {
    // Recurse into nested blocks first
    for stmt in stmts.iter_mut() {
        match stmt {
            Stat::If { then_body, elseif_clauses, else_body, .. } => {
                inline_single_use_temps_pinned(then_body, arity_pinned);
                for (_, body) in elseif_clauses {
                    inline_single_use_temps_pinned(body, arity_pinned);
                }
                if let Some(eb) = else_body { inline_single_use_temps_pinned(eb, arity_pinned); }
            }
            Stat::While { body, .. } => inline_single_use_temps_pinned(body, arity_pinned),
            Stat::Repeat { body, .. } => inline_single_use_temps_pinned(body, arity_pinned),
            Stat::NumericFor { body, .. } => inline_single_use_temps_pinned(body, arity_pinned),
            Stat::GenericFor { body, .. } => inline_single_use_temps_pinned(body, arity_pinned),
            Stat::DoBlock { body } => inline_single_use_temps_pinned(body, arity_pinned),
            _ => {}
        }
    }

    // Iterate and find single-use temps to inline
    let mut changed = true;
    while changed {
        changed = false;
        let mut i = 0;
        while i < stmts.len() {
            // Extract definition: single variable = inlinable expression.
            // Inlinable: Call, MethodCall, Field, Index, Name, BinOp, UnOp,
            //            Number, String, Bool, Nil, Table.
            // Conditionally inlinable: Function — only when the name is a
            //   generated `fn\d+` pattern and the body is short (≤20 stmts).
            //   Phase B0.79: this converts `local fn4 = function() ... end;
            //   Event:Connect(fn4)` → `Event:Connect(function() ... end)`.
            // Phase B0.104: Tables are now inlinable. reconstruct_table_constructors
            //   runs BEFORE this pass, so all field mutations are already absorbed
            //   into the constructor. Single-use read count guards prevent inlining
            //   when the table is mutated or read multiple times.
            let (def_name, def_expr) = match &stmts[i] {
                Stat::Local { names, values } if names.len() == 1 && values.len() == 1 => {
                    match &values[0] {
                        Expr::Function { body, .. } => {
                            // Only inline short functions with generated names
                            if body.len() > 20 || !is_generated_fn_name(&names[0]) {
                                i += 1; continue;
                            }
                            (names[0].clone(), values[0].clone())
                        }
                        _ => (names[0].clone(), values[0].clone())
                    }
                }
                Stat::Assign { targets, values } if targets.len() == 1 && values.len() == 1 => {
                    match (&targets[0], &values[0]) {
                        (Expr::Name(n), v) => match v {
                            Expr::Function { body, .. } => {
                                if body.len() > 20 || !is_generated_fn_name(n) {
                                    i += 1; continue;
                                }
                                (n.clone(), values[0].clone())
                            }
                            _ => (n.clone(), values[0].clone())
                        },
                        _ => { i += 1; continue; }
                    }
                }
                _ => { i += 1; continue; }
            };

            // Guard: don't inline calls whose function is a known side-effect
            // global (require, pcall, etc.). These read/run external code and
            // moving them to the use site changes evaluation timing. Also skip
            // if the name is Konstant-style semantic (Capitalized, likely a
            // module-import local the user would want to keep visible).
            if is_side_effect_call(&def_expr) {
                i += 1;
                continue;
            }
            if def_name.chars().next().map_or(false, |c| c.is_ascii_uppercase()) {
                // Capitalized name suggests a module/class import; keep as-is
                // to match Konstant's style of declaring requires at the top.
                i += 1;
                continue;
            }

            // Count how many times this name is read in ALL statements at this level
            let mut read_count = 0usize;
            for s in stmts.iter() {
                read_count += count_name_reads_in_stmt(s, &def_name);
                if read_count > 1 { break; } // early exit
            }

            // Only inline if read exactly once (the definition's own value doesn't count)
            // Subtract 0 because we counted reads, not the definition itself
            if read_count != 1 {
                i += 1;
                continue;
            }

            // Find the use site (first reader at this block level).
            let mut reader_idx: Option<usize> = None;
            for j in (i + 1)..stmts.len() {
                if stmt_reads_name(&stmts[j], &def_name) {
                    reader_idx = Some(j);
                    break;
                }
            }

            let Some(j) = reader_idx else {
                i += 1;
                continue;
            };

            // Phase B0.45A safety checks ─────────────────────────────────
            //
            // Phase B0.79: Function literals are always safe to inline.
            // Creating a function has no side effects, and Luau closures
            // capture upvalues by reference, so moving the creation point
            // to the use site doesn't change behavior. Skip all safety
            // checks for Function expressions.
            let is_fn_literal = matches!(&def_expr, Expr::Function { .. });

            if !is_fn_literal {
            // (A) Loop-body re-evaluation guard: if the reader is inside a
            //     loop body AND the RHS contains a call, inlining would make
            //     the call execute N times instead of once.  Skip.
            let rhs_has_call = expr_contains_call(&def_expr);
            let reader_in_loop = read_is_inside_loop(&stmts[j], &def_name)
                .unwrap_or(false);
            if rhs_has_call && reader_in_loop {
                i += 1;
                continue;
            }

            // (B) Intervening-side-effect guard: if the RHS is NOT pure
            //     (e.g. Field/Index/Call/MethodCall/BinOp-of-impure), check
            //     that no intervening stmts (strictly between i and j)
            //     have observable side effects.  Pure RHS (literals,
            //     Name, BinOp-of-pure, etc.) is always safe to inline.
            if !is_pure_expr(&def_expr) {
                let intervening = &stmts[i + 1..j];
                if intervening.iter().any(stmt_has_observable_side_effect) {
                    i += 1;
                    continue;
                }
            } else {
                // (C) Pure-Name RHS: if RHS is just `Expr::Name(x)`, make sure
                //     `x` isn't reassigned between i and j.  Otherwise the
                //     inlined expression would observe the new value, not the
                //     captured snapshot.  (Deep Name-containing exprs like
                //     `t.field` also need this for the base name; we check
                //     all Names in the RHS conservatively.)
                let intervening = &stmts[i + 1..j];
                if !intervening.is_empty() {
                    // Collect all Name identifiers used in RHS.
                    let mut rhs_names: Vec<String> = Vec::new();
                    collect_names_in_expr(&def_expr, &mut rhs_names);
                    if rhs_names.iter().any(|n| stmts_reassign_name(intervening, n)) {
                        i += 1;
                        continue;
                    }
                }
            }
            } // end if !is_fn_literal

            // Phase B0.114: don't inline Table literals into require() arguments.
            // Table reconstruction absorbs field assignments into the constructor,
            // then this pass would inline the table into require(), producing
            // `require({Framework = v5})` — nonsensical Luau. Keep the local.
            if matches!(&def_expr, Expr::Table { .. }) && stmt_passes_name_to_require(&stmts[j], &def_name) {
                i += 1;
                continue;
            }

            // B0.117: don't inline non-Name expressions into assignment target
            // roots. Inlining `local v5 = {}` into `v5.field = val` produces
            // `({}).field = val` which is syntactically invalid Luau. Only allow
            // inlining into targets when the replacement is itself a valid lvalue
            // root (Name or Field/Index chain rooted in Name).
            if !is_valid_lvalue_root(&def_expr) && stmt_uses_name_as_target_root(&stmts[j], &def_name) {
                i += 1;
                continue;
            }

            // B0.120: don't inline Table literals when the name is used as an
            // expression base (object of Field/Index/MethodCall or func of Call).
            // Produces garbled `({}).field`, `({}):Method()`, `({})("arg")` etc.
            // Keep the local declaration so the code remains readable.
            if matches!(&def_expr, Expr::Table { .. }) && stmt_uses_name_as_expr_base(&stmts[j], &def_name) {
                i += 1;
                continue;
            }

            // The temp holds a call the bytecode pinned to exactly one result
            // (`local x = (f())`). Splicing it into a tail-expanding slot — the
            // last argument of a call, the last return value, the last array
            // field of a table — would re-expand the call to all its results,
            // turning `print((s:gsub(a, b)))` back into `print(s:gsub(a, b))`.
            // Keep the temp visible instead; it is semantically exact.
            if arity_pinned.contains(&def_name)
                && stmt_uses_name_in_tail_position(&stmts[j], &def_name)
            {
                i += 1;
                continue;
            }

            // Safe to inline.
            replace_name_in_stmt(&mut stmts[j], &def_name, &def_expr);
            stmts.remove(i);
            changed = true;
            // Don't advance i — re-check from same position
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
// Phase C10O — unwrap pre-materialized require() wrapper locals
// ═══════════════════════════════════════════════════════════════════
//
// C8 (opcode_handlers.rs) unwraps inline `require({K = inner})` at
// CALL-handler time. But the lifter frequently materializes the wrapper
// table into its own local before the CALL, so at CALL time the arg is
// `Name(R)`, not `Table`. B0.114 then blocks `inline_single_use_temps`
// from re-inlining the table into require() — correctly, because the
// untransformed shape `require({K = X})` is nonsense Luau that we never
// want to emit.
//
// This pass rewrites the pre-materialized form in one step:
//
//     local R = { K = inner }        -- exactly one Named field
//     [stmt referencing require(R)]  -- R is a direct arg to require()
//
// becomes
//
//     [stmt referencing require(inner)]
//
// Safety: R must be read exactly once across the block (so dropping the
// local is guaranteed safe), and `inner` must be a module-shaped
// expression (Name / Field / Index / MethodCall / Call) — matching the
// same gate C8 uses on the inline form.
pub(super) fn unwrap_require_wrapper_locals(stmts: &mut Vec<Stat>) {
    for stmt in stmts.iter_mut() {
        match stmt {
            Stat::If { then_body, elseif_clauses, else_body, .. } => {
                unwrap_require_wrapper_locals(then_body);
                for (_, body) in elseif_clauses {
                    unwrap_require_wrapper_locals(body);
                }
                if let Some(eb) = else_body {
                    unwrap_require_wrapper_locals(eb);
                }
            }
            Stat::While { body, .. }
            | Stat::Repeat { body, .. }
            | Stat::DoBlock { body }
            | Stat::NumericFor { body, .. }
            | Stat::GenericFor { body, .. } => unwrap_require_wrapper_locals(body),
            _ => {}
        }
    }

    let mut i = 0;
    while i + 1 < stmts.len() {
        let (name, inner) = match &stmts[i] {
            Stat::Local { names, values }
                if names.len() == 1 && values.len() == 1 =>
            {
                match &values[0] {
                    Expr::Table { fields } if fields.len() == 1 => match &fields[0] {
                        TableField::Named(_, inner)
                            if matches!(
                                inner,
                                Expr::Name(_)
                                    | Expr::Field { .. }
                                    | Expr::Index { .. }
                                    | Expr::MethodCall { .. }
                                    | Expr::Call { .. }
                            ) =>
                        {
                            (names[0].clone(), inner.clone())
                        }
                        _ => {
                            i += 1;
                            continue;
                        }
                    },
                    _ => {
                        i += 1;
                        continue;
                    }
                }
            }
            _ => {
                i += 1;
                continue;
            }
        };

        if !stmt_passes_name_to_require(&stmts[i + 1], &name) {
            i += 1;
            continue;
        }

        let mut read_count = 0usize;
        for s in stmts.iter() {
            read_count += count_name_reads_in_stmt(s, &name);
            if read_count > 1 {
                break;
            }
        }
        if read_count != 1 {
            i += 1;
            continue;
        }

        replace_name_in_stmt(&mut stmts[i + 1], &name, &inner);
        stmts.remove(i);
        // don't advance — the replacement may expose another wrapper
    }
}

// ═══════════════════════════════════════════════════════════════════
// Phase B0.51B — INLINE PURE LITERALS at all read sites
// ═══════════════════════════════════════════════════════════════════
//
// `inline_single_use_temps` (B0.45A) only inlines when the RHS has read
// count == 1.  But for pure literal RHS values (Nil, Bool, Number,
// String) there is no semantic difference between inlining once and
// inlining N times — re-evaluating a literal cannot change its value
// or have side effects.  Vector, Import (= Field/Name), Table, and
// Function are intentionally excluded; see `is_inlinable_literal`.
//
// This pass targets the common Roblox pattern where a single register is
// reused with multiple LOADK loads of constant strings:
//
//   local v3 = "Players"
//   game:GetService(v3)        -- read 1
//   v3 = "Workspace"           -- reassigned
//   game:GetService(v3)        -- read 2
//
// AND the more common case where the lifter emits `local v3 = "X"` once
// but the call references it multiple times because the same register
// was consumed by repeated calls in the source code.
//
// Because literals are pure and immutable, inlining at every read site
// (between def and the next reassignment of `name`) is always safe.
//
// Constraints maintained:
//   * Capitalized names (Konstant-style imports) are skipped — they
//     mirror the user's original style and should remain visible.
//   * Multi-value Locals (e.g., `local a, b = pcall(f)`) are skipped.
//   * Reassignments of the name truncate the inlining range — reads
//     after the reassignment see the new value, not the literal.
//   * Recurses into nested blocks (If/While/Repeat/For/DoBlock) so the
//     pass operates at every scope.

/// Returns true for "small immutable" literal expressions whose
/// re-evaluation has no observable cost or side effect.
///
/// Includes: Nil, Bool, Number, String.
/// Excludes: Vector (multi-component, user preference to keep as
/// `Vector3.new(...)` at the declaration site), Import (becomes Field
/// or Name via constant_to_expr and is handled by B0.45A), Table
/// (identity-sensitive, mutable after construction), Function (closure
/// state), Call (side effects).
pub(super) fn is_inlinable_literal(expr: &Expr) -> bool {
    matches!(expr,
        Expr::Nil | Expr::Bool(_) | Expr::Number(_) | Expr::String(_))
}

/// Returns true if any statement in `stmts` writes to a binding named
/// `name` — either by re-declaring it (`local name = ...`) or by
/// assigning to it (`name = ...`).  Recurses into control-flow blocks
/// (If/While/Repeat/For/DoBlock) so a deeply-nested reassignment is
/// detected.
///
/// Used by `inline_pure_literals` to find the first reassignment after
/// a definition; reads BEFORE the reassignment are safely inlinable,
/// reads AFTER refer to the new value and must be left alone.
pub(super) fn stmt_writes_name_recursive(stmt: &Stat, name: &str) -> bool {
    match stmt {
        Stat::Local { names, .. } => names.iter().any(|n| n == name),
        Stat::Assign { targets, .. } => {
            targets.iter().any(|t| matches!(t, Expr::Name(n) if n == name))
        }
        Stat::If { then_body, elseif_clauses, else_body, .. } => {
            then_body.iter().any(|s| stmt_writes_name_recursive(s, name))
            || elseif_clauses.iter().any(|(_, b)|
                b.iter().any(|s| stmt_writes_name_recursive(s, name)))
            || else_body.as_ref().map_or(false, |eb|
                eb.iter().any(|s| stmt_writes_name_recursive(s, name)))
        }
        Stat::While { body, .. } | Stat::Repeat { body, .. }
        | Stat::DoBlock { body }
        | Stat::NumericFor { body, .. } | Stat::GenericFor { body, .. } => {
            body.iter().any(|s| stmt_writes_name_recursive(s, name))
        }
        _ => false,
    }
}

/// Phase B0.51B: inline pure literal locals at ALL read sites
/// (regardless of read count) up to the next reassignment of the name.
///
/// Removes the local declaration if it was successfully inlined and
/// the binding is not referenced past its inlining range.
pub(super) fn inline_pure_literals(stmts: &mut Vec<Stat>) {
    // Recurse into nested blocks first, so inner-scope inlining is
    // handled before the outer scope decides what to remove.
    for stmt in stmts.iter_mut() {
        match stmt {
            Stat::If { then_body, elseif_clauses, else_body, .. } => {
                inline_pure_literals(then_body);
                for (_, body) in elseif_clauses {
                    inline_pure_literals(body);
                }
                if let Some(eb) = else_body { inline_pure_literals(eb); }
            }
            Stat::While { body, .. } => inline_pure_literals(body),
            Stat::Repeat { body, .. } => inline_pure_literals(body),
            Stat::NumericFor { body, .. } => inline_pure_literals(body),
            Stat::GenericFor { body, .. } => inline_pure_literals(body),
            Stat::DoBlock { body } => inline_pure_literals(body),
            _ => {}
        }
    }

    let mut i = 0;
    while i < stmts.len() {
        // Find a single-name local with a literal RHS.  We deliberately
        // exclude `Stat::Assign` here because re-binding a literal to an
        // existing name doesn't reduce reads (the binding still exists);
        // and we want to keep the visible reassignment in the output.
        let (def_name, def_expr) = match &stmts[i] {
            Stat::Local { names, values } if names.len() == 1 && values.len() == 1 => {
                if !is_inlinable_literal(&values[0]) {
                    i += 1;
                    continue;
                }
                (names[0].clone(), values[0].clone())
            }
            _ => { i += 1; continue; }
        };

        // Konstant-style: capitalized names are kept visible (matches the
        // existing `inline_single_use_temps` policy, see B0.45A).
        if def_name.chars().next().map_or(false, |c| c.is_ascii_uppercase()) {
            i += 1;
            continue;
        }

        // A local captured as an upvalue must not be inlined or removed.
        // `stmt_reads_name` / `stmt_writes_name_recursive` below do not descend
        // into `Expr::Function` bodies, so a captured local looked completely
        // dead: its literal was constant-propagated into the one visible read
        // site and the declaration was then deleted, silently turning the
        // upvalue into an undeclared global. Detect references that exist ONLY
        // inside a closure body and leave the binding alone.
        let captured_by_closure = stmts[(i + 1)..].iter().any(|s| {
            (stmt_reads_name_deep(s, &def_name) && !stmt_reads_name(s, &def_name))
                || (stmt_writes_name(s, &def_name)
                    && !stmt_writes_name_recursive(s, &def_name))
        });
        if captured_by_closure {
            i += 1;
            continue;
        }

        // Find the first reassignment of `def_name` after the definition.
        // Reads strictly between i and `reassign_idx` can be inlined;
        // reads at or after `reassign_idx` see the new value.
        let mut reassign_idx: Option<usize> = None;
        for j in (i + 1)..stmts.len() {
            if stmt_writes_name_recursive(&stmts[j], &def_name) {
                reassign_idx = Some(j);
                break;
            }
        }

        let end = reassign_idx.unwrap_or(stmts.len());

        // Inline at every read site in (i, end).  Replace any Name(def_name)
        // occurrence with the literal expression — `replace_name_in_stmt`
        // recurses into nested blocks/expressions for us.
        let mut any_inlined = false;
        for j in (i + 1)..end {
            if stmt_reads_name(&stmts[j], &def_name) {
                replace_name_in_stmt(&mut stmts[j], &def_name, &def_expr);
                any_inlined = true;
            }
        }

        if any_inlined && reassign_idx.is_none() {
            // No reassignment ahead and we successfully inlined every
            // read — the local declaration is now dead, remove it.
            stmts.remove(i);
            // Don't advance i; re-check the same position.
            continue;
        }

        // If there's a future reassignment, keep the declaration so the
        // assign target remains valid; we still inlined the reads in the
        // pre-reassignment range, which is the win we wanted.
        i += 1;
    }
}

/// Phase B0.60 — reconstruct Luau method-function statements from the
/// two-step `local F = function(...) end; Base.X = F` pattern.
///
/// Most Roblox module code compiles to:
///
///   NEWCLOSURE R(tmp) -> stored as `local F = function(...)` (with some name)
///   SETTABLEKS R(base) K("X") R(tmp) -> stored as `Base.X = F`
///
/// Idiomatic Luau source would have been either
///   `function Base:X(...) ... end`     (when first param is self)
///   `function Base.X(...) ... end`     (when not a method)
///   `Base.X = function(...) ... end`   (raw assign — rare in modules)
///
/// This pass walks statement lists and fuses matching Local+Assign pairs
/// into `Stat::MethodFunction`, which emit.rs knows how to render
/// correctly. When the function's first parameter is used as a field or
/// method receiver (`self.field`, `self:method(...)`), we set
/// `is_method = true` so the emitter produces `:` syntax and strips the
/// leading self param from the rendered signature.
///
/// Conservative — only fires when:
///   * F has exactly one reader (the following assign) in the scope
///   * The receiver is a simple Name or Field (no Call / Index)
///   * The assign targets a field, not a global
///   * The function isn't reassigned or re-read later
pub(super) fn reconstruct_method_assignments(stmts: &mut Vec<Stat>) {
    // Recurse into nested bodies first so inner patterns convert before
    // outer ones (inner method-function wins don't leave stale Local
    // declarations that would confuse outer detection).
    for stmt in stmts.iter_mut() {
        match stmt {
            Stat::If { then_body, elseif_clauses, else_body, .. } => {
                reconstruct_method_assignments(then_body);
                for (_, body) in elseif_clauses.iter_mut() {
                    reconstruct_method_assignments(body);
                }
                if let Some(eb) = else_body {
                    reconstruct_method_assignments(eb);
                }
            }
            Stat::While { body, .. }
            | Stat::Repeat { body, .. }
            | Stat::NumericFor { body, .. }
            | Stat::GenericFor { body, .. }
            | Stat::DoBlock { body } => {
                reconstruct_method_assignments(body);
            }
            Stat::LocalFunction { func, .. } | Stat::MethodFunction { func, .. } => {
                if let Expr::Function { body, .. } = func {
                    reconstruct_method_assignments(body);
                }
            }
            Stat::Local { values, .. } | Stat::Assign { values, .. } => {
                for v in values.iter_mut() {
                    if let Expr::Function { body, .. } = v {
                        reconstruct_method_assignments(body);
                    }
                }
            }
            _ => {}
        }
    }

    let mut i = 0;
    while i + 1 < stmts.len() {
        // Match `local F = function(...) end` at position i.
        let (fname, params_first, is_vararg, is_single_local) = match &stmts[i] {
            Stat::Local { names, values } if names.len() == 1 && values.len() == 1 => {
                if let Expr::Function { params, is_vararg, .. } = &values[0] {
                    (names[0].clone(), params.first().cloned(), *is_vararg, true)
                } else { i += 1; continue; }
            }
            _ => { i += 1; continue; }
        };
        let _ = params_first; // only used via body inspection below
        let _ = is_vararg;
        let _ = is_single_local;

        // Match `<receiver>.<method> = F` at position i+1.
        let (receiver, method) = match &stmts[i + 1] {
            Stat::Assign { targets, values } if targets.len() == 1 && values.len() == 1 => {
                if let (Expr::Field { object, field }, Expr::Name(n)) = (&targets[0], &values[0]) {
                    if n != &fname { i += 1; continue; }
                    if !is_safe_receiver(object) { i += 1; continue; }
                    ((**object).clone(), field.clone())
                } else { i += 1; continue; }
            }
            _ => { i += 1; continue; }
        };

        // Ensure `F` has no other readers / writers in later statements.
        let mut has_other_ref = false;
        for j in (i + 2)..stmts.len() {
            if count_name_reads_in_stmt(&stmts[j], &fname) > 0
                || stmts_reassign_name(&stmts[j..=j], &fname)
            {
                has_other_ref = true;
                break;
            }
        }
        if has_other_ref { i += 1; continue; }

        // Re-extract the Expr::Function (consume both statements).
        let func = match stmts.remove(i) {
            Stat::Local { mut values, .. } => values.remove(0),
            _ => { unreachable!(); }
        };
        // Remove the Assign — it's now at index i after removing Local.
        let _ = stmts.remove(i);

        let is_method = match &func {
            Expr::Function { params, body, .. } => {
                should_emit_as_method(params, body)
            }
            _ => false,
        };

        stmts.insert(i, Stat::MethodFunction {
            receiver,
            method,
            is_method,
            func,
        });
        // Skip past the just-inserted statement.
        i += 1;
    }
}

/// B0.60 — a receiver is safe for method-function shorthand if evaluating
/// it has no side effects AND it's syntactically valid as the LHS of
/// `function <receiver>:method(...)`. We allow Name, Field(Name), and
/// Field chains; reject Index (dynamic keys can be effectful) and Call.
///
/// Phase C10S: also reject `Name(n)` where `n` is a Luau stdlib *function*
/// (e.g. `setmetatable`, `pcall`, `require`). Those are never tables in
/// real code — surface-level output `function setmetatable.X()` is
/// guaranteed decompiler garbage from an upstream register-tracking miss.
fn is_safe_receiver(e: &Expr) -> bool {
    match e {
        Expr::Name(n) => !is_stdlib_function_only_shadow(n),
        Expr::Field { object, .. } => is_safe_receiver(object),
        _ => false,
    }
}

/// Phase C10S — names that are Luau **stdlib functions** (not tables).
/// Assigning to `setmetatable.X = ...` or reading `pcall:method(...)` is
/// never valid source code; any such AST node is a decompiler artifact
/// from a corrupted register hint.
///
/// Distinct from `is_stdlib_shadow_name` (which also covers stdlib *tables*
/// like `math`, `string`, `table` — those CAN legally appear as field
/// bases e.g. `math.floor`).
pub(super) fn is_stdlib_function_only_shadow(s: &str) -> bool {
    matches!(s,
        "setmetatable" | "getmetatable"
        | "pcall" | "xpcall" | "require"
        | "assert" | "error" | "select"
        | "tostring" | "tonumber" | "typeof" | "type"
        | "print" | "warn"
        | "next" | "pairs" | "ipairs"
        | "rawget" | "rawset" | "rawequal" | "rawlen"
        | "unpack" | "collectgarbage"
        | "loadstring" | "newproxy"
        | "tick" | "time"
        | "wait" | "delay" | "spawn")
}

/// Phase C10S — drop `Stat::Assign { target: Field { object: Name(shadow), .. } }`
/// when `shadow` is a stdlib function. These are always corrupt lvalues:
/// real source code never writes `setmetatable.field = v`, and rendering
/// the artifact in output is pure noise. Also drops companion
/// `Stat::MethodFunction { receiver: Name(shadow), .. }` nodes that would
/// have been formed upstream if the `is_safe_receiver` guard hadn't fired.
///
/// Safety: only drops assigns / method-functions; never touches reads.
/// Reading `pcall.whatever` is still harmless (nil or runtime-error) and
/// typically gets elided by downstream dead-code passes.
pub(super) fn drop_stdlib_function_lvalue_artifacts(stmts: &mut Vec<Stat>) {
    // Recurse into nested bodies first.
    for stmt in stmts.iter_mut() {
        match stmt {
            Stat::If { then_body, elseif_clauses, else_body, .. } => {
                drop_stdlib_function_lvalue_artifacts(then_body);
                for (_, body) in elseif_clauses.iter_mut() {
                    drop_stdlib_function_lvalue_artifacts(body);
                }
                if let Some(eb) = else_body {
                    drop_stdlib_function_lvalue_artifacts(eb);
                }
            }
            Stat::While { body, .. }
            | Stat::Repeat { body, .. }
            | Stat::NumericFor { body, .. }
            | Stat::GenericFor { body, .. }
            | Stat::DoBlock { body } => {
                drop_stdlib_function_lvalue_artifacts(body);
            }
            Stat::LocalFunction { func, .. } | Stat::MethodFunction { func, .. } => {
                if let Expr::Function { body, .. } = func {
                    drop_stdlib_function_lvalue_artifacts(body);
                }
            }
            Stat::Local { values, .. } | Stat::Assign { values, .. } => {
                for v in values.iter_mut() {
                    if let Expr::Function { body, .. } = v {
                        drop_stdlib_function_lvalue_artifacts(body);
                    }
                }
            }
            _ => {}
        }
    }

    // Walk both Field and Index chains back to their Name root.
    // `require.X[v245] = v` has target Expr::Index { object: Expr::Field { object: Name("require") } }
    // so we must recurse through Index too.
    fn field_root_is_shadow(e: &Expr) -> bool {
        match e {
            Expr::Name(n) => is_stdlib_function_only_shadow(n),
            Expr::Field { object, .. } | Expr::Index { object, .. } => {
                field_root_is_shadow(object)
            }
            _ => false,
        }
    }

    stmts.retain(|s| match s {
        Stat::Assign { targets, .. } if targets.len() == 1 => {
            !matches!(&targets[0], Expr::Field { .. } | Expr::Index { .. }
                if field_root_is_shadow(&targets[0]))
        }
        Stat::MethodFunction { receiver, .. } => {
            !matches!(receiver, Expr::Name(n) if is_stdlib_function_only_shadow(n))
        }
        _ => true,
    });
}

/// B0.60 — heuristic: is `param` used at least twice as an object-position
/// receiver in the body? Looks for `param.field` and `param:method(...)`.
/// Two or more uses is a strong signal that the caller intended this as a
/// method and the first param is `self`.
fn is_self_used_as_receiver(body: &[Stat], param: &str) -> bool {
    let mut count = 0usize;
    count_receiver_uses_in_stmts(body, param, &mut count);
    count >= 2
}

/// B0.60 — decide whether to emit a function as Luau method syntax
/// (`function Base:X(...) end`). Two branches:
///
/// 1. **First param literally named `self`** — clear signal that the source
///    used method syntax; emit as method.
/// 2. **First param is generic (e.g. `Players2`, `arg1`) but the body uses
///    `self.field` / `self:method(...)` 2+ times AND never references the
///    first param by its name.** This is the Roblox-module idiom where the
///    bytecode encodes `function(self, ...)` but the hint system gave the
///    self register a misleading name inherited from a parent proto. In
///    this case the first param IS the self; emit.rs will strip it when
///    rendering method syntax, and the body's free `self.*` references
///    will resolve to the implicit self.
fn should_emit_as_method(params: &[String], body: &[Stat]) -> bool {
    let Some(first) = params.first() else { return false; };
    if first == "self" {
        return is_self_used_as_receiver(body, "self");
    }
    // Case B: the first param is generic but body looks method-y.
    let self_receiver_uses = {
        let mut c = 0usize;
        count_receiver_uses_in_stmts(body, "self", &mut c);
        c
    };
    if self_receiver_uses < 2 { return false; }
    // Conservative: if the first param is referenced ANYWHERE in the body
    // (not just as a receiver), the param has actual meaning and we can't
    // safely drop it via method syntax. Leave as field-function.
    let first_any_uses = {
        let mut c = 0usize;
        count_name_uses_in_stmts(body, first, &mut c);
        c
    };
    first_any_uses == 0
}

fn count_name_uses_in_stmts(stmts: &[Stat], name: &str, count: &mut usize) {
    for s in stmts { count_name_uses_in_stmt(s, name, count); }
}

fn count_name_uses_in_stmt(stmt: &Stat, name: &str, count: &mut usize) {
    match stmt {
        Stat::Local { values, .. } | Stat::Assign { values, .. } | Stat::Return { values } => {
            for v in values { count_name_uses_in_expr(v, name, count); }
        }
        Stat::ExprStat(e) => count_name_uses_in_expr(e, name, count),
        Stat::If { condition, then_body, elseif_clauses, else_body, .. } => {
            count_name_uses_in_expr(condition, name, count);
            count_name_uses_in_stmts(then_body, name, count);
            for (c, b) in elseif_clauses {
                count_name_uses_in_expr(c, name, count);
                count_name_uses_in_stmts(b, name, count);
            }
            if let Some(eb) = else_body { count_name_uses_in_stmts(eb, name, count); }
        }
        Stat::While { condition, body } | Stat::Repeat { condition, body } => {
            count_name_uses_in_expr(condition, name, count);
            count_name_uses_in_stmts(body, name, count);
        }
        Stat::NumericFor { start, stop, step, body, .. } => {
            count_name_uses_in_expr(start, name, count);
            count_name_uses_in_expr(stop, name, count);
            if let Some(s) = step { count_name_uses_in_expr(s, name, count); }
            count_name_uses_in_stmts(body, name, count);
        }
        Stat::GenericFor { iterators, body, .. } => {
            for e in iterators { count_name_uses_in_expr(e, name, count); }
            count_name_uses_in_stmts(body, name, count);
        }
        Stat::DoBlock { body } => count_name_uses_in_stmts(body, name, count),
        Stat::LocalFunction { func, .. } | Stat::MethodFunction { func, .. } => {
            count_name_uses_in_expr(func, name, count);
        }
        _ => {}
    }
}

fn count_name_uses_in_expr(expr: &Expr, name: &str, count: &mut usize) {
    match expr {
        Expr::Name(n) => if n == name { *count += 1; },
        Expr::Field { object, .. } => count_name_uses_in_expr(object, name, count),
        Expr::Index { object, key } => {
            count_name_uses_in_expr(object, name, count);
            count_name_uses_in_expr(key, name, count);
        }
        Expr::BinOp { left, right, .. } => {
            count_name_uses_in_expr(left, name, count);
            count_name_uses_in_expr(right, name, count);
        }
        Expr::UnOp { operand, .. } => count_name_uses_in_expr(operand, name, count),
        Expr::Call { func, args } => {
            count_name_uses_in_expr(func, name, count);
            for a in args { count_name_uses_in_expr(a, name, count); }
        }
        Expr::MethodCall { object, args, .. } => {
            count_name_uses_in_expr(object, name, count);
            for a in args { count_name_uses_in_expr(a, name, count); }
        }
        Expr::Function { body, .. } => count_name_uses_in_stmts(body, name, count),
        Expr::Table { fields } => {
            for f in fields {
                match f {
                    crate::ast::TableField::Sequential(e) => count_name_uses_in_expr(e, name, count),
                    crate::ast::TableField::Named(_, e) => count_name_uses_in_expr(e, name, count),
                    crate::ast::TableField::Indexed(k, v) => {
                        count_name_uses_in_expr(k, name, count);
                        count_name_uses_in_expr(v, name, count);
                    }
                }
            }
        }
        Expr::Ternary { cond, then_expr, else_expr } => {
            count_name_uses_in_expr(cond, name, count);
            count_name_uses_in_expr(then_expr, name, count);
            count_name_uses_in_expr(else_expr, name, count);
        }
        _ => {}
    }
}

fn count_receiver_uses_in_stmts(stmts: &[Stat], param: &str, count: &mut usize) {
    for stmt in stmts {
        count_receiver_uses_in_stmt(stmt, param, count);
    }
}

fn count_receiver_uses_in_stmt(stmt: &Stat, param: &str, count: &mut usize) {
    match stmt {
        Stat::Local { values, .. } | Stat::Assign { values, .. } | Stat::Return { values } => {
            for v in values { count_receiver_uses_in_expr(v, param, count); }
        }
        Stat::ExprStat(e) => count_receiver_uses_in_expr(e, param, count),
        Stat::If { condition, then_body, elseif_clauses, else_body, .. } => {
            count_receiver_uses_in_expr(condition, param, count);
            count_receiver_uses_in_stmts(then_body, param, count);
            for (c, b) in elseif_clauses {
                count_receiver_uses_in_expr(c, param, count);
                count_receiver_uses_in_stmts(b, param, count);
            }
            if let Some(eb) = else_body { count_receiver_uses_in_stmts(eb, param, count); }
        }
        Stat::While { condition, body } | Stat::Repeat { condition, body } => {
            count_receiver_uses_in_expr(condition, param, count);
            count_receiver_uses_in_stmts(body, param, count);
        }
        Stat::NumericFor { start, stop, step, body, .. } => {
            count_receiver_uses_in_expr(start, param, count);
            count_receiver_uses_in_expr(stop, param, count);
            if let Some(s) = step { count_receiver_uses_in_expr(s, param, count); }
            count_receiver_uses_in_stmts(body, param, count);
        }
        Stat::GenericFor { iterators, body, .. } => {
            for e in iterators { count_receiver_uses_in_expr(e, param, count); }
            count_receiver_uses_in_stmts(body, param, count);
        }
        Stat::DoBlock { body } => count_receiver_uses_in_stmts(body, param, count),
        Stat::LocalFunction { func, .. } | Stat::MethodFunction { func, .. } => {
            count_receiver_uses_in_expr(func, param, count);
        }
        Stat::Break | Stat::Continue | Stat::Comment(_) => {}
    }
}

fn count_receiver_uses_in_expr(expr: &Expr, param: &str, count: &mut usize) {
    match expr {
        Expr::Field { object, .. } => {
            if let Expr::Name(n) = object.as_ref() {
                if n == param { *count += 1; }
            }
            count_receiver_uses_in_expr(object, param, count);
        }
        Expr::MethodCall { object, args, .. } => {
            if let Expr::Name(n) = object.as_ref() {
                if n == param { *count += 1; }
            }
            count_receiver_uses_in_expr(object, param, count);
            for a in args { count_receiver_uses_in_expr(a, param, count); }
        }
        Expr::Index { object, key } => {
            count_receiver_uses_in_expr(object, param, count);
            count_receiver_uses_in_expr(key, param, count);
        }
        Expr::BinOp { left, right, .. } => {
            count_receiver_uses_in_expr(left, param, count);
            count_receiver_uses_in_expr(right, param, count);
        }
        Expr::UnOp { operand, .. } => count_receiver_uses_in_expr(operand, param, count),
        Expr::Call { func, args } => {
            count_receiver_uses_in_expr(func, param, count);
            for a in args { count_receiver_uses_in_expr(a, param, count); }
        }
        Expr::Function { body, .. } => count_receiver_uses_in_stmts(body, param, count),
        Expr::Table { fields } => {
            for f in fields {
                match f {
                    crate::ast::TableField::Sequential(e) => count_receiver_uses_in_expr(e, param, count),
                    crate::ast::TableField::Named(_, e) => count_receiver_uses_in_expr(e, param, count),
                    crate::ast::TableField::Indexed(k, v) => {
                        count_receiver_uses_in_expr(k, param, count);
                        count_receiver_uses_in_expr(v, param, count);
                    }
                }
            }
        }
        Expr::Ternary { cond, then_expr, else_expr } => {
            count_receiver_uses_in_expr(cond, param, count);
            count_receiver_uses_in_expr(then_expr, param, count);
            count_receiver_uses_in_expr(else_expr, param, count);
        }
        _ => {}
    }
}

/// Phase B0.93c: collapse `if cond then return true else return false end`
/// into `return cond` (and the inverse into `return not cond`).
///
/// Also handles the two-statement variant:
///   `if cond then return true end; return false` → `return cond`
///   `if cond then return false end; return true` → `return not cond`
///
/// This is common in Roblox predicate functions (e.g., `IsAlive`, `HasItem`,
/// `CanAfford`) where the compiler emits a conditional jump to two Return
/// branches rather than computing and returning the boolean directly.
///
/// Safety: only fires when both branches are pure `return <bool-literal>` with
/// no other statements in the bodies. The condition is preserved as-is.
pub(super) fn collapse_if_return_bool(stmts: &mut Vec<Stat>) {
    // Recurse into nested blocks first.
    for stmt in stmts.iter_mut() {
        match stmt {
            Stat::If { then_body, elseif_clauses, else_body, .. } => {
                collapse_if_return_bool(then_body);
                for (_, body) in elseif_clauses.iter_mut() {
                    collapse_if_return_bool(body);
                }
                if let Some(eb) = else_body {
                    collapse_if_return_bool(eb);
                }
            }
            Stat::While { body, .. }
            | Stat::Repeat { body, .. }
            | Stat::DoBlock { body }
            | Stat::NumericFor { body, .. }
            | Stat::GenericFor { body, .. } => {
                collapse_if_return_bool(body);
            }
            Stat::LocalFunction { func, .. } | Stat::MethodFunction { func, .. } => {
                if let Expr::Function { body, .. } = func {
                    collapse_if_return_bool(body);
                }
            }
            _ => {}
        }
    }

    let mut i = 0;
    while i < stmts.len() {
        // Pattern A: `if cond then return true else return false end`
        //         or `if cond then return false else return true end`
        if let Stat::If { condition, then_body, elseif_clauses, else_body } = &stmts[i] {
            if elseif_clauses.is_empty() {
                if let Some(else_body) = else_body {
                    if let (Some(then_bool), Some(else_bool)) =
                        (single_return_bool(then_body), single_return_bool(else_body))
                    {
                        if then_bool && !else_bool {
                            // if cond then return true else return false end → return cond
                            stmts[i] = Stat::Return { values: vec![condition.clone()] };
                            continue;
                        } else if !then_bool && else_bool {
                            // if cond then return false else return true end → return not cond
                            stmts[i] = Stat::Return {
                                values: vec![Expr::UnOp {
                                    op: UnOp::Not,
                                    operand: Box::new(condition.clone()),
                                }],
                            };
                            continue;
                        }
                    }
                }
            }
        }

        // Pattern B: `if cond then return <bool> end; return <opposite-bool>`
        if i + 1 < stmts.len() {
            if let Stat::If { condition, then_body, elseif_clauses, else_body } = &stmts[i] {
                if elseif_clauses.is_empty() && else_body.is_none() {
                    if let Some(then_bool) = single_return_bool(then_body) {
                        if let Stat::Return { values } = &stmts[i + 1] {
                            if values.len() == 1 {
                                if let Expr::Bool(next_bool) = &values[0] {
                                    if then_bool && !next_bool {
                                        // if cond then return true end; return false → return cond
                                        stmts[i] = Stat::Return { values: vec![condition.clone()] };
                                        stmts.remove(i + 1);
                                        continue;
                                    } else if !then_bool && *next_bool {
                                        // if cond then return false end; return true → return not cond
                                        stmts[i] = Stat::Return {
                                            values: vec![Expr::UnOp {
                                                op: UnOp::Not,
                                                operand: Box::new(condition.clone()),
                                            }],
                                        };
                                        stmts.remove(i + 1);
                                        continue;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        i += 1;
    }
}

/// Helper: check if a body is exactly `[Return { values: [Bool(b)] }]`.
fn single_return_bool(body: &[Stat]) -> Option<bool> {
    if body.len() != 1 { return None; }
    match &body[0] {
        Stat::Return { values } if values.len() == 1 => {
            if let Expr::Bool(b) = &values[0] {
                Some(*b)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Phase B0.95: collapse `if cond then X = true else X = false end`
/// into `X = cond` (and the inverse into `X = not cond`).
///
/// Also handles the two-statement variant:
///   `if cond then X = true end; X = false` → `X = cond`
///   `if cond then X = false end; X = true` → `X = not cond`
///
/// Unlike `collapse_if_return_bool` (which handles return), this handles
/// assignment-to-same-target patterns. The short-circuit collapse (B0.42)
/// would convert these to `X = cond and true or false`, but that expression
/// doesn't simplify because `cond and true` ≠ `cond` in general (e.g.
/// `5 and true` = `true`, not `5`). At the statement level, however, the
/// intent is clearly boolean assignment, so direct collapse is correct.
pub(super) fn collapse_if_assign_bool(stmts: &mut Vec<Stat>) {
    // Recurse into nested blocks first.
    for stmt in stmts.iter_mut() {
        match stmt {
            Stat::If { then_body, elseif_clauses, else_body, .. } => {
                collapse_if_assign_bool(then_body);
                for (_, body) in elseif_clauses.iter_mut() {
                    collapse_if_assign_bool(body);
                }
                if let Some(eb) = else_body {
                    collapse_if_assign_bool(eb);
                }
            }
            Stat::While { body, .. }
            | Stat::Repeat { body, .. }
            | Stat::DoBlock { body }
            | Stat::NumericFor { body, .. }
            | Stat::GenericFor { body, .. } => {
                collapse_if_assign_bool(body);
            }
            Stat::LocalFunction { func, .. } | Stat::MethodFunction { func, .. } => {
                if let Expr::Function { body, .. } = func {
                    collapse_if_assign_bool(body);
                }
            }
            _ => {}
        }
    }

    let mut i = 0;
    while i < stmts.len() {
        // Pattern A: `if cond then X = true else X = false end`
        //         or `if cond then X = false else X = true end`
        if let Stat::If { condition, then_body, elseif_clauses, else_body } = &stmts[i] {
            if elseif_clauses.is_empty() {
                if let Some(else_body) = else_body {
                    if let (Some((then_name, then_bool)), Some((else_name, else_bool))) =
                        (single_assign_bool(then_body), single_assign_bool(else_body))
                    {
                        if then_name == else_name && then_bool != else_bool {
                            let expr = if then_bool {
                                // if cond then X = true else X = false end → X = cond
                                condition.clone()
                            } else {
                                // if cond then X = false else X = true end → X = not cond
                                Expr::UnOp {
                                    op: UnOp::Not,
                                    operand: Box::new(condition.clone()),
                                }
                            };
                            stmts[i] = Stat::Assign {
                                targets: vec![Expr::Name(then_name)],
                                values: vec![expr],
                            };
                            continue;
                        }
                    }
                }
            }
        }

        // Pattern B: `if cond then X = <bool> end; X = <opposite-bool>`
        if i + 1 < stmts.len() {
            if let Stat::If { condition, then_body, elseif_clauses, else_body } = &stmts[i] {
                if elseif_clauses.is_empty() && else_body.is_none() {
                    if let Some((then_name, then_bool)) = single_assign_bool(then_body) {
                        if let Some((next_name, next_bool)) = assign_bool_stmt(&stmts[i + 1]) {
                            if then_name == next_name && then_bool != next_bool {
                                let expr = if then_bool {
                                    condition.clone()
                                } else {
                                    Expr::UnOp {
                                        op: UnOp::Not,
                                        operand: Box::new(condition.clone()),
                                    }
                                };
                                stmts[i] = Stat::Assign {
                                    targets: vec![Expr::Name(then_name)],
                                    values: vec![expr],
                                };
                                stmts.remove(i + 1);
                                continue;
                            }
                        }
                    }
                }
            }
        }

        i += 1;
    }
}

/// Helper: check if a body is exactly `[Assign { targets: [Name(n)], values: [Bool(b)] }]`.
fn single_assign_bool(body: &[Stat]) -> Option<(String, bool)> {
    if body.len() != 1 { return None; }
    assign_bool_stmt(&body[0])
}

/// Helper: check if a statement is `Assign { targets: [Name(n)], values: [Bool(b)] }`.
fn assign_bool_stmt(stmt: &Stat) -> Option<(String, bool)> {
    match stmt {
        Stat::Assign { targets, values } if targets.len() == 1 && values.len() == 1 => {
            if let (Expr::Name(n), Expr::Bool(b)) = (&targets[0], &values[0]) {
                Some((n.clone(), *b))
            } else {
                None
            }
        }
        _ => None,
    }
}

// ═══════════════════════════════════════════════════════════════════
// Phase B0.97 — COLLAPSE IF/ELSE RETURN TO TERNARY RETURN
// ═══════════════════════════════════════════════════════════════════
//
// Converts:
//   Pattern A: `if cond then return a else return b end`
//            → `return if cond then a else b`
//
//   Pattern B: `if cond then return a end; return b`
//            → `return if cond then a else b`
//
// Constraints:
//   - No elseif clauses.
//   - Both returns must have exactly 1 value (multi-return can't be
//     represented as a single ternary expression).
//   - Must run AFTER `collapse_if_return_bool` so boolean-specific
//     simplifications (`return cond` / `return not cond`) fire first.
//   - Recurses into all nested block types.

/// Helper: if body is `[Return { values: [expr] }]`, return the expr.
fn single_return_expr(body: &[Stat]) -> Option<&Expr> {
    if body.len() != 1 { return None; }
    match &body[0] {
        Stat::Return { values } if values.len() == 1 => Some(&values[0]),
        _ => None,
    }
}

pub(super) fn collapse_if_return_ternary(stmts: &mut Vec<Stat>) {
    // Recurse into nested blocks first.
    for stmt in stmts.iter_mut() {
        match stmt {
            Stat::If { then_body, elseif_clauses, else_body, .. } => {
                collapse_if_return_ternary(then_body);
                for (_, body) in elseif_clauses.iter_mut() {
                    collapse_if_return_ternary(body);
                }
                if let Some(eb) = else_body {
                    collapse_if_return_ternary(eb);
                }
            }
            Stat::While { body, .. }
            | Stat::Repeat { body, .. }
            | Stat::DoBlock { body }
            | Stat::NumericFor { body, .. }
            | Stat::GenericFor { body, .. } => {
                collapse_if_return_ternary(body);
            }
            Stat::LocalFunction { func, .. } | Stat::MethodFunction { func, .. } => {
                if let Expr::Function { body, .. } = func {
                    collapse_if_return_ternary(body);
                }
            }
            _ => {}
        }
    }

    let mut i = 0;
    while i < stmts.len() {
        // Pattern A: `if cond then return a else return b end`
        if let Stat::If { condition, then_body, elseif_clauses, else_body } = &stmts[i] {
            if elseif_clauses.is_empty() {
                if let Some(eb) = else_body {
                    if let (Some(then_val), Some(else_val)) =
                        (single_return_expr(then_body), single_return_expr(eb))
                    {
                        let ternary = Expr::Ternary {
                            cond: Box::new(condition.clone()),
                            then_expr: Box::new(then_val.clone()),
                            else_expr: Box::new(else_val.clone()),
                        };
                        stmts[i] = Stat::Return { values: vec![ternary] };
                        // Don't advance — the new return might enable further opts
                        continue;
                    }
                }
            }
        }

        // Pattern B: `if cond then return a end; return b`
        if i + 1 < stmts.len() {
            if let Stat::If { condition, then_body, elseif_clauses, else_body } = &stmts[i] {
                if elseif_clauses.is_empty() && else_body.is_none() {
                    if let Some(then_val) = single_return_expr(then_body) {
                        if let Stat::Return { values } = &stmts[i + 1] {
                            if values.len() == 1 {
                                let ternary = Expr::Ternary {
                                    cond: Box::new(condition.clone()),
                                    then_expr: Box::new(then_val.clone()),
                                    else_expr: Box::new(values[0].clone()),
                                };
                                stmts[i] = Stat::Return { values: vec![ternary] };
                                stmts.remove(i + 1);
                                continue;
                            }
                        }
                    }
                }
            }
        }

        i += 1;
    }
}

// ═══════════════════════════════════════════════════════════════════
// Phase C2 — convert `T.m = function(self, ...)` to `function T:m(...)`
// ═══════════════════════════════════════════════════════════════════

/// Phase C2 pass #5 — method-notation conversion.
///
/// When a function is emitted as `T.m = function(self, ...)` (the Assign-
/// with-Field-target shape) AND its body uses `self.x` or `self:y()` at
/// least twice, rewrite the declaration to idiomatic Luau method syntax:
/// `function T:m(...)`.
///
/// Walks recursively into nested function bodies and control-flow bodies.
/// Only fires for:
///   * `Stat::Assign` with a single `Field(_, _)` target whose value is an
///     `Expr::Function` with `self` as the first parameter AND the body
///     references `self` as a receiver at least twice.
///
/// The Local shape (`local f = function(self, ...)`) is not rewritten —
/// there is no receiver to place before `:m`, so method-sugar doesn't
/// apply. We still recurse into its body.
///
/// Skipped:
///   * Index targets (`T[m] = function` — dynamic key)
///   * Anonymous functions (inside an `ExprStat`, argument, etc.)
///   * Functions where `self` is not the first param
///   * Bodies with fewer than 2 self-receiver uses
pub(super) fn convert_dot_to_method_function(stmts: &mut Vec<Stat>) {
    // Recurse first so inner patterns convert before outer ones.
    for stmt in stmts.iter_mut() {
        match stmt {
            Stat::If { then_body, elseif_clauses, else_body, .. } => {
                convert_dot_to_method_function(then_body);
                for (_, body) in elseif_clauses.iter_mut() {
                    convert_dot_to_method_function(body);
                }
                if let Some(eb) = else_body {
                    convert_dot_to_method_function(eb);
                }
            }
            Stat::While { body, .. }
            | Stat::Repeat { body, .. }
            | Stat::NumericFor { body, .. }
            | Stat::GenericFor { body, .. }
            | Stat::DoBlock { body } => {
                convert_dot_to_method_function(body);
            }
            Stat::LocalFunction { func, .. } | Stat::MethodFunction { func, .. } => {
                if let Expr::Function { body, .. } = func {
                    convert_dot_to_method_function(body);
                }
            }
            Stat::Local { values, .. } | Stat::Assign { values, .. } => {
                for v in values.iter_mut() {
                    if let Expr::Function { body, .. } = v {
                        convert_dot_to_method_function(body);
                    }
                }
            }
            _ => {}
        }
    }

    // Convert matching Assign-with-Field-target statements in place.
    for stmt in stmts.iter_mut() {
        let (receiver, method) = match stmt {
            Stat::Assign { targets, values }
                if targets.len() == 1 && values.len() == 1 =>
            {
                let is_func_with_self = matches!(
                    &values[0],
                    Expr::Function { params, .. }
                        if params.first().map_or(false, |p| p == "self")
                );
                if !is_func_with_self {
                    continue;
                }
                match &targets[0] {
                    Expr::Field { object, field } => {
                        ((**object).clone(), field.clone())
                    }
                    // Index targets intentionally excluded (dynamic key).
                    _ => continue,
                }
            }
            _ => continue,
        };

        // Count self-receiver uses in the body.
        let self_uses = match stmt {
            Stat::Assign { values, .. } => match &values[0] {
                Expr::Function { body, .. } => {
                    let mut c = 0usize;
                    count_receiver_uses_in_stmts(body, "self", &mut c);
                    c
                }
                _ => 0,
            },
            _ => 0,
        };
        if self_uses < 2 {
            continue;
        }

        // Consume the Function expression and rewrite as MethodFunction.
        // Leave `self` in `params[0]` — emit.rs strips the leading param
        // when `is_method=true` (see Stat::MethodFunction emission path).
        let func = match stmt {
            Stat::Assign { values, .. } => std::mem::replace(
                &mut values[0],
                Expr::Nil,
            ),
            _ => continue,
        };

        *stmt = Stat::MethodFunction {
            receiver,
            method,
            is_method: true,
            func,
        };
    }
}

// ═══════════════════════════════════════════════════════════════════
// Phase B0.114 helper — detect name usage as require() argument
// ═══════════════════════════════════════════════════════════════════

/// Returns true if `name` appears as a direct argument to `require()` in `stmt`.
fn stmt_passes_name_to_require(stmt: &Stat, name: &str) -> bool {
    match stmt {
        Stat::Local { values, .. } => values.iter().any(|e| expr_passes_name_to_require(e, name)),
        Stat::Assign { values, .. } => values.iter().any(|e| expr_passes_name_to_require(e, name)),
        Stat::ExprStat(e) => expr_passes_name_to_require(e, name),
        Stat::Return { values } => values.iter().any(|e| expr_passes_name_to_require(e, name)),
        Stat::If { then_body, elseif_clauses, else_body, .. } => {
            then_body.iter().any(|s| stmt_passes_name_to_require(s, name))
                || elseif_clauses.iter().any(|(_, body)| body.iter().any(|s| stmt_passes_name_to_require(s, name)))
                || else_body.as_ref().map_or(false, |eb| eb.iter().any(|s| stmt_passes_name_to_require(s, name)))
        }
        _ => false,
    }
}

/// Returns true if this expression is `require(Name(name))` or contains such a sub-expression.
fn expr_passes_name_to_require(expr: &Expr, name: &str) -> bool {
    match expr {
        Expr::Call { func, args } => {
            let is_require = matches!(&**func, Expr::Name(n) if n == "require");
            if is_require && args.iter().any(|a| matches!(a, Expr::Name(n) if n == name)) {
                return true;
            }
            // Recurse into sub-expressions
            args.iter().any(|a| expr_passes_name_to_require(a, name))
        }
        Expr::MethodCall { object, args, .. } => {
            expr_passes_name_to_require(object, name)
                || args.iter().any(|a| expr_passes_name_to_require(a, name))
        }
        Expr::Field { object, .. } => expr_passes_name_to_require(object, name),
        Expr::Index { object, key } => {
            expr_passes_name_to_require(object, name) || expr_passes_name_to_require(key, name)
        }
        Expr::BinOp { left, right, .. } => {
            expr_passes_name_to_require(left, name) || expr_passes_name_to_require(right, name)
        }
        Expr::UnOp { operand, .. } => expr_passes_name_to_require(operand, name),
        _ => false,
    }
}

// ═══════════════════════════════════════════════════════════════════
// Phase C2 — FOLD MULTI-RETURN UNPACK into single multi-target assign
// ═══════════════════════════════════════════════════════════════════
//
// Lifts the common pattern produced when a multi-return call's results are
// scattered into separate temp locals and then re-homed into field/index
// targets one-by-one:
//
//     local v1, v2, v3 = f(...)        -- multi-return producing N temps
//     x.a = v1                         -- re-home #1
//     x.b = v2                         -- re-home #2
//     x.c = v3                         -- re-home #3
//
// Fold into the idiomatic Luau form:
//
//     x.a, x.b, x.c = f(...)
//
// The fold is gated on strict conditions so we never change observable
// behavior:
//
//   (1) The `Local` declares exactly N >= 2 names, with values.len() == 1
//       and values[0] is a Call or MethodCall (multi-return producers).
//   (2) The next N statements (strictly sequential, no intervening
//       statements) are all single-target / single-value `Assign`s whose
//       RHS is `Expr::Name(vK)` matching the K-th local name in order.
//   (3) Each temp `vK` has exactly one read in the entire block — the
//       corresponding Assign — verified via `count_name_reads_in_stmt`.
//
// Runs AFTER `inline_single_use_temps` so that pass has a chance to
// operate on its preferred one-to-one patterns first; any surviving
// multi-return cluster is what this pass targets.
pub(super) fn fold_multireturn_unpack(stmts: &mut Vec<Stat>) {
    // Recurse into nested blocks first.
    for stmt in stmts.iter_mut() {
        match stmt {
            Stat::If { then_body, elseif_clauses, else_body, .. } => {
                fold_multireturn_unpack(then_body);
                for (_, body) in elseif_clauses {
                    fold_multireturn_unpack(body);
                }
                if let Some(eb) = else_body { fold_multireturn_unpack(eb); }
            }
            Stat::While { body, .. } => fold_multireturn_unpack(body),
            Stat::Repeat { body, .. } => fold_multireturn_unpack(body),
            Stat::NumericFor { body, .. } => fold_multireturn_unpack(body),
            Stat::GenericFor { body, .. } => fold_multireturn_unpack(body),
            Stat::DoBlock { body } => fold_multireturn_unpack(body),
            Stat::LocalFunction { func, .. } | Stat::MethodFunction { func, .. } => {
                if let Expr::Function { body, .. } = func {
                    fold_multireturn_unpack(body);
                }
            }
            _ => {}
        }
    }

    let mut i = 0;
    while i < stmts.len() {
        // (1) Find a `local v1, ..., vN = call(...)` with N >= 2.
        let (temp_names, call_expr) = match &stmts[i] {
            Stat::Local { names, values }
                if names.len() >= 2
                    && values.len() == 1
                    && matches!(&values[0], Expr::Call { .. } | Expr::MethodCall { .. }) =>
            {
                (names.clone(), values[0].clone())
            }
            _ => { i += 1; continue; }
        };

        let n = temp_names.len();

        // (2) Need N following statements available strictly after `i`.
        if i + n >= stmts.len() {
            i += 1;
            continue;
        }

        // Each of the next N must be `Assign { targets: [lhs], values: [Name(vK)] }`
        // with the K-th local name in order.
        let mut lhs_targets: Vec<Expr> = Vec::with_capacity(n);
        let mut pattern_ok = true;
        for k in 0..n {
            let s = &stmts[i + 1 + k];
            match s {
                Stat::Assign { targets, values }
                    if targets.len() == 1 && values.len() == 1 =>
                {
                    match &values[0] {
                        Expr::Name(n2) if n2 == &temp_names[k] => {
                            lhs_targets.push(targets[0].clone());
                        }
                        _ => { pattern_ok = false; break; }
                    }
                }
                _ => { pattern_ok = false; break; }
            }
        }
        if !pattern_ok {
            i += 1;
            continue;
        }

        // (3) Each temp must be referenced exactly once across the whole
        // block. The one read we expect is the `values[0] = Name(vK)` entry
        // in its corresponding Assign; anything else (reader before the
        // Local, extra reader after the unpack tail, or an LHS that itself
        // references the temp) aborts the fold.
        let mut counts_ok = true;
        for (k, tname) in temp_names.iter().enumerate() {
            let mut total = 0usize;
            for s in stmts.iter() {
                total += count_name_reads_in_stmt(s, tname);
                if total > 1 { break; }
            }
            if total != 1 {
                counts_ok = false;
                break;
            }
            // Also defend against the (unlikely) case where the single read
            // lives inside the LHS of our own Assign rather than the RHS
            // Name slot. count_name_reads_in_stmt would count that read, but
            // we'd then lose it by replacing the Assign with a multi-target
            // fold. Ensure the one read is actually the RHS Name we matched.
            if let Stat::Assign { targets, .. } = &stmts[i + 1 + k] {
                for t in targets {
                    if count_name_reads_in_expr_local(t, tname) > 0 {
                        counts_ok = false;
                        break;
                    }
                }
            }
            if !counts_ok { break; }
        }
        if !counts_ok {
            i += 1;
            continue;
        }

        // Fold: replace stmts[i..=i+n] with a single multi-target Assign.
        let folded = Stat::Assign {
            targets: lhs_targets,
            values: vec![call_expr],
        };
        stmts.splice(i..i + 1 + n, std::iter::once(folded));
        i += 1;
    }
}

/// Local helper: count reads of `name` in an expression.  Duplicates the
/// logic of the lifter's private `count_name_reads_in_expr` so the multi-
/// return fold pass can verify that no temp sneaks into an LHS root.
fn count_name_reads_in_expr_local(expr: &Expr, name: &str) -> usize {
    match expr {
        Expr::Name(n) => if n == name { 1 } else { 0 },
        Expr::Field { object, .. } => count_name_reads_in_expr_local(object, name),
        Expr::Index { object, key } => {
            count_name_reads_in_expr_local(object, name)
                + count_name_reads_in_expr_local(key, name)
        }
        Expr::BinOp { left, right, .. } => {
            count_name_reads_in_expr_local(left, name)
                + count_name_reads_in_expr_local(right, name)
        }
        Expr::UnOp { operand, .. } => count_name_reads_in_expr_local(operand, name),
        Expr::Call { func, args } => {
            count_name_reads_in_expr_local(func, name)
                + args.iter().map(|a| count_name_reads_in_expr_local(a, name)).sum::<usize>()
        }
        Expr::MethodCall { object, args, .. } => {
            count_name_reads_in_expr_local(object, name)
                + args.iter().map(|a| count_name_reads_in_expr_local(a, name)).sum::<usize>()
        }
        Expr::Ternary { cond, then_expr, else_expr } => {
            count_name_reads_in_expr_local(cond, name)
                + count_name_reads_in_expr_local(then_expr, name)
                + count_name_reads_in_expr_local(else_expr, name)
        }
        _ => 0,
    }
}

// Phase C10P: rename `local serviceN = game:GetService("ClassName")` locals
// to the class name itself (e.g. `local RunService = game:GetService("RunService")`).
// Upstream naming (mod.rs:LITERAL_NAMING_METHODS) is supposed to propagate the
// string arg into the register name, but ~1112 corpus locals still surface as
// `serviceN` (generic "service" helper-prefix). This post-pass catches the
// survivors by rewriting at AST level.
//
// Scope rules:
//   - Only rewrites when the new name is a valid Luau identifier, is not a
//     reserved word or stdlib shadow, and does not collide with an existing
//     local in the same scope (including other rename targets we're about to
//     apply in this scope).
//   - Renames the Local's own declared name AND all subsequent references in
//     the current scope and nested child scopes (via replace_name_in_stmt).
//   - Recurses into nested function bodies / control-flow blocks so each
//     inner scope runs its own pass independently.
pub(super) fn rename_service_locals(stmts: &mut Vec<Stat>) {
    // Recurse into nested scopes first so outer renames observe the post-
    // recursion state of inner blocks (matches C10O's layout).
    for stmt in stmts.iter_mut() {
        rename_service_locals_in_stmt(stmt);
    }

    // Collect every local name declared anywhere in this scope (incl. nested
    // for our collision check — a nested `local Players = ...` would shadow
    // our rename, which is OK, but we still avoid picking a name that clashes
    // with a same-scope peer).
    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
    collect_declared_local_names(stmts, &mut used);

    let mut renames: Vec<(String, String)> = Vec::new();
    for stmt in stmts.iter() {
        if let Stat::Local { names, values } = stmt {
            if names.len() == 1 && values.len() == 1 {
                let old = &names[0];
                if !is_generic_service_name(old) {
                    continue;
                }
                if let Some(class_name) = extract_getservice_classname(&values[0]) {
                    if used.contains(&class_name) {
                        continue;
                    }
                    if renames.iter().any(|(_, n)| n == &class_name) {
                        continue;
                    }
                    renames.push((old.clone(), class_name.clone()));
                    used.insert(class_name);
                }
            }
        }
    }

    if renames.is_empty() {
        return;
    }

    for (old, new) in &renames {
        let replacement = Expr::Name(new.clone());
        for stmt in stmts.iter_mut() {
            super::replace_name_in_stmt(stmt, old, &replacement);
        }
        // replace_name_in_stmt intentionally doesn't touch Local declaration
        // names (those are binding sites, not references), so rename the
        // binding ourselves.
        for stmt in stmts.iter_mut() {
            rename_local_binding(stmt, old, new);
        }
    }
}

fn rename_service_locals_in_stmt(stmt: &mut Stat) {
    match stmt {
        Stat::If { then_body, elseif_clauses, else_body, .. } => {
            rename_service_locals(then_body);
            for (_, body) in elseif_clauses {
                rename_service_locals(body);
            }
            if let Some(eb) = else_body {
                rename_service_locals(eb);
            }
        }
        Stat::While { body, .. }
        | Stat::Repeat { body, .. }
        | Stat::DoBlock { body }
        | Stat::NumericFor { body, .. }
        | Stat::GenericFor { body, .. } => rename_service_locals(body),
        Stat::Local { values, .. } | Stat::Assign { values, .. } => {
            for v in values {
                rename_service_locals_in_expr(v);
            }
        }
        Stat::Return { values } => {
            for v in values {
                rename_service_locals_in_expr(v);
            }
        }
        Stat::ExprStat(e) => rename_service_locals_in_expr(e),
        _ => {}
    }
}

fn rename_service_locals_in_expr(expr: &mut Expr) {
    match expr {
        Expr::Function { body, .. } => rename_service_locals(body),
        Expr::Call { func, args } => {
            rename_service_locals_in_expr(func);
            for a in args {
                rename_service_locals_in_expr(a);
            }
        }
        Expr::MethodCall { object, args, .. } => {
            rename_service_locals_in_expr(object);
            for a in args {
                rename_service_locals_in_expr(a);
            }
        }
        Expr::Field { object, .. } => rename_service_locals_in_expr(object),
        Expr::Index { object, key } => {
            rename_service_locals_in_expr(object);
            rename_service_locals_in_expr(key);
        }
        Expr::BinOp { left, right, .. } => {
            rename_service_locals_in_expr(left);
            rename_service_locals_in_expr(right);
        }
        Expr::UnOp { operand, .. } => rename_service_locals_in_expr(operand),
        Expr::Ternary { cond, then_expr, else_expr } => {
            rename_service_locals_in_expr(cond);
            rename_service_locals_in_expr(then_expr);
            rename_service_locals_in_expr(else_expr);
        }
        Expr::Table { fields } => {
            for f in fields {
                match f {
                    TableField::Sequential(e) | TableField::Named(_, e) => {
                        rename_service_locals_in_expr(e);
                    }
                    TableField::Indexed(k, v) => {
                        rename_service_locals_in_expr(k);
                        rename_service_locals_in_expr(v);
                    }
                }
            }
        }
        _ => {}
    }
}

fn collect_declared_local_names(
    stmts: &[Stat],
    out: &mut std::collections::HashSet<String>,
) {
    for stmt in stmts {
        match stmt {
            Stat::Local { names, .. } => {
                for n in names {
                    out.insert(n.clone());
                }
            }
            Stat::NumericFor { var, body, .. } => {
                out.insert(var.clone());
                collect_declared_local_names(body, out);
            }
            Stat::GenericFor { vars, body, .. } => {
                for v in vars {
                    out.insert(v.clone());
                }
                collect_declared_local_names(body, out);
            }
            Stat::If { then_body, elseif_clauses, else_body, .. } => {
                collect_declared_local_names(then_body, out);
                for (_, body) in elseif_clauses {
                    collect_declared_local_names(body, out);
                }
                if let Some(eb) = else_body {
                    collect_declared_local_names(eb, out);
                }
            }
            Stat::While { body, .. }
            | Stat::Repeat { body, .. }
            | Stat::DoBlock { body } => collect_declared_local_names(body, out),
            _ => {}
        }
    }
}

fn rename_local_binding(stmt: &mut Stat, old: &str, new: &str) {
    match stmt {
        Stat::Local { names, .. } => {
            for n in names.iter_mut() {
                if n == old {
                    *n = new.to_string();
                }
            }
        }
        Stat::If { then_body, elseif_clauses, else_body, .. } => {
            for s in then_body {
                rename_local_binding(s, old, new);
            }
            for (_, body) in elseif_clauses {
                for s in body {
                    rename_local_binding(s, old, new);
                }
            }
            if let Some(eb) = else_body {
                for s in eb {
                    rename_local_binding(s, old, new);
                }
            }
        }
        Stat::While { body, .. }
        | Stat::Repeat { body, .. }
        | Stat::DoBlock { body }
        | Stat::NumericFor { body, .. }
        | Stat::GenericFor { body, .. } => {
            for s in body {
                rename_local_binding(s, old, new);
            }
        }
        _ => {}
    }
}

fn is_generic_service_name(name: &str) -> bool {
    // "service" or "serviceN" where N is all digits (from helper_name_for
    // "GetService" => "service" in mod.rs).
    if let Some(rest) = name.strip_prefix("service") {
        rest.is_empty() || rest.chars().all(|c| c.is_ascii_digit())
    } else {
        false
    }
}

fn extract_getservice_classname(expr: &Expr) -> Option<String> {
    let (object, method, args) = match expr {
        Expr::MethodCall { object, method, args } => (object.as_ref(), method, args),
        _ => return None,
    };
    if method != "GetService" {
        return None;
    }
    // Phase C10T: accept any receiver that could plausibly expose a
    // :GetService method on a Roblox DataModel-ish object. In real source
    // the receiver is one of:
    //   game              / Name("game")
    //   UserSettings      / Name stdlib-global (Roblox)
    //   usersettings      / Name local holding UserSettings() result
    //   UserSettings()    / Call { callee: Name("UserSettings"), args: [] }
    //   game.Parent etc.  / Field chain rooted at game
    //
    // The *class name* argument is what matters for the rename — the
    // receiver is just "some object that exposes GetService". Rejecting
    // non-DataModel receivers was overly cautious: if the method name is
    // literally `GetService` and the argument is a string that's a valid
    // identifier, the local is almost certainly meant to hold the service.
    // False-positive risk: a user-defined :GetService method on an unrelated
    // class — acceptable collateral given Roblox convention.
    fn is_plausible_getservice_receiver(e: &Expr) -> bool {
        match e {
            Expr::Name(_) => true,
            Expr::Call { func, .. } => is_plausible_getservice_receiver(func),
            Expr::Field { object, .. } => is_plausible_getservice_receiver(object),
            _ => false,
        }
    }
    if !is_plausible_getservice_receiver(object) {
        return None;
    }
    if args.len() != 1 {
        return None;
    }
    let s = match &args[0] {
        Expr::String(s) => s,
        _ => return None,
    };
    if !crate::decompiler::is_valid_luau_identifier(s) {
        return None;
    }
    Some(s.clone())
}

#[cfg(test)]
mod c10o_tests {
    use super::*;
    use crate::ast::{Expr, Stat, TableField};

    fn local_wrapper(name: &str, field: &str, inner_name: &str) -> Stat {
        Stat::Local {
            names: vec![name.to_string()],
            values: vec![Expr::Table {
                fields: vec![TableField::Named(
                    field.to_string(),
                    Expr::Name(inner_name.to_string()),
                )],
            }],
        }
    }

    fn local_require(result: &str, arg: &str) -> Stat {
        Stat::Local {
            names: vec![result.to_string()],
            values: vec![Expr::Call {
                func: Box::new(Expr::Name("require".to_string())),
                args: vec![Expr::Name(arg.to_string())],
            }],
        }
    }

    fn assign_field(obj: &str, field: &str, value: &str) -> Stat {
        Stat::Assign {
            targets: vec![Expr::Field {
                object: Box::new(Expr::Name(obj.to_string())),
                field: field.to_string(),
            }],
            values: vec![Expr::Name(value.to_string())],
        }
    }

    /// Mirrors WorldMap.lua lines 120-134: interleaved
    /// `v1.Shared = vN; local resultM = {K=vN}; local resultM2 = require(resultM)`.
    #[test]
    fn worldmap_interleaved_wrappers_unwrap() {
        let mut stmts = vec![
            assign_field("v1", "Shared", "v17"),
            local_wrapper("result24", "Framework", "v17"),
            local_require("result25", "result24"),
            assign_field("v1", "Client", "v18"),
            local_wrapper("result26", "Gui", "v18"),
            local_require("result27", "result26"),
            assign_field("v1", "Shared", "v19"),
            local_wrapper("result28", "Framework", "v19"),
            local_require("result29", "result28"),
        ];

        unwrap_require_wrapper_locals(&mut stmts);

        // After C10O: each wrapper-local should be removed; require() arg = inner Name.
        // Remaining stmts: 3× Assign + 3× `local resultN = require(vM)`.
        assert_eq!(stmts.len(), 6, "expected 6 stmts, got {}: {:#?}", stmts.len(), stmts);

        // Position 1: local result25 = require(v17)
        match &stmts[1] {
            Stat::Local { values, .. } => match &values[0] {
                Expr::Call { args, .. } => {
                    assert_eq!(args.len(), 1);
                    assert!(matches!(&args[0], Expr::Name(n) if n == "v17"), "got {:?}", args[0]);
                }
                other => panic!("expected Call, got {:?}", other),
            },
            other => panic!("expected Local, got {:?}", other),
        }
    }
}

#[cfg(test)]
mod c10p_tests {
    use super::*;
    use crate::ast::{Expr, Stat};

    fn local_service(name: &str, class: &str) -> Stat {
        Stat::Local {
            names: vec![name.to_string()],
            values: vec![Expr::MethodCall {
                object: Box::new(Expr::Name("game".to_string())),
                method: "GetService".to_string(),
                args: vec![Expr::String(class.to_string())],
            }],
        }
    }

    #[test]
    fn renames_service_local_and_uses() {
        let mut stmts = vec![
            local_service("service2", "RunService"),
            Stat::ExprStat(Expr::MethodCall {
                object: Box::new(Expr::Name("service2".to_string())),
                method: "Heartbeat".to_string(),
                args: vec![],
            }),
        ];

        rename_service_locals(&mut stmts);

        match &stmts[0] {
            Stat::Local { names, .. } => assert_eq!(names[0], "RunService"),
            other => panic!("expected Local, got {:?}", other),
        }
        match &stmts[1] {
            Stat::ExprStat(Expr::MethodCall { object, .. }) => {
                assert!(matches!(object.as_ref(), Expr::Name(n) if n == "RunService"));
            }
            other => panic!("expected ExprStat(MethodCall), got {:?}", other),
        }
    }

    #[test]
    fn skips_when_class_name_already_used() {
        // `local Players = ...` already exists; don't clobber it.
        let mut stmts = vec![
            Stat::Local {
                names: vec!["Players".to_string()],
                values: vec![Expr::Nil],
            },
            local_service("service1", "Players"),
        ];

        rename_service_locals(&mut stmts);

        match &stmts[1] {
            Stat::Local { names, .. } => assert_eq!(names[0], "service1",
                "rename must NOT fire when target name is already bound in scope"),
            other => panic!("expected Local, got {:?}", other),
        }
    }

    #[test]
    fn skips_duplicate_getservice_with_same_class() {
        // First `service1 = GetService("Players")` renames to Players.
        // Second `service5 = GetService("Players")` would collide → skipped.
        let mut stmts = vec![
            local_service("service1", "Players"),
            local_service("service5", "Players"),
        ];

        rename_service_locals(&mut stmts);

        match &stmts[0] {
            Stat::Local { names, .. } => assert_eq!(names[0], "Players"),
            other => panic!("expected Local, got {:?}", other),
        }
        match &stmts[1] {
            Stat::Local { names, .. } => assert_eq!(names[0], "service5",
                "second duplicate must keep the old name to avoid collision"),
            other => panic!("expected Local, got {:?}", other),
        }
    }

    #[test]
    fn renames_with_non_game_name_receiver() {
        // Phase C10T: non-`game` Name receivers (e.g. `usersettings`) also
        // accepted. The `service\d*` local-name guard + valid-ident class
        // name filter constrain this to real GetService results.
        let mut stmts = vec![Stat::Local {
            names: vec!["service1".to_string()],
            values: vec![Expr::MethodCall {
                object: Box::new(Expr::Name("usersettings".to_string())),
                method: "GetService".to_string(),
                args: vec![Expr::String("UserGameSettings".to_string())],
            }],
        }];

        rename_service_locals(&mut stmts);

        match &stmts[0] {
            Stat::Local { names, .. } => assert_eq!(names[0], "UserGameSettings"),
            other => panic!("expected Local, got {:?}", other),
        }
    }

    #[test]
    fn renames_with_call_receiver() {
        // Phase C10T: `UserSettings():GetService("UserGameSettings")` — the
        // receiver is a Call expression, not a raw Name.
        let mut stmts = vec![Stat::Local {
            names: vec!["service3".to_string()],
            values: vec![Expr::MethodCall {
                object: Box::new(Expr::Call {
                    func: Box::new(Expr::Name("UserSettings".to_string())),
                    args: vec![],
                }),
                method: "GetService".to_string(),
                args: vec![Expr::String("UserGameSettings".to_string())],
            }],
        }];

        rename_service_locals(&mut stmts);

        match &stmts[0] {
            Stat::Local { names, .. } => assert_eq!(names[0], "UserGameSettings"),
            other => panic!("expected Local, got {:?}", other),
        }
    }

    #[test]
    fn renames_with_field_receiver() {
        // Phase C10T: `game.Parent:GetService("Players")` — Field chain root.
        let mut stmts = vec![Stat::Local {
            names: vec!["service7".to_string()],
            values: vec![Expr::MethodCall {
                object: Box::new(Expr::Field {
                    object: Box::new(Expr::Name("game".to_string())),
                    field: "Parent".to_string(),
                }),
                method: "GetService".to_string(),
                args: vec![Expr::String("Players".to_string())],
            }],
        }];

        rename_service_locals(&mut stmts);

        match &stmts[0] {
            Stat::Local { names, .. } => assert_eq!(names[0], "Players"),
            other => panic!("expected Local, got {:?}", other),
        }
    }

    #[test]
    fn skips_non_plausible_receiver() {
        // Number receiver isn't plausibly a DataModel — don't rename.
        let mut stmts = vec![Stat::Local {
            names: vec!["service1".to_string()],
            values: vec![Expr::MethodCall {
                object: Box::new(Expr::Number(42.0)),
                method: "GetService".to_string(),
                args: vec![Expr::String("Players".to_string())],
            }],
        }];

        rename_service_locals(&mut stmts);

        match &stmts[0] {
            Stat::Local { names, .. } => assert_eq!(names[0], "service1"),
            other => panic!("expected Local, got {:?}", other),
        }
    }

    #[test]
    fn renames_inside_nested_function() {
        let mut stmts = vec![Stat::Local {
            names: vec!["fn1".to_string()],
            values: vec![Expr::Function {
                params: vec![],
                is_vararg: false,
                body: vec![
                    local_service("service3", "TweenService"),
                    Stat::ExprStat(Expr::MethodCall {
                        object: Box::new(Expr::Name("service3".to_string())),
                        method: "Create".to_string(),
                        args: vec![],
                    }),
                ],
            }],
        }];

        rename_service_locals(&mut stmts);

        let body = match &stmts[0] {
            Stat::Local { values, .. } => match &values[0] {
                Expr::Function { body, .. } => body,
                other => panic!("expected Function, got {:?}", other),
            },
            other => panic!("expected Local, got {:?}", other),
        };
        match &body[0] {
            Stat::Local { names, .. } => assert_eq!(names[0], "TweenService"),
            other => panic!("expected Local, got {:?}", other),
        }
        match &body[1] {
            Stat::ExprStat(Expr::MethodCall { object, .. }) => {
                assert!(matches!(object.as_ref(), Expr::Name(n) if n == "TweenService"));
            }
            other => panic!("expected ExprStat, got {:?}", other),
        }
    }
}
