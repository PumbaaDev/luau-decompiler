//! Scope repair for retired upvalue-alias names.
//!
//! When the lifter retires an upvalue alias (see `naming::retired_fresh_alias`)
//! the register takes a fresh name that is declared `local` at the write that
//! retired it, which may sit in an inner scope. Later code for the same
//! register can then fall outside that scope:
//!
//!  * a bare write (`name = value`), possibly inside a closure that captured
//!    the register, would target a global, because the only declaration is in
//!    a sibling or inner block;
//!  * a read would name something no enclosing scope declares.
//!
//! This pass runs once per proto over its finished statements, including the
//! closure bodies nested in them, and repairs both:
//!
//!  * a name with an out-of-scope WRITE is hoisted: every `local name = v` of
//!    it becomes `name = v` and a single `local name` is placed at the top of
//!    the proto, which dominates every use (the outermost scope covering all
//!    uses). A declaration shared by several names (`local a, name = f()`)
//!    is split into `local a` and `a, name = f()`;
//!  * a name with only out-of-scope READS (no out-of-scope write) has no value
//!    there: the reads fall back to the upvalue alias the register carried before the
//!    retire, which is what the code read before the retire existed.

use crate::ast::{Expr, Stat, TableField};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Collect,
    Rewrite,
}

struct State<'a> {
    mode: Mode,
    /// Retired fresh name -> the upvalue alias it was split off from.
    retired: &'a HashMap<String, String>,
    scopes: Vec<HashSet<String>>,
    /// Retired names a `local` in this proto has declared so far.
    seen: HashSet<String>,
    /// Retired names with a `local` site anywhere in the proto.
    declared: HashSet<String>,
    outside_write: HashSet<String>,
    /// Names to hoist (Rewrite mode only).
    hoist: HashSet<String>,
}

impl State<'_> {
    fn visible(&self, name: &str) -> bool {
        self.scopes.iter().any(|s| s.contains(name))
    }

    fn declare(&mut self, name: &str) {
        if self.retired.contains_key(name) {
            self.seen.insert(name.to_string());
            self.declared.insert(name.to_string());
        }
        if let Some(top) = self.scopes.last_mut() {
            top.insert(name.to_string());
        }
    }
}

/// Repair out-of-scope uses of this proto's retired names (see module docs).
pub(super) fn fix_retired_scopes(stmts: &mut Vec<Stat>, retired: &HashMap<String, String>) {
    if retired.is_empty() {
        return;
    }
    let mut collect = State {
        mode: Mode::Collect,
        retired,
        scopes: vec![HashSet::new()],
        seen: HashSet::new(),
        declared: HashSet::new(),
        outside_write: HashSet::new(),
        hoist: HashSet::new(),
    };
    walk_block(stmts, &mut collect, false);
    let hoist: HashSet<String> = collect
        .outside_write
        .iter()
        .filter(|n| collect.declared.contains(*n))
        .cloned()
        .collect();

    let mut rewrite = State {
        mode: Mode::Rewrite,
        retired,
        scopes: vec![hoist.clone()],
        seen: HashSet::new(),
        declared: HashSet::new(),
        outside_write: HashSet::new(),
        hoist: hoist.clone(),
    };
    walk_block(stmts, &mut rewrite, false);

    if !hoist.is_empty() {
        let mut names: Vec<String> = hoist.into_iter().collect();
        names.sort();
        stmts.insert(0, Stat::Local { names, values: vec![] });
    }
}

fn walk_block(stmts: &mut Vec<Stat>, st: &mut State, in_open_scope: bool) {
    if !in_open_scope {
        st.scopes.push(HashSet::new());
    }
    if st.mode == Mode::Rewrite {
        split_hoisted_shared_locals(stmts, &st.hoist);
    }
    for stmt in stmts.iter_mut() {
        walk_stmt(stmt, st);
    }
    if !in_open_scope {
        st.scopes.pop();
    }
}

/// `local a, hoisted = f()` -> `local a` + `a, hoisted = f()`, so the hoisted
/// name can be declared once at the top of the proto.
fn split_hoisted_shared_locals(stmts: &mut Vec<Stat>, hoist: &HashSet<String>) {
    if hoist.is_empty() {
        return;
    }
    let mut out = Vec::with_capacity(stmts.len());
    for stmt in stmts.drain(..) {
        match stmt {
            Stat::Local { names, values } if names.len() > 1 && names.iter().any(|n| hoist.contains(n)) => {
                let kept: Vec<String> = names.iter().filter(|n| !hoist.contains(*n)).cloned().collect();
                if !kept.is_empty() {
                    out.push(Stat::Local { names: kept, values: vec![] });
                }
                if !values.is_empty() {
                    let targets = names.into_iter().map(Expr::Name).collect();
                    out.push(Stat::Assign { targets, values });
                }
            }
            other => out.push(other),
        }
    }
    *stmts = out;
}

