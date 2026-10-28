//! Last resort for Luau's register limits: move locals into one table.
//!
//! The real Luau compiler allows 200 locals and 200 upvalues per function.
//! A big module can hold more than that even after `local_pressure` has
//! folded what it safely can: hundreds of module-level locals that several
//! closures read, so nothing is single-use. This pass fires only when a
//! function is predicted to break a limit, so ordinary output is unchanged.
//!
//! In such a function `A`, every local that is declared directly in `A`'s top
//! block, exactly once in `A` (deep: no parameter, loop variable or nested
//! local shares the name), and never mentioned before its declaration, is
//! stored in a fresh table instead: `local slots = {}` first, then `x`
//! becomes `slots.x` at every mention, `local x = e` becomes `slots.x = e`
//! and `local x` is dropped (a missing field reads nil, like a fresh local).
//! A closure then captures one table, not one upvalue per name.
//!
//! Safe because a table field behaves like a captured variable: one shared
//! mutable cell, read and written at the same points. Each such local exists
//! once per call of `A` (top block, no loop), so the table is created once per
//! call too. No expression is moved, copied or dropped, so no call or string
//! is reordered or duplicated. A mixed `local a, b = f()` where only some
//! names qualify is left alone.

use super::local_pressure::{
    census_block, declared_names_in_stat, expr_children_mut, free_in_expr, names_in_expr,
    names_in_stat, stat_blocks_mut, stat_exprs_mut,
    Census,
};
use crate::ast::{Expr, Stat};
use std::collections::{HashMap, HashSet};

/// Luau allows exactly 200 of each. The estimates never undercount (an
/// upvalue estimate counts every free name that is a local anywhere), so
/// "estimate above 200" catches every real failure and leaves alone any
/// function that compiles today.
const LOCAL_LIMIT: usize = 200;
const UPVALUE_LIMIT: usize = 200;

/// Entry point: rewrite the root function body `stmts` in place.
pub fn spill(stmts: &mut Vec<Stat>) {
    let mut c = Census::default();
    census_block(stmts, &mut c, false);
    let declared: HashSet<String> = c.decls.keys().cloned().collect();
    spill_fn(stmts, &[], &declared);
}

fn spill_fn(body: &mut Vec<Stat>, params: &[String], declared: &HashSet<String>) {
    let bad_free = free_of_heavy_closures(body, declared);
    let local_heavy = params.len() + max_live(body) > LOCAL_LIMIT;
    if local_heavy || !bad_free.is_empty() {
        let chosen = choose(body, params, local_heavy, &bad_free);
        if !chosen.is_empty() {
            apply(body, &chosen);
        }
    }
    for s in body.iter_mut() {
        stat_exprs_mut(s, &mut |e| recurse_functions(e, declared));
        stat_blocks_mut(s, &mut |b| recurse_blocks(b, declared));
    }
}

fn recurse_blocks(b: &mut Vec<Stat>, declared: &HashSet<String>) {
    for s in b.iter_mut() {
        stat_exprs_mut(s, &mut |e| recurse_functions(e, declared));
        stat_blocks_mut(s, &mut |bb| recurse_blocks(bb, declared));
    }
}

fn recurse_functions(e: &mut Expr, declared: &HashSet<String>) {
    if let Expr::Function { params, body, .. } = e {
        let p = params.clone();
        spill_fn(body, &p, declared);
        return;
    }
    expr_children_mut(e, &mut |c| recurse_functions(c, declared));
}

// ---------------------------------------------------------------------------
// Triggers
// ---------------------------------------------------------------------------

/// Most locals live at once in one function (nested blocks add up along a
/// path; sibling blocks do not; closures count on their own).
fn max_live(stmts: &[Stat]) -> usize {
    let own: usize = stmts
        .iter()
        .map(|s| match s {
            Stat::Local { names, .. } => names.len(),
            Stat::LocalFunction { .. } => 1,
            _ => 0,
        })
        .sum();
    let mut deepest = 0;
    for s in stmts {
        let (extra, blocks) = match s {
            Stat::NumericFor { body, .. } => (1, vec![body]),
            Stat::GenericFor { vars, body, .. } => (vars.len(), vec![body]),
            Stat::If { then_body, elseif_clauses, else_body, .. } => {
                let mut v = vec![then_body];
                v.extend(elseif_clauses.iter().map(|(_, b)| b));
                v.extend(else_body.iter());
                (0, v)
            }
            Stat::While { body, .. } | Stat::Repeat { body, .. } | Stat::DoBlock { body } => {
                (0, vec![body])
            }
            _ => continue,
        };
        for b in blocks {
            deepest = deepest.max(extra + max_live(b));
        }
    }
    own + deepest
}

