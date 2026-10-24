//! Fold the PRELUDE of a short-circuit operand into one expression.
//!
//! `a and F.x:Get()` compiles to a chain whose right operand is several
//! instructions (`GETUPVAL; GETTABLEKS; NAMECALL; CALL`). Lifted on its own the
//! operand leaves temp locals behind (`local T = F.x; local R = T:Get()`), and
//! the chain fold used to refuse anything that was not exactly one statement,
//! dropping the operand. This module inlines those single-use temps back into
//! the operand, but ONLY when doing so cannot change evaluation order.
//!
//! A temp declared before the operand is evaluated first. Moving its value to
//! the temp's single use site is only safe when nothing evaluated earlier in
//! the merged expression could observe, or be observed by, that value:
//!
//! * a value containing a call may not jump over any other call or read;
//! * a value containing a read may not jump over a call;
//! * a value with side effects may not move into a conditionally evaluated
//!   position (the right side of `and`/`or`, or a ternary branch), where it
//!   would stop running on some paths.

use super::{count_name_reads_in_expr, replace_name_in_expr};
use crate::ast::{BinOp, Expr, Stat, TableField};

/// What evaluating an expression can do, ignoring closure bodies (building a
/// closure runs nothing).
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct Effects {
    /// Contains a call or method call.
    call: bool,
    /// Contains a table read (`.field`, `[key]`), which may hit `__index`.
    read: bool,
}

impl Effects {
    fn or(self, other: Effects) -> Effects {
        Effects { call: self.call || other.call, read: self.read || other.read }
    }
}

fn effects_of(e: &Expr) -> Effects {
    let mut fx = children(e).into_iter().fold(Effects::default(), |acc, c| acc.or(effects_of(c)));
    match e {
        Expr::Call { .. } | Expr::MethodCall { .. } => fx.call = true,
        Expr::Field { .. } | Expr::Index { .. } => fx.read = true,
        _ => {}
    }
    fx
}

/// Does evaluating `e` read a table field or call something? Such a value
/// parked lazily in a register must keep its place relative to later calls.
pub(super) fn reads_or_calls(e: &Expr) -> bool {
    let fx = effects_of(e);
    fx.call || fx.read
}

/// Number of calls and method calls in `e` (closure bodies not counted).
pub(super) fn call_count(e: &Expr) -> usize {
    let own = usize::from(matches!(e, Expr::Call { .. } | Expr::MethodCall { .. }));
    own + children(e).into_iter().map(call_count).sum::<usize>()
}

/// The direct sub-expressions of `e`, in evaluation order.
fn children(e: &Expr) -> Vec<&Expr> {
    match e {
        Expr::Field { object, .. } => vec![object],
        Expr::Index { object, key } => vec![object, key],
        Expr::BinOp { left, right, .. } => vec![left, right],
        Expr::UnOp { operand, .. } => vec![operand],
        Expr::Call { func, args } => std::iter::once(&**func).chain(args.iter()).collect(),
        Expr::MethodCall { object, args, .. } => {
            std::iter::once(&**object).chain(args.iter()).collect()
        }
        Expr::Table { fields } => fields
            .iter()
            .flat_map(|field| match field {
                TableField::Sequential(v) | TableField::Named(_, v) => vec![v],
                TableField::Indexed(k, v) => vec![k, v],
            })
            .collect(),
        Expr::Ternary { cond, then_expr, else_expr } => vec![cond, then_expr, else_expr],
        _ => Vec::new(),
    }
}

/// Where the single read of a name sits in evaluation order.
#[derive(Debug, PartialEq, Eq)]
struct Site {
    /// Effects of everything evaluated before the read.
    before: Effects,
    /// The read is only evaluated on some paths.
    conditional: bool,
}

/// Find the read of `name` in `e`, accumulating what runs before it.
fn locate(e: &Expr, name: &str, before: &mut Effects, conditional: bool) -> Option<Site> {
    if let Expr::Name(n) = e {
        return (n == name).then_some(Site { before: *before, conditional });
    }
    // Children in evaluation order; the node's own effect happens after them.
    let mut kids: Vec<(&Expr, bool)> = Vec::new();
    match e {
        Expr::BinOp { left, op, right } => {
            let short = matches!(op, BinOp::And | BinOp::Or);
            kids.push((left, false));
            kids.push((right, short));
        }
        Expr::Ternary { cond, then_expr, else_expr } => {
            kids.push((cond, false));
            kids.push((then_expr, true));
            kids.push((else_expr, true));
        }
        _ => kids.extend(children(e).into_iter().map(|c| (c, false))),
    }
    for (child, cond) in kids {
        if let Some(site) = locate(child, name, before, conditional || cond) {
            return Some(site);
        }
        *before = before.or(effects_of(child));
    }
    None
}

/// May a value with effects `value` move to `site` without changing the order
/// in which calls and reads happen?
fn move_is_safe(value: Effects, site: &Site) -> bool {
    if (value.call || value.read) && site.conditional {
        return false;
    }
    if value.call && (site.before.call || site.before.read) {
        return false;
    }
    if value.read && site.before.call {
        return false;
    }
    true
}