fn walk_stmt(stmt: &mut Stat, st: &mut State) {
    match stmt {
        Stat::Local { names, values } => {
            values.iter_mut().for_each(|v| walk_expr(v, st));
            let hoisted = names.len() == 1 && st.mode == Mode::Rewrite && st.hoist.contains(&names[0]);
            if hoisted && values.len() <= 1 {
                let name = names[0].clone();
                let value = values.pop().unwrap_or(Expr::Nil);
                *stmt = Stat::Assign { targets: vec![Expr::Name(name)], values: vec![value] };
                return;
            }
            for n in names.iter() {
                st.declare(n);
            }
        }
        Stat::Assign { targets, values } => {
            values.iter_mut().for_each(|v| walk_expr(v, st));
            for t in targets.iter_mut() {
                match t {
                    Expr::Name(n) => {
                        if st.mode == Mode::Collect && st.retired.contains_key(n.as_str()) && !st.visible(n) {
                            st.outside_write.insert(n.clone());
                        }
                    }
                    other => walk_expr(other, st),
                }
            }
        }
        Stat::If { condition, then_body, elseif_clauses, else_body } => {
            walk_expr(condition, st);
            walk_block(then_body, st, false);
            for (c, body) in elseif_clauses.iter_mut() {
                walk_expr(c, st);
                walk_block(body, st, false);
            }
            if let Some(body) = else_body {
                walk_block(body, st, false);
            }
        }
        Stat::While { condition, body } => {
            walk_expr(condition, st);
            walk_block(body, st, false);
        }
        Stat::Repeat { body, condition } => {
            st.scopes.push(HashSet::new());
            walk_block(body, st, true);
            walk_expr(condition, st);
            st.scopes.pop();
        }
        Stat::NumericFor { var, start, stop, step, body } => {
            walk_expr(start, st);
            walk_expr(stop, st);
            if let Some(s) = step {
                walk_expr(s, st);
            }
            st.scopes.push(HashSet::from([var.clone()]));
            walk_block(body, st, false);
            st.scopes.pop();
        }
        Stat::GenericFor { vars, iterators, body } => {
            iterators.iter_mut().for_each(|e| walk_expr(e, st));
            st.scopes.push(vars.iter().cloned().collect());
            walk_block(body, st, false);
            st.scopes.pop();
        }
        Stat::Return { values } => values.iter_mut().for_each(|v| walk_expr(v, st)),
        Stat::DoBlock { body } => walk_block(body, st, false),
        Stat::ExprStat(e) => walk_expr(e, st),
        Stat::LocalFunction { name, func } => {
            st.declare(name);
            walk_expr(func, st);
        }
        Stat::MethodFunction { receiver, func, .. } => {
            walk_expr(receiver, st);
            walk_expr(func, st);
        }
        Stat::Break | Stat::Continue | Stat::Comment(_) => {}
    }
}