/// Union of the free names of every closure (at any depth below `body`) whose
/// estimated upvalue count is over the limit.
fn free_of_heavy_closures(body: &mut [Stat], declared: &HashSet<String>) -> HashSet<String> {
    let mut out = HashSet::new();
    for s in body.iter_mut() {
        stat_exprs_mut(s, &mut |e| heavy_in_expr(e, declared, &mut out));
        stat_blocks_mut(s, &mut |b| heavy_in_block(b, declared, &mut out));
    }
    out
}

fn heavy_in_block(b: &mut [Stat], declared: &HashSet<String>, out: &mut HashSet<String>) {
    for s in b.iter_mut() {
        stat_exprs_mut(s, &mut |e| heavy_in_expr(e, declared, out));
        stat_blocks_mut(s, &mut |bb| heavy_in_block(bb, declared, out));
    }
}

fn heavy_in_expr(e: &mut Expr, declared: &HashSet<String>, out: &mut HashSet<String>) {
    if let Expr::Function { .. } = e {
        let mut free = HashSet::new();
        free_in_expr(e, &mut Vec::new(), &mut free);
        free.retain(|n| declared.contains(n));
        if free.len() > UPVALUE_LIMIT {
            out.extend(free);
        }
    }
    match e {
        Expr::Function { body, .. } => heavy_in_block(body, declared, out),
        _ => expr_children_mut(e, &mut |c| heavy_in_expr(c, declared, out)),
    }
}

// ---------------------------------------------------------------------------
// Selection
// ---------------------------------------------------------------------------

/// Names to move into the table: eligible locals of the top block and of the
/// `do` / `if` blocks reachable from it without crossing a loop or closure
/// (such a block runs at most once per call, so its locals exist once). All
/// of them when this function itself holds too many locals, else those a
/// heavy closure captures.
fn choose(
    body: &mut [Stat],
    params: &[String],
    local_heavy: bool,
    bad_free: &HashSet<String>,
) -> HashSet<String> {
    let mut census = Census::default();
    census_block(body, &mut census, false);
    let mut chosen = HashSet::new();
    let ctx = Pick { census: &census, params, local_heavy, bad_free };
    scan_block(body, &ctx, &mut chosen);
    chosen
}

struct Pick<'a> {
    census: &'a Census,
    params: &'a [String],
    local_heavy: bool,
    bad_free: &'a HashSet<String>,
}

fn uses_of(s: &mut Stat) -> HashMap<String, usize> {
    let mut c = Census::default();
    census_block(std::slice::from_mut(s), &mut c, false);
    c.uses
}

fn scan_block(block: &mut [Stat], ctx: &Pick, chosen: &mut HashSet<String>) {
    let per_stmt: Vec<HashMap<String, usize>> = block.iter_mut().map(uses_of).collect();
    let mut total: HashMap<String, usize> = HashMap::new();
    for m in &per_stmt {
        for (n, c) in m {
            *total.entry(n.clone()).or_insert(0) += c;
        }
    }
    let mut before: HashMap<String, usize> = HashMap::new();
    for (k, s) in block.iter_mut().enumerate() {
        consider(s, ctx, &total, &before, chosen);
        for (n, c) in &per_stmt[k] {
            *before.entry(n.clone()).or_insert(0) += c;
        }
        match s {
            Stat::DoBlock { body } => scan_block(body, ctx, chosen),
            Stat::If { then_body, elseif_clauses, else_body, .. } => {
                scan_block(then_body, ctx, chosen);
                for (_, b) in elseif_clauses.iter_mut() {
                    scan_block(b, ctx, chosen);
                }
                if let Some(b) = else_body {
                    scan_block(b, ctx, chosen);
                }
            }
            _ => {}
        }
    }
}