/// Fold a short-circuit operand whose lift left a PRELUDE of single-use temp
/// locals (`local T = F.x; local R = T:Get()`) into one expression.
///
/// Every statement must be `local <one name> = <one value>` and every name must
/// be read exactly once, in the operand or in a later prelude value. Returns
/// `None` (the caller's old behaviour: reject the chain) on any failure,
/// including any inlining that would reorder calls or reads.
pub(super) fn fold_prelude_temps(prelude: &[Stat], operand: Expr) -> Option<Expr> {
    let mut decls: Vec<(String, Expr)> = Vec::with_capacity(prelude.len());
    for st in prelude {
        match st {
            Stat::Local { names, values } if names.len() == 1 && values.len() == 1 => {
                decls.push((names[0].clone(), values[0].clone()));
            }
            _ => return None,
        }
    }
    let mut operand = operand;
    for (name, value) in decls.iter().rev() {
        // Walking backwards, every later declaration has already been merged
        // into `operand`, so all remaining reads of `name` are in `operand`.
        if count_name_reads_in_expr(&operand, name) != 1 {
            return None;
        }
        let site = locate(&operand, name, &mut Effects::default(), false)?;
        if !move_is_safe(effects_of(value), &site) {
            return None;
        }
        replace_name_in_expr(&mut operand, name, value);
    }
    Some(operand)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(s: &str) -> Expr {
        Expr::Name(s.to_string())
    }
    fn field(obj: Expr, f: &str) -> Expr {
        Expr::Field { object: Box::new(obj), field: f.to_string() }
    }
    fn call(f: &str) -> Expr {
        Expr::Call { func: Box::new(name(f)), args: vec![] }
    }
    fn method(obj: Expr, m: &str) -> Expr {
        Expr::MethodCall { object: Box::new(obj), method: m.to_string(), args: vec![] }
    }
    fn add(l: Expr, r: Expr) -> Expr {
        Expr::BinOp { left: Box::new(l), op: BinOp::Add, right: Box::new(r) }
    }
    fn and(l: Expr, r: Expr) -> Expr {
        Expr::BinOp { left: Box::new(l), op: BinOp::And, right: Box::new(r) }
    }
    fn show(e: Option<Expr>) -> Option<String> {
        e.map(|e| format!("{e:?}"))
    }
    fn local(n: &str, v: Expr) -> Stat {
        Stat::Local { names: vec![n.to_string()], values: vec![v] }
    }

    /// The RaceRallySchedule shape: `local T = F.x; local R = T:Get()`.
    #[test]
    fn folds_a_field_then_method_call_prelude() {
        let prelude = vec![local("T", field(name("F"), "x"))];
        let got = fold_prelude_temps(&prelude, method(name("T"), "Get"));
        assert_eq!(show(got), show(Some(method(field(name("F"), "x"), "Get"))));
    }

    /// `local a = f(); local b = g(); return b + a` evaluates f then g. Inlining
    /// both gives `g() + f()`, which calls them in the opposite order.
    #[test]
    fn refuses_to_swap_two_calls() {
        let prelude = vec![local("a", call("f")), local("b", call("g"))];
        let operand = add(name("b"), name("a"));
        assert!(fold_prelude_temps(&prelude, operand).is_none());
    }

    /// Same two calls read in their original order are fine: `f() + g()`.
    #[test]
    fn keeps_two_calls_in_source_order() {
        let prelude = vec![local("a", call("f")), local("b", call("g"))];
        let operand = add(name("a"), name("b"));
        assert_eq!(show(fold_prelude_temps(&prelude, operand)), show(Some(add(call("f"), call("g")))));
    }

    /// `local a = f(); return h() + a`: `a` would move after `h()`.
    #[test]
    fn refuses_to_move_a_call_past_another_call() {
        let prelude = vec![local("a", call("f"))];
        let operand = add(call("h"), name("a"));
        assert!(fold_prelude_temps(&prelude, operand).is_none());
    }

    /// `local a = f(); return F.x + a`: the read of `F.x` would run before `f()`.
    #[test]
    fn refuses_to_move_a_call_past_a_read() {
        let prelude = vec![local("a", call("f"))];
        let operand = add(field(name("F"), "x"), name("a"));
        assert!(fold_prelude_temps(&prelude, operand).is_none());
    }

    /// `local a = F.x; return h() + a`: the read would observe `h()`'s effects.
    #[test]
    fn refuses_to_move_a_read_past_a_call() {
        let prelude = vec![local("a", field(name("F"), "x"))];
        let operand = add(call("h"), name("a"));
        assert!(fold_prelude_temps(&prelude, operand).is_none());
    }

    /// `local a = f(); return p and a` would stop calling `f` when `p` is falsy.
    #[test]
    fn refuses_to_make_a_call_conditional() {
        let prelude = vec![local("a", call("f"))];
        let operand = and(name("p"), name("a"));
        assert!(fold_prelude_temps(&prelude, operand).is_none());
    }

    /// A temp read twice cannot be inlined without duplicating its value.
    #[test]
    fn refuses_a_temp_read_twice() {
        let prelude = vec![local("a", call("f"))];
        let operand = add(name("a"), name("a"));
        assert!(fold_prelude_temps(&prelude, operand).is_none());
    }

    /// Anything other than a single-name `local` is not a foldable prelude.
    #[test]
    fn refuses_a_non_local_statement() {
        let prelude = vec![Stat::ExprStat(call("f"))];
        assert!(fold_prelude_temps(&prelude, name("x")).is_none());
    }

    /// Pure values (names, constants) move freely, even past calls.
    #[test]
    fn moves_a_pure_value_past_a_call() {
        let prelude = vec![local("a", name("p"))];
        let operand = add(call("h"), name("a"));
        assert_eq!(show(fold_prelude_temps(&prelude, operand)), show(Some(add(call("h"), name("p")))));
    }
}
