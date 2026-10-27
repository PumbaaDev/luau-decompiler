//! Liveness helpers for stores to captured variables.
//!
//! A store to an upvalue, or to a local that a closure also touches, is
//! observable by code the statement list cannot see: any call, yield or
//! return between the store and the next overwrite may run that closure or
//! the parent. The syntactic passes (dead-store elimination, literal
//! propagation) therefore treat such names as live.

use crate::ast::{Expr, Stat, TableField};
use std::collections::HashSet;

/// Calls `f` on every expression directly held by `stat`, then recurses into
/// nested statement lists. Does not descend into expressions.
fn for_each_stat_expr(stat: &Stat, f: &mut dyn FnMut(&Expr)) {
    let walk = |b: &Vec<Stat>, f: &mut dyn FnMut(&Expr)| b.iter().for_each(|s| for_each_stat_expr(s, f));
    match stat {
        Stat::Local { values, .. } | Stat::Return { values } => values.iter().for_each(|e| f(e)),
        Stat::Assign { targets, values } => {
            targets.iter().for_each(|e| f(e));
            values.iter().for_each(|e| f(e));
        }
        Stat::If { condition, then_body, elseif_clauses, else_body } => {
            f(condition);
            walk(then_body, f);
            for (c, b) in elseif_clauses {
                f(c);
                walk(b, f);
            }
            if let Some(b) = else_body {
                walk(b, f);
            }
        }
        Stat::While { condition, body } => {
            f(condition);
            walk(body, f);
        }
        Stat::Repeat { body, condition } => {
            walk(body, f);
            f(condition);
        }
        Stat::NumericFor { start, stop, step, body, .. } => {
            f(start);
            f(stop);
            if let Some(s) = step {
                f(s);
            }
            walk(body, f);
        }
        Stat::GenericFor { iterators, body, .. } => {
            iterators.iter().for_each(|e| f(e));
            walk(body, f);
        }
        Stat::DoBlock { body } => walk(body, f),
        Stat::ExprStat(e) => f(e),
        Stat::LocalFunction { func, .. } => f(func),
        Stat::MethodFunction { receiver, func, .. } => {
            f(receiver);
            f(func);
        }
        Stat::Break | Stat::Continue | Stat::Comment(_) => {}
    }
}

/// Pre-order visit of `expr` and every sub-expression, including the
/// expressions inside nested function bodies.
fn visit_expr(expr: &Expr, f: &mut dyn FnMut(&Expr)) {
    f(expr);
    match expr {
        Expr::Field { object, .. } => visit_expr(object, f),
        Expr::Index { object, key } => {
            visit_expr(object, f);
            visit_expr(key, f);
        }
        Expr::BinOp { left, right, .. } => {
            visit_expr(left, f);
            visit_expr(right, f);
        }
        Expr::UnOp { operand, .. } => visit_expr(operand, f),
        Expr::Call { func, args } => {
            visit_expr(func, f);
            args.iter().for_each(|a| visit_expr(a, f));
        }
        Expr::MethodCall { object, args, .. } => {
            visit_expr(object, f);
            args.iter().for_each(|a| visit_expr(a, f));
        }
        Expr::Function { body, .. } => body.iter().for_each(|s| visit_stat(s, f)),
        Expr::Table { fields } => fields.iter().for_each(|fl| match fl {
            TableField::Sequential(e) | TableField::Named(_, e) => visit_expr(e, f),
            TableField::Indexed(k, v) => {
                visit_expr(k, f);
                visit_expr(v, f);
            }
        }),
        Expr::Ternary { cond, then_expr, else_expr } => {
            visit_expr(cond, f);
            visit_expr(then_expr, f);
            visit_expr(else_expr, f);
        }
        Expr::Paren(inner) => visit_expr(inner, f),
        Expr::Nil | Expr::Bool(_) | Expr::Number(_) | Expr::String(_) | Expr::Varargs
        | Expr::Name(_) | Expr::Vector(..) => {}
    }
}

fn visit_stat(stat: &Stat, f: &mut dyn FnMut(&Expr)) {
    for_each_stat_expr(stat, &mut |e| visit_expr(e, f));
}

