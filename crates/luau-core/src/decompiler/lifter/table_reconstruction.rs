//! Table-constructor reconstruction passes.
//!
//! Extracted from `lifter.rs` as part of Phase B0.52P6 (part 3/3).  Folds
//! the bytecode pattern
//!     local M = {}    ;    M.a = ...    ;    M.b = ...    ;    return M
//! into a single expression
//!     local M = { a = ..., b = ... }
//! to match the Luau ModuleScript surface syntax.
//!
//! * `reconstruct_table_constructors`     — the statement-level pass.
//! * `constructor_seed_name`              — detects the empty-table seed.
//! * `field_assign_to_constructor_field`  — converts a single field assign.
//! * `two_step_field_absorb`              — Phase B0.48 nested-binding absorb.
//! * `is_pure_two_step_value`             — purity gate for B0.48.
//! * `is_valid_luau_identifier`           — syntactic identifier check.
//! * `reconstruct_table_constructors_in_expr` — same pass recursively on
//!                                          nested `Function { body }` AST
//!                                          expressions.
//!
//! All items are `pub(super)` so `mod.rs` and sibling submodules can call
//! them.  Two helpers (`count_name_reads_in_stmt`, `expr_references_name`)
//! live in `lifter/mod.rs`; we import them via `super::...`.

use std::collections::HashMap;

use crate::ast::{Expr, Stat, TableField};

use super::expr_references_name;