/// Add the names this statement declares if every one of them qualifies:
/// declared once in the function, not a parameter, mentioned only from this
/// statement on inside its own block (an earlier or outside mention would be
/// a global of the same name), and not in its own initialiser.
fn consider(
    s: &mut Stat,
    ctx: &Pick,
    total: &HashMap<String, usize>,
    before: &HashMap<String, usize>,
    chosen: &mut HashSet<String>,
) {
    let names: Vec<String> = match s {
        Stat::Local { names, .. } => names.clone(),
        Stat::LocalFunction { name, .. } => vec![name.clone()],
        _ => return,
    };
    let mut in_values = HashSet::new();
    if let Stat::Local { values, .. } = s {
        for v in values.iter_mut() {
            names_in_expr(v, &mut in_values);
        }
    }
    let qualifies = |n: &String| {
        let from_here = total.get(n).copied().unwrap_or(0) - before.get(n).copied().unwrap_or(0);
        ctx.census.decls.get(n) == Some(&1)
            && !ctx.params.contains(n)
            && !in_values.contains(n)
            && from_here == ctx.census.uses.get(n).copied().unwrap_or(0)
            && (ctx.local_heavy || ctx.bad_free.contains(n))
    };
    // A bare `local a, b, c` has no initialiser to keep together, so each
    // name is judged alone; with values it is all or nothing.
    let bare = matches!(s, Stat::Local { values, .. } if values.is_empty());
    if bare {
        chosen.extend(names.iter().filter(|n| qualifies(n)).cloned());
    } else if names.iter().all(qualifies) {
        chosen.extend(names);
    }
}

// ---------------------------------------------------------------------------
// Rewrite
// ---------------------------------------------------------------------------

fn apply(body: &mut Vec<Stat>, chosen: &HashSet<String>) {
    let mut taken = HashSet::new();
    for s in body.iter_mut() {
        names_in_stat(s, &mut taken);
        declared_names_in_stat(s, &mut taken);
    }
    let mut table = String::from("slots");
    let mut n = 2;
    while taken.contains(&table) {
        table = format!("slots{n}");
        n += 1;
    }
    rewrite_block(body, chosen, &table);
    body.insert(
        0,
        Stat::Local { names: vec![table], values: vec![Expr::Table { fields: Vec::new() }] },
    );
}

fn slot(table: &str, name: &str) -> Expr {
    Expr::Field { object: Box::new(Expr::Name(table.to_string())), field: name.to_string() }
}

/// Rewrite every mention in a block (closures included), then turn the
/// declarations of moved names into assignments (or drop them if bare).
fn rewrite_block(block: &mut Vec<Stat>, chosen: &HashSet<String>, table: &str) {
    for s in block.iter_mut() {
        stat_exprs_mut(s, &mut |e| rewrite_expr(e, chosen, table));
        stat_blocks_mut(s, &mut |b| rewrite_block(b, chosen, table));
    }
    let old = std::mem::take(block);
    for s in old {
        match s {
            Stat::Local { names, values } if values.is_empty() && names.iter().any(|n| chosen.contains(n)) => {
                let rest: Vec<String> = names.into_iter().filter(|n| !chosen.contains(n)).collect();
                if !rest.is_empty() {
                    block.push(Stat::Local { names: rest, values });
                }
            }
            Stat::Local { names, values } if !names.is_empty() && names.iter().all(|n| chosen.contains(n)) => {
                let targets = names.iter().map(|n| slot(table, n)).collect();
                block.push(Stat::Assign { targets, values });
            }
            Stat::LocalFunction { name, func } if chosen.contains(&name) => {
                block.push(Stat::Assign { targets: vec![slot(table, &name)], values: vec![func] });
            }
            other => block.push(other),
        }
    }
}

