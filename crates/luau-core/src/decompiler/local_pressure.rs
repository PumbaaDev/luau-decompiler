//! Keep a function under Luau's limit of 200 simultaneously live locals.
//!
//! The lifter turns every value that sat in a register into a named `local`.
//! The original source held most of them as temporaries inside one expression
//! (`tbl.X = require(script:WaitForChild("X"))`), which cost no local slot. A
//! big module therefore decompiles to more than 200 locals in one scope, and
//! the real Luau compiler rejects it with
//! `Out of local registers when trying to allocate X: exceeded limit 200`.
//!
//! This pass only runs on a block whose live local count is already high
//! (`PRESSURE_TRIGGER`), so ordinary output is byte-identical. It folds a
//! single-use `local x = <init>` back into its one use, in two cases:
//!
//! 1. PURE: `init` is a side-effect-free builtin constructor over literals
//!    (`Color3.fromRGB(1, 2, 3)`, `Vector3.new(0, 1, 0)`, `Enum.A.B`). It can
//!    move to the use at any distance, because nothing can observe when it ran.
//! 2. ORDERED: the declaration is the statement directly before the one that
//!    uses it, and everything that statement evaluates BEFORE reaching the use
//!    is a literal, a plain name or a callee path (`table.insert`). Then the
//!    init runs at the same point in the call sequence as before; no call is
//!    reordered, dropped or duplicated. Chains (`a`, then `b = g(a)`, then
//!    `t.x = b`) fold one link at a time.
//!
//! Never inlined: names used more than once, used inside a closure, loop or
//! conditional branch (ORDERED mode), re-declared anywhere in the block, or
//! assigned. A call that returns several values is wrapped in `( )` when it
//! lands in a multi-value slot, so `local x = f(); g(x)` becomes `g((f()))`
//! and keeps passing one value.
//!
//! Not modelled: a global read earlier in the same statement being rewritten
//! by the folded call itself (`g(someGlobal, x)` where `x`'s init assigns
//! `someGlobal`). Locals written by closures are treated as unstable.

use crate::ast::{BinOp, Expr, Stat, TableField};
use std::collections::{HashMap, HashSet};

/// Live locals (this scope plus enclosing scopes) above which a block is
/// rewritten. Luau's hard limit is 200; this leaves room for loop variables.
pub const PRESSURE_TRIGGER: usize = 100;

const PURE_HEADS: &[&str] = &[
    "Color3", "Vector3", "Vector2", "CFrame", "UDim2", "UDim", "Rect", "ColorSequence",
    "ColorSequenceKeypoint", "NumberRange", "NumberSequence", "NumberSequenceKeypoint",
    "BrickColor",
];
const PURE_FIELDS: &[&str] = &[
    "new", "fromRGB", "fromHSV", "fromHex", "Angles", "fromEulerAnglesXYZ", "fromEulerAnglesYXZ",
    "fromOrientation", "lookAt", "fromAxisAngle", "fromScale", "fromOffset",
];

/// Entry point: rewrite `stmts` (a whole function body) in place.
pub fn relieve(stmts: &mut Vec<Stat>) {
    relieve_block(stmts, 0);
}

fn direct_locals(stmts: &[Stat]) -> usize {
    stmts
        .iter()
        .map(|s| match s {
            Stat::Local { names, .. } => names.len(),
            Stat::LocalFunction { .. } => 1,
            _ => 0,
        })
        .sum()
}

fn relieve_block(stmts: &mut Vec<Stat>, inherited: usize) {
    if inherited + direct_locals(stmts) > PRESSURE_TRIGGER {
        inline_block(stmts);
    }
    let live = inherited + direct_locals(stmts);
    for s in stmts.iter_mut() {
        stat_blocks_mut(s, &mut |b| relieve_block(b, live + 4));
        stat_exprs_mut(s, &mut |e| relieve_functions(e));
    }
}

fn relieve_functions(e: &mut Expr) {
    if let Expr::Function { params, body, .. } = e {
        let n = params.len();
        relieve_block(body, n);
        return;
    }
    expr_children_mut(e, &mut |c| relieve_functions(c));
}

// ---------------------------------------------------------------------------
// Generic mutable walkers
// ---------------------------------------------------------------------------