/// Names a statement list declares itself: `local`, `local function`, loop
/// variables. Does not look inside nested function bodies.
fn declared_names(stmts: &[Stat], out: &mut HashSet<String>) {
    for s in stmts {
        match s {
            Stat::Local { names, .. } => out.extend(names.iter().cloned()),
            Stat::LocalFunction { name, .. } => {
                out.insert(name.clone());
            }
            Stat::NumericFor { var, body, .. } => {
                out.insert(var.clone());
                declared_names(body, out);
            }
            Stat::GenericFor { vars, body, .. } => {
                out.extend(vars.iter().cloned());
                declared_names(body, out);
            }
            Stat::If { then_body, elseif_clauses, else_body, .. } => {
                declared_names(then_body, out);
                for (_, b) in elseif_clauses {
                    declared_names(b, out);
                }
                if let Some(b) = else_body {
                    declared_names(b, out);
                }
            }
            Stat::While { body, .. } | Stat::Repeat { body, .. } | Stat::DoBlock { body } => {
                declared_names(body, out)
            }
            _ => {}
        }
    }
}

/// Every name a closure inside `stmts` refers to without declaring it: the
/// closure's upvalues (and globals). Reads and writes both count.
pub(super) fn closure_free_names(stmts: &[Stat]) -> HashSet<String> {
    let mut out = HashSet::new();
    for s in stmts {
        visit_stat(s, &mut |e| {
            if let Expr::Function { params, body, .. } = e {
                let mut declared: HashSet<String> = params.iter().cloned().collect();
                declared_names(body, &mut declared);
                let mut used = HashSet::new();
                for b in body {
                    visit_stat(b, &mut |inner| {
                        if let Expr::Name(n) = inner {
                            used.insert(n.clone());
                        }
                    });
                    collect_assign_targets(b, &mut used);
                }
                out.extend(used.into_iter().filter(|n| !declared.contains(n)));
            }
        });
        // A `local function` / method-function body is held as an Expr, so
        // `visit_stat` already reached it through `for_each_stat_expr`.
    }
    out
}

fn collect_assign_targets(stat: &Stat, out: &mut HashSet<String>) {
    // Assign targets that are plain names are not reached as Expr::Name by
    // `visit_stat` only when nested statements are skipped, so walk them here.
    match stat {
        Stat::Assign { targets, .. } => {
            for t in targets {
                if let Expr::Name(n) = t {
                    out.insert(n.clone());
                }
            }
        }
        Stat::If { then_body, elseif_clauses, else_body, .. } => {
            then_body.iter().for_each(|s| collect_assign_targets(s, out));
            for (_, b) in elseif_clauses {
                b.iter().for_each(|s| collect_assign_targets(s, out));
            }
            if let Some(b) = else_body {
                b.iter().for_each(|s| collect_assign_targets(s, out));
            }
        }
        Stat::While { body, .. }
        | Stat::Repeat { body, .. }
        | Stat::DoBlock { body }
        | Stat::NumericFor { body, .. }
        | Stat::GenericFor { body, .. } => body.iter().for_each(|s| collect_assign_targets(s, out)),
        _ => {}
    }
}