fn walk_expr(expr: &mut Expr, st: &mut State) {
    match expr {
        Expr::Name(n) => {
            let hidden = st.mode == Mode::Rewrite
                && st.retired.contains_key(n.as_str())
                && st.seen.contains(n.as_str())
                && !st.visible(n);
            if hidden {
                if let Some(alias) = st.retired.get(n.as_str()) {
                    *n = alias.clone();
                }
            }
        }
        Expr::Field { object, .. } => walk_expr(object, st),
        Expr::Index { object, key } => {
            walk_expr(object, st);
            walk_expr(key, st);
        }
        Expr::BinOp { left, right, .. } => {
            walk_expr(left, st);
            walk_expr(right, st);
        }
        Expr::UnOp { operand, .. } => walk_expr(operand, st),
        Expr::Call { func, args } => {
            walk_expr(func, st);
            args.iter_mut().for_each(|a| walk_expr(a, st));
        }
        Expr::MethodCall { object, args, .. } => {
            walk_expr(object, st);
            args.iter_mut().for_each(|a| walk_expr(a, st));
        }
        Expr::Function { params, body, .. } => {
            st.scopes.push(params.iter().cloned().collect());
            walk_block(body, st, false);
            st.scopes.pop();
        }
        Expr::Table { fields } => {
            for f in fields.iter_mut() {
                match f {
                    TableField::Sequential(e) | TableField::Named(_, e) => walk_expr(e, st),
                    TableField::Indexed(k, v) => {
                        walk_expr(k, st);
                        walk_expr(v, st);
                    }
                }
            }
        }
        Expr::Ternary { cond, then_expr, else_expr } => {
            walk_expr(cond, st);
            walk_expr(then_expr, st);
            walk_expr(else_expr, st);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(n: &str) -> Expr {
        Expr::Name(n.to_string())
    }

    fn retired(fresh: &str, alias: &str) -> HashMap<String, String> {
        HashMap::from([(fresh.to_string(), alias.to_string())])
    }

    fn local(n: &str, v: Expr) -> Stat {
        Stat::Local { names: vec![n.into()], values: vec![v] }
    }

    fn if_then(body: Vec<Stat>) -> Stat {
        Stat::If { condition: name("c"), then_body: body, elseif_clauses: vec![], else_body: None }
    }

    /// `if c then local t = a end; t = b` writes a global: the declaration is
    /// hoisted to the top of the proto and the inner `local` becomes a write.
    #[test]
    fn bare_write_outside_the_declaring_scope_hoists_the_declaration() {
        let mut stmts = vec![
            if_then(vec![local("v342", name("a"))]),
            Stat::Assign { targets: vec![name("v342")], values: vec![name("b")] },
        ];
        fix_retired_scopes(&mut stmts, &retired("v342", "v34"));
        assert!(matches!(&stmts[0], Stat::Local { names, values } if names == &vec!["v342".to_string()] && values.is_empty()));
        match &stmts[1] {
            Stat::If { then_body, .. } => assert!(matches!(&then_body[0], Stat::Assign { .. })),
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(&stmts[2], Stat::Assign { .. }));
    }

    /// A closure in a sibling branch that writes the name is an out-of-scope
    /// write too: the declaration must be hoisted above both branches.
    #[test]
    fn closure_write_in_a_sibling_scope_hoists_the_declaration() {
        let closure = Expr::Function {
            params: vec![],
            is_vararg: false,
            body: vec![Stat::Assign { targets: vec![name("v342")], values: vec![name("k")] }],
        };
        let mut stmts = vec![
            if_then(vec![local("v342", name("a"))]),
            if_then(vec![Stat::ExprStat(Expr::Call { func: Box::new(name("f")), args: vec![closure] })]),
        ];
        fix_retired_scopes(&mut stmts, &retired("v342", "v34"));
        assert!(matches!(&stmts[0], Stat::Local { names, .. } if names == &vec!["v342".to_string()]));
    }

    /// A read where no declaration is visible falls back to the alias.
    #[test]
    fn read_of_an_out_of_scope_retired_name_falls_back_to_the_alias() {
        let mut stmts = vec![
            if_then(vec![local("v4044", name("a"))]),
            local("x", Expr::Index { object: Box::new(name("v4044")), key: Box::new(name("k")) }),
        ];
        fix_retired_scopes(&mut stmts, &retired("v4044", "v40"));
        assert_eq!(stmts.len(), 2);
        match &stmts[1] {
            Stat::Local { values, .. } => {
                assert!(matches!(&values[0], Expr::Index { object, .. } if matches!(&**object, Expr::Name(n) if n == "v40")))
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    /// A name declared in an enclosing scope is untouched.
    #[test]
    fn retired_name_visible_in_scope_is_left_alone() {
        let mut stmts = vec![
            local("v9", name("a")),
            Stat::Assign { targets: vec![name("v9")], values: vec![name("b")] },
        ];
        fix_retired_scopes(&mut stmts, &retired("v9", "v1"));
        assert_eq!(stmts.len(), 2);
        assert!(matches!(&stmts[0], Stat::Local { values, .. } if values.len() == 1));
    }

    /// A name that shares its `local` with other names is split off so it can
    /// be declared once at the top.
    #[test]
    fn shared_declaration_is_split_and_hoisted() {
        let mut stmts = vec![
            if_then(vec![Stat::Local { names: vec!["v7".into(), "z".into()], values: vec![name("a")] }]),
            Stat::Assign { targets: vec![name("v7")], values: vec![name("b")] },
        ];
        fix_retired_scopes(&mut stmts, &retired("v7", "v1"));
        assert!(matches!(&stmts[0], Stat::Local { names, values } if names == &vec!["v7".to_string()] && values.is_empty()));
        match &stmts[1] {
            Stat::If { then_body, .. } => {
                assert!(matches!(&then_body[0], Stat::Local { names, values } if names == &vec!["z".to_string()] && values.is_empty()));
                assert!(matches!(&then_body[1], Stat::Assign { targets, .. } if targets.len() == 2));
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