/// Phase B0.47: Reconstruct `local M = { a = ..., b = ... }` table-constructor
/// expressions from a `local M = {}` statement followed by a CONTIGUOUS run of
/// `M.a = ...; M.b = ...` field-assignment statements.
///
/// This is the canonical Luau ModuleScript pattern — `local M = {}; M.foo = ...
/// M.bar = ...; return M` — which the lifter must materialize as separate
/// statements at the bytecode level (NEWTABLE writes one register, then each
/// SETTABLEKS/SETTABLEN writes a field).  Recovering the constructor form
/// produces dramatically more readable output and eliminates a large class of
/// `v\d+` generic names that survive only as table-storage temporaries.
///
/// Pattern recognised (one local table, contiguous prefix of field assigns):
///
///   local M = {}                ─┐
///   M.foo = expr1                ├ all merged into:
///   M.bar = expr2                │
///   M[3]   = expr3               ┘     local M = {
///                                          foo = expr1,
///                                          bar = expr2,
///                                          [3]   = expr3,
///                                      }
///
/// Constraints:
///   * The starting statement is `Stat::Local { names: [N], values: [Table{[]}] }`
///     OR `Stat::Assign { targets: [Name(N)], values: [Table{[]}] }`.
///   * Every following absorbed statement must be exactly:
///       `Stat::Assign { targets: [Field|Index{obj=Name(N), key=K}],
///                       values: [V] }`
///     where the Field/Index target's object reads `Name(N)` (no nesting like
///     `M.a.b`), and the value `V` must NOT read back from `Name(N)` (circular
///     reference — keep as post-init field assign).
///   * Stop at the first non-matching statement; the prefix is folded.
///   * If no field assigns follow, leave the empty-table local alone.
///
/// Recursion: applied to all nested blocks (then/else/loop bodies) so
/// inner module-style table builds are also reconstructed.
///
/// This pass MUST run before `inline_single_use_temps` because removing
/// the intermediate `M.foo = X` statements changes the read-count of `M`
/// (going from N+1 reads to 1 read may unlock further inlining).
pub(super) fn reconstruct_table_constructors(stmts: &mut Vec<Stat>) {
    // Recurse into nested blocks FIRST so inner constructors get rebuilt
    // before the outer block sees them (matters for `local M = {}; M.a = {};
    // M.a.b = 1` style — though the inner one is at a different scope and
    // doesn't actually fold here, this keeps semantics consistent with the
    // other post-AST passes).
    for stmt in stmts.iter_mut() {
        match stmt {
            Stat::If { then_body, elseif_clauses, else_body, .. } => {
                reconstruct_table_constructors(then_body);
                for (_, body) in elseif_clauses.iter_mut() {
                    reconstruct_table_constructors(body);
                }
                if let Some(eb) = else_body {
                    reconstruct_table_constructors(eb);
                }
            }
            Stat::While { body, .. }
            | Stat::Repeat { body, .. }
            | Stat::DoBlock { body }
            | Stat::NumericFor { body, .. }
            | Stat::GenericFor { body, .. } => {
                reconstruct_table_constructors(body);
            }
            // Function literals can also have nested module-style tables
            // inside their bodies — drill down so closures get the same
            // treatment.
            Stat::Local { values, .. } | Stat::Assign { values, .. } => {
                for v in values.iter_mut() {
                    reconstruct_table_constructors_in_expr(v);
                }
            }
            Stat::ExprStat(e) => {
                reconstruct_table_constructors_in_expr(e);
            }
            Stat::Return { values } => {
                for v in values.iter_mut() {
                    reconstruct_table_constructors_in_expr(v);
                }
            }
            _ => {}
        }
    }

    // Walk this level looking for the start of a constructor pattern.
    let mut i = 0;
    while i < stmts.len() {
        // Try to identify the local-empty-table seed at index i.
        let local_name = match constructor_seed_name(&stmts[i]) {
            Some(n) => n,
            None => {
                i += 1;
                continue;
            }
        };

        // Greedily collect the contiguous prefix of statements starting at
        // i+1. Two shapes are absorbed:
        //   (a) direct: `local_name.K = V`  (B0.47 original)
        //   (b) two-step: `local F = VALUE; local_name.K = F`  (B0.48)
        //       where VALUE is pure (Function/String/Number/Bool/Nil/Vector/
        //       Table) and F is read exactly once across the remainder of the
        //       block (that one read being the field-assign we're about to
        //       absorb).
        //
        // Phase B0.112: pre-compute name-read counts across stmts[i+1..end]
        // so two-step remainder checks are O(1) lookups instead of O(n) scans.
        let mut remaining_reads: HashMap<String, usize> = HashMap::new();
        for stmt in &stmts[i + 1..] {
            accumulate_name_reads_in_stmt(stmt, &mut remaining_reads);
        }

        let mut absorbed = Vec::<TableField>::new();
        let mut j = i + 1;
        while j < stmts.len() {
            // Try direct shape first — it's the common case and cheaper.
            if let Some(field) = field_assign_to_constructor_field(&stmts[j], &local_name) {
                absorbed.push(field);
                subtract_name_reads_in_stmt(&stmts[j], &mut remaining_reads);
                j += 1;
                continue;
            }

            // Two-step shape: current must be a single-name Local with a pure
            // RHS, next must be a Field/Index assign to local_name whose
            // value is Name(F) matching the local we just saw.
            if j + 1 < stmts.len() {
                if let Some((field, _f_name)) =
                    two_step_field_absorb(&stmts[j], &stmts[j + 1], &local_name, &remaining_reads)
                {
                    subtract_name_reads_in_stmt(&stmts[j], &mut remaining_reads);
                    subtract_name_reads_in_stmt(&stmts[j + 1], &mut remaining_reads);
                    absorbed.push(field);
                    j += 2;
                    continue;
                }
            }

            break;
        }

        if absorbed.is_empty() {
            // No fields to absorb — leave the empty table alone.
            i += 1;
            continue;
        }

        // Merge absorbed fields into the seed statement's table expression.
        match &mut stmts[i] {
            Stat::Local { values, .. } => {
                if let Expr::Table { fields } = &mut values[0] {
                    fields.extend(absorbed);
                }
            }
            Stat::Assign { values, .. } => {
                if let Expr::Table { fields } = &mut values[0] {
                    fields.extend(absorbed);
                }
            }
            _ => unreachable!("constructor_seed_name guards both arms"),
        }

        // Drain absorbed statements (i+1 .. j) — using drain to avoid
        // shifting elements one at a time.
        stmts.drain(i + 1..j);

        // After folding we move past the seed (which now holds the constructor)
        // — there could be ANOTHER independent table constructor right after.
        i += 1;
    }
}