/// Can anything outside this statement list observe program state while
/// `stat` runs or right after it? True for calls, closures (which may be
/// called later), and statements that leave the block (`return`, `break`,
/// `continue`) or loop (a loop body re-reads the variable on its next trip).
/// A store to an upvalue is only dead when none of these separates it from the
/// store that overwrites it.
pub(super) fn stat_may_observe(stat: &Stat) -> bool {
    let mut observed = false;
    let any = |b: &Vec<Stat>| b.iter().any(stat_may_observe);
    let expr_observes = |e: &Expr| {
        let mut hit = false;
        visit_expr(e, &mut |x| {
            hit |= matches!(x, Expr::Call { .. } | Expr::MethodCall { .. } | Expr::Function { .. });
        });
        hit
    };
    match stat {
        Stat::Return { .. } | Stat::Break | Stat::Continue => observed = true,
        Stat::While { .. } | Stat::Repeat { .. } | Stat::NumericFor { .. } | Stat::GenericFor { .. } => {
            observed = true
        }
        Stat::LocalFunction { .. } | Stat::MethodFunction { .. } => observed = true,
        Stat::Local { values, .. } => observed = values.iter().any(expr_observes),
        Stat::Assign { targets, values } => {
            observed = targets.iter().any(expr_observes) || values.iter().any(expr_observes)
        }
        Stat::ExprStat(e) => observed = expr_observes(e),
        Stat::If { condition, then_body, elseif_clauses, else_body } => {
            observed = expr_observes(condition)
                || any(then_body)
                || elseif_clauses.iter().any(|(c, b)| expr_observes(c) || any(b))
                || else_body.as_ref().is_some_and(any);
        }
        Stat::DoBlock { body } => observed = any(body),
        Stat::Comment(_) => {}
    }
    observed
}

/// Does `expr` contain, anywhere inside it, a function body that assigns to
/// `name`? Catches `task.spawn(function() flag = true end)` and similar
/// closures passed as call arguments.
pub(super) fn expr_has_closure_writing(expr: &Expr, name: &str) -> bool {
    let mut found = false;
    visit_expr(expr, &mut |e| {
        if let Expr::Function { body, .. } = e {
            let mut targets = HashSet::new();
            body.iter().for_each(|s| collect_assign_targets(s, &mut targets));
            if targets.contains(name) {
                found = true;
            }
        }
    });
    found
}

/// Does any expression held by `stat`, or by a statement nested in it, contain
/// a function body that assigns to `name`? Unlike `stmt_writes_name` this sees
/// closures passed as call arguments.
pub(super) fn stat_has_closure_writing(stat: &Stat, name: &str) -> bool {
    let mut found = false;
    for_each_stat_expr(stat, &mut |e| found |= expr_has_closure_writing(e, name));
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(n: &str) -> Expr {
        Expr::Name(n.to_string())
    }

    fn assign(n: &str, v: Expr) -> Stat {
        Stat::Assign { targets: vec![name(n)], values: vec![v] }
    }

    #[test]
    fn closure_free_names_reports_upvalue_writes_inside_call_arguments() {
        let closure = Expr::Function { params: vec![], is_vararg: false, body: vec![assign("flag", Expr::Bool(true))] };
        let stmts = vec![Stat::ExprStat(Expr::Call { func: Box::new(name("spawn")), args: vec![closure] })];
        assert!(closure_free_names(&stmts).contains("flag"));
    }

    #[test]
    fn closure_free_names_ignores_the_closures_own_locals() {
        let body = vec![
            Stat::Local { names: vec!["x".into()], values: vec![Expr::Number(1.0)] },
            assign("x", Expr::Number(2.0)),
        ];
        let closure = Expr::Function { params: vec![], is_vararg: false, body };
        let stmts = vec![Stat::ExprStat(Expr::Call { func: Box::new(name("f")), args: vec![closure] })];
        assert!(!closure_free_names(&stmts).contains("x"));
    }

    #[test]
    fn stat_may_observe_flags_calls_and_exits_but_not_field_reads() {
        let call = Stat::ExprStat(Expr::Call { func: Box::new(name("f")), args: vec![] });
        let read = Stat::Local {
            names: vec!["p".into()],
            values: vec![Expr::Field { object: Box::new(name("game")), field: "Players".into() }],
        };
        assert!(stat_may_observe(&call));
        assert!(stat_may_observe(&Stat::Return { values: vec![] }));
        assert!(!stat_may_observe(&read));
    }

    #[test]
    fn expr_has_closure_writing_finds_nested_closure_store() {
        let closure = Expr::Function { params: vec![], is_vararg: false, body: vec![assign("flag", Expr::Bool(true))] };
        let call = Expr::Call { func: Box::new(name("spawn")), args: vec![closure] };
        assert!(expr_has_closure_writing(&call, "flag"));
        assert!(!expr_has_closure_writing(&call, "other"));
    }
}