/// Visit the expression slots a statement owns directly (not nested blocks).
fn stat_exprs_mut(s: &mut Stat, f: &mut dyn FnMut(&mut Expr)) {
    match s {
        Stat::Local { values, .. } | Stat::Return { values } => values.iter_mut().for_each(|e| f(e)),
        Stat::Assign { targets, values } => {
            targets.iter_mut().for_each(|e| f(e));
            values.iter_mut().for_each(|e| f(e));
        }
        Stat::If { condition, elseif_clauses, .. } => {
            f(condition);
            elseif_clauses.iter_mut().for_each(|(c, _)| f(c));
        }
        Stat::While { condition, .. } | Stat::Repeat { condition, .. } => f(condition),
        Stat::NumericFor { start, stop, step, .. } => {
            f(start);
            f(stop);
            if let Some(st) = step {
                f(st);
            }
        }
        Stat::GenericFor { iterators, .. } => iterators.iter_mut().for_each(|e| f(e)),
        Stat::ExprStat(e) => f(e),
        Stat::LocalFunction { func, .. } => f(func),
        Stat::MethodFunction { receiver, func, .. } => {
            f(receiver);
            f(func);
        }
        Stat::Break | Stat::Continue | Stat::Comment(_) | Stat::DoBlock { .. } => {}
    }
}

/// Visit the nested statement blocks of a statement.
fn stat_blocks_mut(s: &mut Stat, f: &mut dyn FnMut(&mut Vec<Stat>)) {
    match s {
        Stat::If { then_body, elseif_clauses, else_body, .. } => {
            f(then_body);
            elseif_clauses.iter_mut().for_each(|(_, b)| f(b));
            if let Some(b) = else_body {
                f(b);
            }
        }
        Stat::While { body, .. }
        | Stat::Repeat { body, .. }
        | Stat::NumericFor { body, .. }
        | Stat::GenericFor { body, .. }
        | Stat::DoBlock { body } => f(body),
        _ => {}
    }
}

/// Visit the direct sub-expressions of an expression (not function bodies).
fn expr_children_mut(e: &mut Expr, f: &mut dyn FnMut(&mut Expr)) {
    match e {
        Expr::Field { object, .. } => f(object),
        Expr::Index { object, key } => {
            f(object);
            f(key);
        }
        Expr::BinOp { left, right, .. } => {
            f(left);
            f(right);
        }
        Expr::UnOp { operand, .. } => f(operand),
        Expr::Call { func, args } => {
            f(func);
            args.iter_mut().for_each(|a| f(a));
        }
        Expr::MethodCall { object, args, .. } => {
            f(object);
            args.iter_mut().for_each(|a| f(a));
        }
        Expr::Table { fields } => {
            for fl in fields {
                match fl {
                    TableField::Sequential(v) | TableField::Named(_, v) => f(v),
                    TableField::Indexed(k, v) => {
                        f(k);
                        f(v);
                    }
                }
            }
        }
        Expr::Ternary { cond, then_expr, else_expr } => {
            f(cond);
            f(then_expr);
            f(else_expr);
        }
        Expr::Paren(inner) => f(inner),
        Expr::Nil
        | Expr::Bool(_)
        | Expr::Number(_)
        | Expr::String(_)
        | Expr::Varargs
        | Expr::Name(_)
        | Expr::Vector(..)
        | Expr::Function { .. } => {}
    }
}

// ---------------------------------------------------------------------------
// Census
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Census {
    /// `Expr::Name` occurrences, deep (closures included, assignment targets included).
    uses: HashMap<String, usize>,
    /// Declarations of each name (locals, params, loop variables), deep.
    decls: HashMap<String, usize>,
    /// Names assigned from inside some closure body.
    closure_written: HashSet<String>,
}

fn census_block(stmts: &mut [Stat], c: &mut Census, in_closure: bool) {
    for s in stmts.iter_mut() {
        census_stat(s, c, in_closure);
    }
}

fn declare(c: &mut Census, name: &str) {
    *c.decls.entry(name.to_string()).or_insert(0) += 1;
}

fn census_stat(s: &mut Stat, c: &mut Census, in_closure: bool) {
    match s {
        Stat::Local { names, .. } => names.iter().for_each(|n| declare(c, n)),
        Stat::LocalFunction { name, .. } => declare(c, name),
        Stat::NumericFor { var, .. } => declare(c, var),
        Stat::GenericFor { vars, .. } => vars.iter().for_each(|n| declare(c, n)),
        Stat::Assign { targets, .. } if in_closure => {
            for t in targets.iter() {
                if let Expr::Name(n) = t {
                    c.closure_written.insert(n.clone());
                }
            }
        }
        _ => {}
    }
    stat_exprs_mut(s, &mut |e| census_expr(e, c, in_closure));
    stat_blocks_mut(s, &mut |b| census_block(b, c, in_closure));
}