/// If `stmt` is an empty-table seed (`local N = {}` or `N = {}`), return the
/// name N. Returns None for everything else.
pub(super) fn constructor_seed_name(stmt: &Stat) -> Option<String> {
    match stmt {
        Stat::Local { names, values }
            if names.len() == 1 && values.len() == 1 =>
        {
            if let Expr::Table { fields } = &values[0] {
                if fields.is_empty() {
                    return Some(names[0].clone());
                }
            }
            None
        }
        Stat::Assign { targets, values }
            if targets.len() == 1 && values.len() == 1 =>
        {
            if let (Expr::Name(n), Expr::Table { fields }) = (&targets[0], &values[0]) {
                if fields.is_empty() {
                    return Some(n.clone());
                }
            }
            None
        }
        _ => None,
    }
}

/// If `stmt` is `Name(target).K = V` or `Name(target)[K] = V` AND
/// V doesn't circularly read `target`, return the corresponding TableField.
/// Returns None otherwise (terminates the contiguous run).
pub(super) fn field_assign_to_constructor_field(stmt: &Stat, target_name: &str) -> Option<TableField> {
    let (targets, values) = match stmt {
        Stat::Assign { targets, values } if targets.len() == 1 && values.len() == 1 => {
            (targets, values)
        }
        _ => return None,
    };

    let value = &values[0];

    // Circular-reference guard: if the RHS reads back from target_name,
    // it would observe the table mid-construction. Stop folding.
    if expr_references_name(value, target_name) {
        return None;
    }

    match &targets[0] {
        // `target_name.field = value`
        Expr::Field { object, field } => {
            if let Expr::Name(obj) = object.as_ref() {
                if obj == target_name {
                    return Some(TableField::Named(field.clone(), value.clone()));
                }
            }
            None
        }
        // `target_name[key] = value`  (SETTABLEN-style or general SETTABLE)
        Expr::Index { object, key } => {
            if let Expr::Name(obj) = object.as_ref() {
                if obj == target_name {
                    // If the key is a simple string literal AND a valid Luau
                    // identifier, prefer the named form over the indexed form.
                    if let Expr::String(s) = key.as_ref() {
                        if is_valid_luau_identifier(s) {
                            return Some(TableField::Named(s.clone(), value.clone()));
                        }
                    }
                    return Some(TableField::Indexed(*key.clone(), value.clone()));
                }
            }
            None
        }
        _ => None,
    }
}