fn rewrite_expr(e: &mut Expr, chosen: &HashSet<String>, table: &str) {
    match e {
        Expr::Name(n) if chosen.contains(n.as_str()) => {
            *e = slot(table, &n.clone());
        }
        Expr::Function { body, .. } => rewrite_block(body, chosen, table),
        _ => expr_children_mut(e, &mut |c| rewrite_expr(c, chosen, table)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decompiler::emit::emit_block;

    fn name(n: &str) -> Expr {
        Expr::Name(n.to_string())
    }
    fn render(stmts: &[Stat]) -> String {
        let mut out = String::new();
        emit_block(&mut out, stmts, 0);
        out
    }
    fn closure_reading(names: &[String]) -> Expr {
        let values = names.iter().map(|n| name(n)).collect();
        Expr::Function { params: vec![], is_vararg: false, body: vec![Stat::Return { values }] }
    }

    /// 250 forward-declared module locals read by one closure: over both limits.
    fn heavy_module() -> Vec<Stat> {
        let names: Vec<String> = (0..250).map(|k| format!("m{k}")).collect();
        vec![
            Stat::Local { names: names.clone(), values: vec![] },
            Stat::Assign { targets: vec![name("m0")], values: vec![Expr::Number(7.0)] },
            Stat::Return { values: vec![closure_reading(&names)] },
        ]
    }

    #[test]
    fn heavy_function_moves_its_locals_into_one_table() {
        let mut stmts = heavy_module();
        spill(&mut stmts);
        let text = render(&stmts);
        assert!(text.starts_with("local slots = {}"), "{}", &text[..60]);
        assert!(text.contains("slots.m0 = 7"));
        assert!(text.contains("slots.m249"));
        assert!(!text.contains("local m0"), "declaration list must be gone");
    }

    #[test]
    fn light_function_is_untouched() {
        let mut stmts = vec![
            Stat::Local { names: vec!["a".into(), "b".into()], values: vec![] },
            Stat::Return { values: vec![closure_reading(&["a".to_string(), "b".to_string()])] },
        ];
        let before = render(&stmts);
        spill(&mut stmts);
        assert_eq!(before, render(&stmts));
    }

    #[test]
    fn name_shadowed_by_a_parameter_is_never_moved() {
        let mut names: Vec<String> = (0..250).map(|k| format!("m{k}")).collect();
        names.push("p".into());
        let inner = Expr::Function {
            params: vec!["p".into()],
            is_vararg: false,
            body: vec![Stat::Return { values: vec![name("p")] }],
        };
        let mut stmts = vec![
            Stat::Local { names: names.clone(), values: vec![] },
            Stat::Return { values: vec![closure_reading(&names[..250]), inner] },
        ];
        spill(&mut stmts);
        let text = render(&stmts);
        assert!(text.contains("local p"), "p is declared twice, so it stays a local");
        assert!(text.contains("return p"));
        assert!(!text.contains("slots.p"));
    }

    #[test]
    fn a_mention_before_the_declaration_is_a_global_and_blocks_the_move() {
        let mut stmts = heavy_module();
        stmts.insert(0, Stat::ExprStat(Expr::Call { func: Box::new(name("print")), args: vec![name("m5")] }));
        spill(&mut stmts);
        let text = render(&stmts);
        assert!(text.contains("print(m5)"), "global read must not become slots.m5");
        assert!(text.contains("local m5") || text.contains(", m5"));
    }

    #[test]
    fn locals_inside_a_do_block_are_moved_but_loop_locals_are_not() {
        let names: Vec<String> = (0..250).map(|k| format!("d{k}")).collect();
        let loop_body = vec![
            Stat::Local { names: vec!["perIter".into()], values: vec![Expr::Number(1.0)] },
            Stat::ExprStat(Expr::Function { params: vec![], is_vararg: false, body: vec![Stat::Return { values: vec![name("perIter")] }] }),
        ];
        let mut stmts = vec![
            Stat::DoBlock {
                body: vec![
                    Stat::Local { names: names.clone(), values: vec![] },
                    Stat::ExprStat(closure_reading(&names)),
                ],
            },
            Stat::While { condition: Expr::Bool(true), body: loop_body },
        ];
        spill(&mut stmts);
        let text = render(&stmts);
        assert!(text.contains("slots.d0"));
        assert!(text.contains("local perIter = 1"), "loop local stays fresh per iteration");
    }
}