fn census_expr(e: &mut Expr, c: &mut Census, in_closure: bool) {
    match e {
        Expr::Name(n) => *c.uses.entry(n.clone()).or_insert(0) += 1,
        Expr::Function { params, body, .. } => {
            params.iter().for_each(|p| declare(c, p));
            census_block(body, c, true);
        }
        _ => expr_children_mut(e, &mut |ch| census_expr(ch, c, in_closure)),
    }
}

/// Names a statement mentions (deep), used to find a name's first user.
fn names_in_stat(s: &mut Stat, out: &mut HashSet<String>) {
    stat_exprs_mut(s, &mut |e| names_in_expr(e, out));
    stat_blocks_mut(s, &mut |b| b.iter_mut().for_each(|x| names_in_stat(x, out)));
}

fn names_in_expr(e: &mut Expr, out: &mut HashSet<String>) {
    match e {
        Expr::Name(n) => {
            out.insert(n.clone());
        }
        Expr::Function { body, .. } => body.iter_mut().for_each(|x| names_in_stat(x, out)),
        _ => expr_children_mut(e, &mut |ch| names_in_expr(ch, out)),
    }
}

// ---------------------------------------------------------------------------
// Pure builtin initialisers
// ---------------------------------------------------------------------------

fn is_pure_arg(a: &Expr, heads: &mut Vec<String>) -> bool {
    match a {
        Expr::Number(_) | Expr::String(_) | Expr::Bool(_) | Expr::Nil => true,
        Expr::UnOp { op: crate::ast::UnOp::Negate, operand } => matches!(**operand, Expr::Number(_)),
        other => is_pure_init(other, heads),
    }
}