/// Phase B0.48 — two-step field absorb.
///
/// Recognise the Roblox-emitted pattern:
///   local F = VALUE            (F = single-register local, VALUE = pure)
///   target_name.K = F          (Field or Index with string/number key)
///
/// AND F is read exactly once across the `remainder` (the slice of
/// statements AFTER the field-assign). Because the field-assign itself
/// reads F exactly once, a total-reads-in-remainder of 0 means F is
/// consumed *only* by this absorption — safe to fold.
///
/// Returns `Some((TableField, F_name))` if the pair absorbs cleanly,
/// otherwise `None` (and the constructor run should break).
pub(super) fn two_step_field_absorb(
    local_stmt: &Stat,
    assign_stmt: &Stat,
    target_name: &str,
    remaining_reads: &HashMap<String, usize>,
) -> Option<(TableField, String)> {
    // First stmt must be either `local F = VALUE` or `F = VALUE` (a bare Name
    // assignment — produced when the lifter reuses a register that already
    // had a `local` earlier; Roblox ModuleScript emit pattern). Both shapes
    // carry the same "name F holds VALUE" semantics for the absorb.
    let (f_name, f_value) = match local_stmt {
        Stat::Local { names, values } if names.len() == 1 && values.len() == 1 => {
            (names[0].clone(), values[0].clone())
        }
        Stat::Assign { targets, values } if targets.len() == 1 && values.len() == 1 => {
            match &targets[0] {
                Expr::Name(n) => (n.clone(), values[0].clone()),
                _ => return None,
            }
        }
        _ => return None,
    };

    // VALUE must be a PURE expression — no Call/MethodCall (side effects)
    // and no Name/Field/Index/BinOp/UnOp (may observe later mutation or
    // be semantically tied to read-order). We fold only literals and
    // function bodies, which are the Roblox-corpus shapes we want.
    if !is_pure_two_step_value(&f_value) {
        return None;
    }

    // Guard: F must not reference target_name (weird shadowing/circular
    // construction) — expression-level check mirrors the direct-assign
    // circular-reference guard in `field_assign_to_constructor_field`.
    if expr_references_name(&f_value, target_name) {
        return None;
    }

    // Second stmt must be a single-target Assign whose RHS reads exactly
    // `Expr::Name(f_name)`.
    let (targets, values) = match assign_stmt {
        Stat::Assign { targets, values } if targets.len() == 1 && values.len() == 1 => {
            (targets, values)
        }
        _ => return None,
    };
    match &values[0] {
        Expr::Name(n) if n == &f_name => {}
        _ => return None,
    }

    // F name must not collide with target_name (edge case — would shadow).
    if f_name == target_name {
        return None;
    }

    // Now extract the key.
    let field = match &targets[0] {
        Expr::Field { object, field } => {
            if let Expr::Name(obj) = object.as_ref() {
                if obj != target_name {
                    return None;
                }
                TableField::Named(field.clone(), f_value)
            } else {
                return None;
            }
        }
        Expr::Index { object, key } => {
            if let Expr::Name(obj) = object.as_ref() {
                if obj != target_name {
                    return None;
                }
                // Promote string-valued identifier key to Named form.
                if let Expr::String(s) = key.as_ref() {
                    if is_valid_luau_identifier(s) {
                        TableField::Named(s.clone(), f_value)
                    } else {
                        TableField::Indexed(*key.clone(), f_value)
                    }
                } else {
                    TableField::Indexed(*key.clone(), f_value)
                }
            } else {
                return None;
            }
        }
        _ => return None,
    };

    // F must be read EXACTLY ZERO times in the remainder (stmts after the
    // field-assign). If F leaks out, it's used elsewhere and cannot be
    // safely folded into the constructor.
    //
    // Phase B0.112: O(1) check via pre-computed remaining_reads map.
    // remaining_reads covers stmts[j..end]. stmts[j] (local F = VALUE)
    // defines F with a pure value (0 reads of F). stmts[j+1] reads F
    // exactly once (the Name(f_name) match above). So reads in
    // stmts[j+2..end] = remaining_reads[f_name] - 0 - 1.
    // We want this == 0, i.e. remaining_reads[f_name] <= 1.
    let total_remaining = remaining_reads.get(&f_name).copied().unwrap_or(0);
    if total_remaining > 1 {
        return None;
    }

    Some((field, f_name))
}

/// Pure RHS predicate for B0.48 two-step absorb.
///
/// Returns true for expressions that are safe to move across the intervening
/// `local F = ...; M.field = F` pair without changing evaluation semantics.
/// Conservative whitelist: literals and function bodies only. Function
/// literals are considered "pure" in this context because they merely
/// allocate a closure — they do not observe or mutate mutable state at
/// definition time.
pub(super) fn is_pure_two_step_value(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Nil
            | Expr::Bool(_)
            | Expr::Number(_)
            | Expr::String(_)
            | Expr::Vector(_, _, _)
            | Expr::Function { .. }
            | Expr::Table { .. }
    )
}

/// Return true if `s` is a syntactically valid Luau identifier
/// (i.e., can appear as `t.s` rather than `t["s"]`).
/// Reserved keywords are intentionally still treated as identifiers
/// here — the emitter handles the escaping if needed.
pub(super) fn is_valid_luau_identifier(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let mut chars = s.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

// ─── Phase B0.112: Bulk name-read accumulation for O(1) remainder checks ───

/// Walk a statement and increment `counts[name]` for every `Expr::Name(name)`
/// read. Mirrors the semantics of `count_name_reads_in_stmt` but collects
/// ALL names in one pass rather than a single target name.
fn accumulate_name_reads_in_stmt(stmt: &Stat, counts: &mut HashMap<String, usize>) {
    match stmt {
        Stat::Local { values, .. } => {
            for v in values { accumulate_name_reads_in_expr(v, counts); }
        }
        Stat::Assign { targets, values } => {
            for v in values { accumulate_name_reads_in_expr(v, counts); }
            for t in targets {
                match t {
                    Expr::Name(_) => {} // writing to name, not reading
                    other => accumulate_name_reads_in_expr(other, counts),
                }
            }
        }
        Stat::ExprStat(e) => accumulate_name_reads_in_expr(e, counts),
        Stat::Return { values } => {
            for v in values { accumulate_name_reads_in_expr(v, counts); }
        }
        Stat::If { condition, then_body, elseif_clauses, else_body } => {
            accumulate_name_reads_in_expr(condition, counts);
            for s in then_body { accumulate_name_reads_in_stmt(s, counts); }
            for (cond, body) in elseif_clauses {
                accumulate_name_reads_in_expr(cond, counts);
                for s in body { accumulate_name_reads_in_stmt(s, counts); }
            }
            if let Some(eb) = else_body {
                for s in eb { accumulate_name_reads_in_stmt(s, counts); }
            }
        }
        Stat::While { condition, body } => {
            accumulate_name_reads_in_expr(condition, counts);
            for s in body { accumulate_name_reads_in_stmt(s, counts); }
        }
        Stat::Repeat { body, condition } => {
            for s in body { accumulate_name_reads_in_stmt(s, counts); }
            accumulate_name_reads_in_expr(condition, counts);
        }
        Stat::NumericFor { start, stop, step, body, .. } => {
            accumulate_name_reads_in_expr(start, counts);
            accumulate_name_reads_in_expr(stop, counts);
            if let Some(s) = step { accumulate_name_reads_in_expr(s, counts); }
            for s in body { accumulate_name_reads_in_stmt(s, counts); }
        }
        Stat::GenericFor { iterators, body, .. } => {
            for it in iterators { accumulate_name_reads_in_expr(it, counts); }
            for s in body { accumulate_name_reads_in_stmt(s, counts); }
        }
        Stat::DoBlock { body } => {
            for s in body { accumulate_name_reads_in_stmt(s, counts); }
        }
        Stat::LocalFunction { func, .. } | Stat::MethodFunction { func, .. } => {
            accumulate_name_reads_in_expr(func, counts);
        }
        _ => {}
    }
}

/// Walk an expression and increment `counts[name]` for every `Expr::Name`.
fn accumulate_name_reads_in_expr(expr: &Expr, counts: &mut HashMap<String, usize>) {
    match expr {
        Expr::Name(n) => { *counts.entry(n.clone()).or_default() += 1; }
        Expr::Field { object, .. } => accumulate_name_reads_in_expr(object, counts),
        Expr::Index { object, key } => {
            accumulate_name_reads_in_expr(object, counts);
            accumulate_name_reads_in_expr(key, counts);
        }
        Expr::BinOp { left, right, .. } => {
            accumulate_name_reads_in_expr(left, counts);
            accumulate_name_reads_in_expr(right, counts);
        }
        Expr::UnOp { operand, .. } => accumulate_name_reads_in_expr(operand, counts),
        Expr::Call { func, args } => {
            accumulate_name_reads_in_expr(func, counts);
            for a in args { accumulate_name_reads_in_expr(a, counts); }
        }
        Expr::MethodCall { object, args, .. } => {
            accumulate_name_reads_in_expr(object, counts);
            for a in args { accumulate_name_reads_in_expr(a, counts); }
        }
        Expr::Table { fields } => {
            for f in fields {
                match f {
                    TableField::Sequential(e) => accumulate_name_reads_in_expr(e, counts),
                    TableField::Named(_, e) => accumulate_name_reads_in_expr(e, counts),
                    TableField::Indexed(k, v) => {
                        accumulate_name_reads_in_expr(k, counts);
                        accumulate_name_reads_in_expr(v, counts);
                    }
                }
            }
        }
        Expr::Function { .. } => {} // closures capture by upvalue, not name
        Expr::Ternary { cond, then_expr, else_expr } => {
            accumulate_name_reads_in_expr(cond, counts);
            accumulate_name_reads_in_expr(then_expr, counts);
            accumulate_name_reads_in_expr(else_expr, counts);
        }
        _ => {}
    }
}

/// Subtract the name reads of a single statement from the running totals.
fn subtract_name_reads_in_stmt(stmt: &Stat, counts: &mut HashMap<String, usize>) {
    let mut sub = HashMap::new();
    accumulate_name_reads_in_stmt(stmt, &mut sub);
    for (name, n) in sub {
        if let Some(v) = counts.get_mut(&name) {
            *v = v.saturating_sub(n);
        }
    }
}

// ─── Phase C2: SETLIST / sequential-integer-index coalesce ──────────────
//
// The lifter emits `local t = {}; t[1] = a; t[2] = b; t[3] = c` for the
// SETLIST bytecode family (and for SETTABLEKS with integer keys).
// `reconstruct_table_constructors` already folds these into
// `Expr::Table { fields: [Indexed(Number(1), a), Indexed(Number(2), b), ...] }`
// (integer keys are not valid Luau identifiers, so they route to `Indexed`).
//
// This pass takes the output of `reconstruct_table_constructors` and
// converts any leading prefix of sequentially-indexed integer fields
// (1, 2, 3, ..., N with NO gaps) into positional `TableField::Sequential`
// entries, so the emitter renders `{a, b, c}` instead of
// `{[1] = a, [2] = b, [3] = c}`.
//
// Requirements:
//   * Keys are integer-valued numbers (`Expr::Number(k)` where k == k.round()
//     and 1.0 <= k).
//   * The sequence starts at 1 and is strictly contiguous (1, 2, 3, ...).
//   * Stop at the first non-integer-indexed field, any key gap, or any
//     non-`Indexed` field. Everything up to that point is promoted.
//
// Correctness: reads of `t` between assignments are already guarded by
// `reconstruct_table_constructors` (it breaks on the first read-back of
// the table name via `expr_references_name`), so if we are seeing a
// clean run of indexed fields in a `Table { fields }`, we know no
// intervening statement referenced the table and promotion is safe.
pub(super) fn coalesce_setlist_sequential(stmts: &mut Vec<Stat>) {
    for stmt in stmts.iter_mut() {
        match stmt {
            Stat::If { then_body, elseif_clauses, else_body, .. } => {
                coalesce_setlist_sequential(then_body);
                for (_, body) in elseif_clauses.iter_mut() {
                    coalesce_setlist_sequential(body);
                }
                if let Some(eb) = else_body {
                    coalesce_setlist_sequential(eb);
                }
            }
            Stat::While { body, .. }
            | Stat::Repeat { body, .. }
            | Stat::DoBlock { body }
            | Stat::NumericFor { body, .. }
            | Stat::GenericFor { body, .. } => {
                coalesce_setlist_sequential(body);
            }
            Stat::Local { values, .. } | Stat::Assign { values, .. } => {
                for v in values.iter_mut() {
                    coalesce_setlist_sequential_in_expr(v);
                }
            }
            Stat::ExprStat(e) => coalesce_setlist_sequential_in_expr(e),
            Stat::Return { values } => {
                for v in values.iter_mut() {
                    coalesce_setlist_sequential_in_expr(v);
                }
            }
            Stat::LocalFunction { func, .. } | Stat::MethodFunction { func, .. } => {
                coalesce_setlist_sequential_in_expr(func);
            }
            _ => {}
        }
    }
}

/// Recurse into expressions looking for `Expr::Table` constructors and
/// apply the sequential-integer-index coalesce to them.
pub(super) fn coalesce_setlist_sequential_in_expr(expr: &mut Expr) {
    match expr {
        Expr::Function { body, .. } => coalesce_setlist_sequential(body),
        Expr::Table { fields } => {
            // Recurse into nested tables first.
            for f in fields.iter_mut() {
                match f {
                    TableField::Sequential(e) => coalesce_setlist_sequential_in_expr(e),
                    TableField::Named(_, e) => coalesce_setlist_sequential_in_expr(e),
                    TableField::Indexed(k, v) => {
                        coalesce_setlist_sequential_in_expr(k);
                        coalesce_setlist_sequential_in_expr(v);
                    }
                }
            }
            // Then perform the leading-prefix promotion on this table.
            promote_sequential_prefix(fields);
        }
        Expr::Field { object, .. } => coalesce_setlist_sequential_in_expr(object),
        Expr::Index { object, key } => {
            coalesce_setlist_sequential_in_expr(object);
            coalesce_setlist_sequential_in_expr(key);
        }
        Expr::BinOp { left, right, .. } => {
            coalesce_setlist_sequential_in_expr(left);
            coalesce_setlist_sequential_in_expr(right);
        }
        Expr::UnOp { operand, .. } => coalesce_setlist_sequential_in_expr(operand),
        Expr::Call { func, args } => {
            coalesce_setlist_sequential_in_expr(func);
            for a in args.iter_mut() {
                coalesce_setlist_sequential_in_expr(a);
            }
        }
        Expr::MethodCall { object, args, .. } => {
            coalesce_setlist_sequential_in_expr(object);
            for a in args.iter_mut() {
                coalesce_setlist_sequential_in_expr(a);
            }
        }
        Expr::Ternary { cond, then_expr, else_expr } => {
            coalesce_setlist_sequential_in_expr(cond);
            coalesce_setlist_sequential_in_expr(then_expr);
            coalesce_setlist_sequential_in_expr(else_expr);
        }
        _ => {}
    }
}

/// Walk the leading fields of a table constructor; while the current field
/// is `Indexed(Number(k), v)` with k forming the next expected integer
/// (1, 2, 3, ...), replace it with `Sequential(v)`. Stop at the first gap
/// or non-integer-indexed field.
pub(super) fn promote_sequential_prefix(fields: &mut Vec<TableField>) {
    let mut expected: i64 = 1;
    for i in 0..fields.len() {
        let k_val = match &fields[i] {
            TableField::Indexed(Expr::Number(n), _) => {
                if !n.is_finite() { break; }
                let rounded = n.round();
                if (*n - rounded).abs() > f64::EPSILON { break; }
                rounded as i64
            }
            _ => break,
        };
        if k_val != expected {
            // Gap or out-of-order — do not promote this field or any after.
            break;
        }
        // Promote: swap the Indexed(Number, v) for Sequential(v).
        let taken = std::mem::replace(&mut fields[i], TableField::Sequential(Expr::Nil));
        if let TableField::Indexed(_k, v) = taken {
            fields[i] = TableField::Sequential(v);
        } else {
            // Shouldn't happen — we already pattern-matched above.
            unreachable!();
        }
        expected += 1;
    }
}

/// Recurse into an expression looking for `Function` bodies and rebuild
/// any module-style constructors inside them. (Sequential drilling via
/// the Stat-level pre-pass already covers most cases, but a Table
/// expression containing a Function field is one example where we need
/// to walk into the value to get nested closures.)
pub(super) fn reconstruct_table_constructors_in_expr(expr: &mut Expr) {
    match expr {
        Expr::Function { body, .. } => {
            reconstruct_table_constructors(body);
        }
        Expr::Table { fields } => {
            for f in fields.iter_mut() {
                match f {
                    TableField::Sequential(e) => reconstruct_table_constructors_in_expr(e),
                    TableField::Named(_, e) => reconstruct_table_constructors_in_expr(e),
                    TableField::Indexed(k, v) => {
                        reconstruct_table_constructors_in_expr(k);
                        reconstruct_table_constructors_in_expr(v);
                    }
                }
            }
        }
        Expr::Field { object, .. } => reconstruct_table_constructors_in_expr(object),
        Expr::Index { object, key } => {
            reconstruct_table_constructors_in_expr(object);
            reconstruct_table_constructors_in_expr(key);
        }
        Expr::BinOp { left, right, .. } => {
            reconstruct_table_constructors_in_expr(left);
            reconstruct_table_constructors_in_expr(right);
        }
        Expr::UnOp { operand, .. } => reconstruct_table_constructors_in_expr(operand),
        Expr::Call { func, args } => {
            reconstruct_table_constructors_in_expr(func);
            for a in args.iter_mut() {
                reconstruct_table_constructors_in_expr(a);
            }
        }
        Expr::MethodCall { object, args, .. } => {
            reconstruct_table_constructors_in_expr(object);
            for a in args.iter_mut() {
                reconstruct_table_constructors_in_expr(a);
            }
        }
        _ => {}
    }
}