/// A side-effect-free builtin constructor. Pushes the global names it reads.
fn is_pure_init(e: &Expr, heads: &mut Vec<String>) -> bool {
    match e {
        Expr::Vector(..) => {
            heads.push("Vector3".to_string());
            true
        }
        Expr::Call { func, args } => {
            let Expr::Field { object, field } = &**func else { return false };
            let Expr::Name(h) = &**object else { return false };
            if !PURE_HEADS.contains(&h.as_str()) || !PURE_FIELDS.contains(&field.as_str()) {
                return false;
            }
            heads.push(h.clone());
            args.iter().all(|a| is_pure_arg(a, heads))
        }
        Expr::Field { .. } => {
            // Enum.Category.Item
            let mut cur = e;
            while let Expr::Field { object, .. } = cur {
                cur = object;
            }
            if matches!(cur, Expr::Name(n) if n == "Enum") {
                heads.push("Enum".to_string());
                true
            } else {
                false
            }
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Replacement engine
// ---------------------------------------------------------------------------

struct Rep<'a> {
    name: &'a str,
    repl: &'a Expr,
    /// ORDERED mode tracks evaluation order; PURE mode ignores it.
    ordered: bool,
    repl_multi: bool,
    repl_prefix_ok: bool,
    closure_written: &'a HashSet<String>,
    unstable: bool,
    done: bool,
    failed: bool,
}

impl Rep<'_> {
    fn fail(&mut self) {
        self.failed = true;
    }

    fn mark_unstable(&mut self) {
        if self.ordered {
            self.unstable = true;
        }
    }

    fn expr(&mut self, e: &mut Expr, tail: bool, prefix: bool, callee: bool, cond: bool) {
        if self.done || self.failed {
            return;
        }
        match e {
            Expr::Name(n) if n == self.name => {
                if self.ordered && (self.unstable || cond) {
                    return self.fail();
                }
                if prefix && !self.repl_prefix_ok {
                    return self.fail();
                }
                let mut v = self.repl.clone();
                if tail && self.repl_multi {
                    v = Expr::Paren(Box::new(v));
                }
                *e = v;
                self.done = true;
            }
            Expr::Name(n) => {
                if self.ordered && self.closure_written.contains(n.as_str()) {
                    self.unstable = true;
                }
            }
            Expr::Nil
            | Expr::Bool(_)
            | Expr::Number(_)
            | Expr::String(_)
            | Expr::Varargs
            | Expr::Vector(..) => {}
            Expr::Function { body, .. } => {
                if !self.ordered {
                    for s in body.iter_mut() {
                        self.pure_stat(s);
                    }
                }
            }
            Expr::Field { object, .. } => {
                self.expr(object, false, true, callee, cond);
                if !callee {
                    self.mark_unstable();
                }
            }
            Expr::Index { object, key } => {
                self.expr(object, false, true, callee, cond);
                self.expr(key, false, false, false, cond);
                if !callee {
                    self.mark_unstable();
                }
            }
            Expr::BinOp { left, op, right } => {
                let short = matches!(op, BinOp::And | BinOp::Or);
                self.expr(left, false, false, false, cond);
                self.expr(right, false, false, false, cond || short);
                self.mark_unstable();
            }
            Expr::UnOp { operand, .. } => {
                self.expr(operand, false, false, false, cond);
                self.mark_unstable();
            }
            Expr::Call { func, args } => {
                self.expr(func, false, true, true, cond);
                let last = args.len().saturating_sub(1);
                for (k, a) in args.iter_mut().enumerate() {
                    self.expr(a, k == last, false, false, cond);
                }
                self.mark_unstable();
            }
            Expr::MethodCall { object, args, .. } => {
                self.expr(object, false, true, true, cond);
                let last = args.len().saturating_sub(1);
                for (k, a) in args.iter_mut().enumerate() {
                    self.expr(a, k == last, false, false, cond);
                }
                self.mark_unstable();
            }
            Expr::Table { fields } => {
                let last = fields.len().saturating_sub(1);
                for (k, fl) in fields.iter_mut().enumerate() {
                    match fl {
                        TableField::Sequential(v) => self.expr(v, k == last, false, false, cond),
                        TableField::Named(_, v) => self.expr(v, false, false, false, cond),
                        TableField::Indexed(kk, v) => {
                            self.expr(kk, false, false, false, cond);
                            self.expr(v, false, false, false, cond);
                        }
                    }
                }
            }
            Expr::Ternary { cond: c, then_expr, else_expr } => {
                self.expr(c, false, false, false, cond);
                self.expr(then_expr, false, false, false, true);
                self.expr(else_expr, false, false, false, true);
                self.mark_unstable();
            }
            Expr::Paren(inner) => self.expr(inner, false, false, false, cond),
        }
    }

    /// Assignment target: a bare name is a write (never replaced); a field or
    /// index path is evaluated like a callee path.
    fn target(&mut self, t: &mut Expr) {
        match t {
            Expr::Name(n) if n == self.name => self.fail(),
            Expr::Name(_) => {}
            Expr::Field { .. } | Expr::Index { .. } => self.expr(t, false, true, true, false),
            _ => self.fail(),
        }
    }

    /// PURE mode: any statement, any depth.
    fn pure_stat(&mut self, s: &mut Stat) {
        if self.done || self.failed {
            return;
        }
        if let Stat::Assign { targets, values } = s {
            for t in targets.iter_mut() {
                self.target(t);
            }
            for v in values.iter_mut() {
                self.expr(v, false, false, false, false);
            }
            return;
        }
        stat_exprs_mut(s, &mut |e| self.expr(e, false, false, false, false));
        stat_blocks_mut(s, &mut |b| {
            for x in b.iter_mut() {
                self.pure_stat(x);
            }
        });
    }

    /// ORDERED mode: only plain straight-line statements.
    fn ordered_stat(&mut self, s: &mut Stat) {
        match s {
            Stat::Local { names, values } => {
                let last = values.len().saturating_sub(1);
                let multi_tail = values.len() < names.len();
                for (k, v) in values.iter_mut().enumerate() {
                    self.expr(v, k == last && multi_tail, false, false, false);
                }
            }
            Stat::Assign { targets, values } => {
                for t in targets.iter_mut() {
                    self.target(t);
                }
                let last = values.len().saturating_sub(1);
                let multi_tail = values.len() < targets.len();
                for (k, v) in values.iter_mut().enumerate() {
                    self.expr(v, k == last && multi_tail, false, false, false);
                }
            }
            Stat::Return { values } => {
                let last = values.len().saturating_sub(1);
                for (k, v) in values.iter_mut().enumerate() {
                    self.expr(v, k == last, false, false, false);
                }
            }
            Stat::ExprStat(e) => self.expr(e, false, false, false, false),
            _ => self.fail(),
        }
    }
}

fn repl_flags(init: &Expr) -> (bool, bool) {
    let multi = matches!(init, Expr::Call { .. } | Expr::MethodCall { .. } | Expr::Varargs);
    let prefix_ok = matches!(
        init,
        Expr::Name(_) | Expr::Field { .. } | Expr::Index { .. } | Expr::Call { .. } | Expr::MethodCall { .. } | Expr::Paren(_)
    );
    (multi, prefix_ok)
}

fn single_local(s: &Stat) -> Option<(&String, &Expr)> {
    match s {
        Stat::Local { names, values } if names.len() == 1 && values.len() == 1 => Some((&names[0], &values[0])),
        _ => None,
    }
}

fn inline_block(stmts: &mut Vec<Stat>) {
    let mut c = Census::default();
    census_block(stmts, &mut c, false);
    let n = stmts.len();
    let mut dead = vec![false; n];

    // First statement that mentions each name.
    let mut first_use: HashMap<String, usize> = HashMap::new();
    for (j, s) in stmts.iter_mut().enumerate() {
        let mut names = HashSet::new();
        names_in_stat(s, &mut names);
        for nm in names {
            first_use.entry(nm).or_insert(j);
        }
    }

    // Phase 1: pure builtin constructors, any distance.
    for i in 0..n {
        let Some((name, init)) = single_local(&stmts[i]) else { continue };
        let (name, init) = (name.clone(), init.clone());
        if c.uses.get(&name) != Some(&1) || c.decls.get(&name) != Some(&1) {
            continue;
        }
        let mut heads = Vec::new();
        if !is_pure_init(&init, &mut heads) {
            continue;
        }
        if heads.iter().any(|h| c.decls.contains_key(h)) {
            continue;
        }
        let Some(&j) = first_use.get(&name) else { continue };
        if j <= i || dead[j] {
            continue;
        }
        let (_, prefix_ok) = repl_flags(&init);
        let mut rep = Rep {
            name: &name,
            repl: &init,
            ordered: false,
            repl_multi: false,
            repl_prefix_ok: prefix_ok,
            closure_written: &c.closure_written,
            unstable: false,
            done: false,
            failed: false,
        };
        rep.pure_stat(&mut stmts[j]);
        if rep.done && !rep.failed {
            dead[i] = true;
        }
    }

    // Phase 2: adjacent single-use temporaries, in evaluation order.
    for j in 0..n {
        if dead[j] {
            continue;
        }
        loop {
            let Some(p) = (0..j).rev().find(|&k| !dead[k]) else { break };
            let Some((name, init)) = single_local(&stmts[p]) else { break };
            let (name, init) = (name.clone(), init.clone());
            if c.uses.get(&name) != Some(&1) || c.decls.get(&name) != Some(&1) {
                break;
            }
            let (multi, prefix_ok) = repl_flags(&init);
            let mut rep = Rep {
                name: &name,
                repl: &init,
                ordered: true,
                repl_multi: multi,
                repl_prefix_ok: prefix_ok,
                closure_written: &c.closure_written,
                unstable: false,
                done: false,
                failed: false,
            };
            rep.ordered_stat(&mut stmts[j]);
            if rep.done && !rep.failed {
                dead[p] = true;
            } else {
                break;
            }
        }
    }

    let mut k = 0;
    stmts.retain(|_| {
        let keep = !dead[k];
        k += 1;
        keep
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decompiler::emit::emit_block;

    fn name(n: &str) -> Expr {
        Expr::Name(n.to_string())
    }
    fn field(o: Expr, f: &str) -> Expr {
        Expr::Field { object: Box::new(o), field: f.to_string() }
    }
    fn call(f: Expr, args: Vec<Expr>) -> Expr {
        Expr::Call { func: Box::new(f), args }
    }
    fn local(n: &str, v: Expr) -> Stat {
        Stat::Local { names: vec![n.to_string()], values: vec![v] }
    }
    fn color(r: f64) -> Expr {
        call(field(name("Color3"), "fromRGB"), vec![Expr::Number(r), Expr::Number(2.0), Expr::Number(3.0)])
    }
    fn pad(count: usize) -> Vec<Stat> {
        (0..count).map(|k| local(&format!("pad{k}"), Expr::Number(k as f64))).collect()
    }
    fn render(stmts: &[Stat]) -> String {
        let mut out = String::new();
        emit_block(&mut out, stmts, 0);
        out
    }

    #[test]
    fn pure_constructors_fold_into_their_one_use_when_pressure_is_high() {
        let mut stmts = pad(PRESSURE_TRIGGER);
        let mut fields = Vec::new();
        for k in 0..30 {
            let n = format!("Col{k}");
            stmts.push(local(&n, color(k as f64)));
            fields.push(TableField::Named(format!("F{k}"), name(&n)));
        }
        stmts.push(Stat::Return { values: vec![Expr::Table { fields }] });
        relieve(&mut stmts);
        let out = render(&stmts);
        assert!(!out.contains("local Col"), "temporaries should be folded:
{out}");
        assert!(out.contains("F29 = Color3.fromRGB(29, 2, 3)"), "{out}");
    }

    #[test]
    fn nothing_changes_below_the_trigger() {
        let mut stmts = pad(10);
        stmts.push(local("Color1", color(1.0)));
        stmts.push(Stat::Return { values: vec![name("Color1")] });
        let before = render(&stmts);
        relieve(&mut stmts);
        assert_eq!(render(&stmts), before);
    }

    #[test]
    fn adjacent_chain_folds_in_call_order_and_keeps_single_value_truncation() {
        let mut stmts = pad(PRESSURE_TRIGGER);
        stmts.push(local("tbl", Expr::Table { fields: vec![] }));
        stmts.push(local(
            "Mod",
            Expr::MethodCall { object: Box::new(name("script")), method: "WaitForChild".into(), args: vec![Expr::String("X".into())] },
        ));
        stmts.push(local("Mod2", call(name("require"), vec![name("Mod")])));
        stmts.push(Stat::Assign { targets: vec![field(name("tbl"), "X")], values: vec![name("Mod2")] });
        relieve(&mut stmts);
        let out = render(&stmts);
        assert!(out.contains("tbl.X = require((script:WaitForChild(\"X\")))"), "{out}");
        assert!(!out.contains("local Mod"), "{out}");
    }

    #[test]
    fn a_call_is_never_moved_past_another_call() {
        // local a = f(); local b = g(); h(b, a)  ->  a must stay: g() would otherwise run before f().
        let mut stmts = pad(PRESSURE_TRIGGER);
        stmts.push(local("a", call(name("f"), vec![])));
        stmts.push(local("b", call(name("g"), vec![])));
        stmts.push(Stat::ExprStat(call(name("h"), vec![name("b"), name("a")])));
        relieve(&mut stmts);
        let out = render(&stmts);
        let f_at = out.find("f()").expect("f call kept");
        let g_at = out.find("g()").expect("g call kept");
        assert!(f_at < g_at, "f must still run before g:
{out}");
        assert!(out.contains("local a = f()"), "{out}");
        assert_eq!(out.matches("f()").count(), 1);
        assert_eq!(out.matches("g()").count(), 1);
    }

    #[test]
    fn multi_use_and_closure_captured_locals_stay() {
        let mut stmts = pad(PRESSURE_TRIGGER);
        stmts.push(local("twice", call(name("f"), vec![])));
        stmts.push(Stat::ExprStat(call(name("h"), vec![name("twice"), name("twice")])));
        stmts.push(local("cap", call(name("g"), vec![])));
        stmts.push(Stat::ExprStat(call(
            name("spawn"),
            vec![Expr::Function {
                params: vec![],
                is_vararg: false,
                body: vec![Stat::ExprStat(call(name("use"), vec![name("cap")]))],
            }],
        )));
        relieve(&mut stmts);
        let out = render(&stmts);
        assert!(out.contains("local twice = f()"), "{out}");
        assert!(out.contains("local cap = g()"), "{out}");
    }

    #[test]
    fn a_shadowed_constructor_head_is_not_folded() {
        let mut stmts = pad(PRESSURE_TRIGGER);
        stmts.push(local("Color3", name("Other")));
        stmts.push(local("c", color(5.0)));
        stmts.push(Stat::ExprStat(call(name("between"), vec![])));
        stmts.push(Stat::Return { values: vec![name("c"), name("Color3")] });
        relieve(&mut stmts);
        let out = render(&stmts);
        assert!(out.contains("local c = Color3.fromRGB(5, 2, 3)"), "{out}");
    }
}
