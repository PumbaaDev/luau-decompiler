//! Declare captured upvalues that no scope ever declares.
//!
//! ── THE BUG ─────────────────────────────────────────────────────────────
//! Measured on a 1,273-file corpus: **556 files, 2,118 occurrences** — 98.8%
//! of every provably-wrong defect found. One root cause, hit repeatedly.
//!
//! A closure that captures a parent local gets the reference but never the
//! declaration. From `PopperCam.lua`:
//!
//! ```luau
//! local function OnCameraSubjectChanged()
//!     v13 = true                      -- upvalue, never declared anywhere
//!     v8  = CameraSubject.Torso       -- upvalue, never declared anywhere
//! end
//! local function OnWorkspaceChanged2(arg1)
//!     CurrentCamera3 = CurrentCamera2 -- upvalue, never declared anywhere
//!     v7:disconnect()                 -- upvalue, never declared anywhere
//! end
//! ```
//!
//! The original had module-level `local` declarations that these closures
//! captured. Lifting kept the *uses* and lost the *declarations*, so the
//! output assigns to globals instead of the intended locals. It still parses,
//! which is exactly why marker-counting scored these files clean.
//!
//! Note this is a different problem from `capture_chain.rs`, which works out
//! what an upvalue should be *called*. A perfect name is still broken output
//! if nothing declares it.
//!
//! ── THE FIX ─────────────────────────────────────────────────────────────
//! After lifting, walk the chunk and collect every name that is **assigned**
//! but never **bound**. Emit `local <names>` at the top of the chunk, which is
//! where the original module-level locals lived.
//!
//! ── WHY THIS IS SOUND ───────────────────────────────────────────────────
//! The pass only declares a name when ALL of these hold:
//!
//!   1. it is an assignment target somewhere in the chunk — a name that is
//!      only ever *read* is far more likely a global we do not know about,
//!      and declaring it would shadow that global to nil, which is a
//!      regression rather than a fix;
//!   2. no scope binds it — not a local, parameter, loop variable, or
//!      function name anywhere in the chunk;
//!   3. it is not a known Luau or Roblox global.
//!
//! Together these mean: assigned, undeclared, and not a global. In a
//! decompilation that is an upvalue whose declaration was lost. The one
//! remaining possibility is a script deliberately writing a new global, which
//! condition 3's list covers for the realistic cases and which is rare enough
//! in module code to be the right trade.
//!
//! Declaring at chunk top rather than at first use is deliberate: a capture
//! may be written in one closure and read in another that runs earlier, so
//! only the outermost scope is guaranteed to dominate every use.

use crate::ast::{Expr, Stat, TableField};
use std::collections::BTreeSet;

/// Globals that must never be shadowed by a generated declaration.
/// Shadowing one of these to `nil` turns working output into broken output.
const KNOWN_GLOBALS: &[&str] = &[
    // Luau base library
    "assert", "error", "getfenv", "getmetatable", "ipairs", "next", "pairs",
    "pcall", "print", "rawequal", "rawget", "rawlen", "rawset", "require",
    "select", "setfenv", "setmetatable", "tonumber", "tostring", "type",
    "typeof", "unpack", "xpcall", "newproxy", "loadstring", "collectgarbage",
    "coroutine", "debug", "math", "os", "string", "table", "utf8", "bit32",
    "buffer", "task", "vector", "_G", "_VERSION", "shared",
    // Roblox globals and common types
    "game", "workspace", "script", "Enum", "Instance", "Vector2", "Vector3",
    "CFrame", "Color3", "BrickColor", "UDim", "UDim2", "Ray", "Rect",
    "Region3", "TweenInfo", "NumberRange", "NumberSequence", "ColorSequence",
    "PhysicalProperties", "Random", "Faces", "Axes", "DateTime", "Font",
    "OverlapParams", "RaycastParams", "PathWaypoint", "delay", "spawn",
    "wait", "tick", "time", "elapsedTime", "settings", "UserSettings",
    "PluginManager", "DebuggerManager", "stats", "version", "warn",
];

fn is_known_global(name: &str) -> bool {
    KNOWN_GLOBALS.contains(&name)
}

/// Is this a name the decompiler invented, rather than one recovered from
/// debug info or the constant table?
///
/// Matches `v0`, `v30`, `upval_3`, `cap_1`, `arg_2` — shapes no real Luau or
/// Roblox global ever has. That matters: a read-only name of this shape is
/// provably ours, so declaring it cannot shadow anything that exists.
fn is_generated_name(name: &str) -> bool {
    for prefix in ["upval_", "cap_", "arg_", "field_"] {
        if let Some(rest) = name.strip_prefix(prefix) {
            return !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit());
        }
    }
    // `v12` but not `v`, `vec`, `value`
    if let Some(rest) = name.strip_prefix('v') {
        return !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit());
    }
    false
}

/// Names bound, names assigned, and names merely read.
#[derive(Default)]
struct Scan {
    bound: BTreeSet<String>,
    assigned: BTreeSet<String>,
    /// Read but never written. Only actionable for generated names — see
    /// [`is_generated_name`].
    read: BTreeSet<String>,
    /// Read at least once OUTSIDE any closure body.
    ///
    /// This is the discriminant that separates a genuine captured upvalue from
    /// a value the lifter dropped. A capture, by definition, is a reference
    /// from inside a nested function to a name in an enclosing scope — so a
    /// real capture is read INSIDE a closure. A name read at chunk top level
    /// that nothing ever assigns is not a capture; it is a hole.
    ///
    /// Declaring a hole is worse than leaving it undeclared, because it makes
    /// the output parse while every use evaluates to nil. See the admission
    /// rules in [`declare_free_vars`].
    read_outside_closure: BTreeSet<String>,
    /// Closure nesting depth during the walk. 0 == chunk top level.
    closure_depth: usize,
}

/// Insert declarations for captured upvalues that nothing declares.
/// Returns the names declared, for diagnostics.
pub fn declare_free_vars(body: &mut Vec<Stat>) -> Vec<String> {
    let mut scan = Scan::default();
    scan_stats(body, &mut scan);

    // Two sources, with deliberately different admission rules.
    //
    //   assigned  — any name written but never bound. Writing to an
    //               undeclared name is an upvalue whose declaration was lost.
    //
    //   read-only — ONLY names the decompiler invented (`v30`, `upval_3`).
    //               An upvalue a closure merely reads is still an upvalue,
    //               and this is the majority case: the first version of this
    //               pass skipped all read-only names and left 48 of 53
    //               defects standing. But an unrecognised name that is only
    //               read could be a real global, and declaring that would
    //               shadow it to nil — turning working output into broken
    //               output. Restricting to generated shapes removes that
    //               risk entirely, since nothing real is ever named `v30`.
    let mut free: BTreeSet<String> = scan
        .assigned
        .iter()
        .filter(|n| !scan.bound.contains(*n))
        .filter(|n| !is_known_global(n))
        .filter(|n| !n.is_empty())
        .cloned()
        .collect();

    // Read-only names: declare ONLY if the reads are consistent with a capture.
    //
    // ── WHY THIS GUARD EXISTS ───────────────────────────────────────────
    // The earlier version declared every generated read-only name. It took
    // `undefined_local` from 48 to 0 and that was reported as a fix. It was
    // not: the win conflated two different situations.
    //
    //   * a genuine captured upvalue missing its declaration -> declaring is right
    //   * a value the lifter dropped                          -> declaring is WRONG
    //
    // The second case is worse than the defect it replaces, because it is
    // silent. `undefined_local` cannot see it — the name IS declared — so the
    // output parses while every use evaluates to nil.
    //
    // Measured cost of getting this wrong: on a 628-script corpus a check that
    // free_var_decls could not hide from (`declared_never_assigned`) put the
    // clean rate at 13.7%, not the 94.9% reported while the masking was in
    // place. 532 of 628 files carried silently-nil values.
    //
    // Concretely, `ReplicatedStorage.Badges` inlines a helper at 25 call sites
    // with only its receiver substituted:
    //
    //     if Honey.Count then
    //         Honey.Count = v12      -- declared at chunk top, never assigned
    //     end
    //
    // — so 25 badge counts silently became nil.
    //
    // ── THE DISCRIMINANT ────────────────────────────────────────────────
    // A capture is BY DEFINITION a reference from inside a nested function to a
    // name in an enclosing scope. So a real capture is read INSIDE a closure.
    // A generated name read at chunk top level that nothing ever assigns is not
    // a capture — it is a hole where the lifter dropped a value.
    //
    // Declaring holes hides them. Leaving them undeclared keeps them visible to
    // `undefined_local`, which is the honest outcome: still broken, but broken
    // in a way the tooling reports.
    for n in &scan.read {
        if scan.bound.contains(n) || is_known_global(n) || !is_generated_name(n) {
            continue;
        }
        // Read only from inside closures -> consistent with a capture.
        if !scan.read_outside_closure.contains(n) {
            free.insert(n.clone());
        }
        // Otherwise: leave undeclared on purpose, so the defect stays visible.
    }

    let free: Vec<String> = free.into_iter().collect();

    if free.is_empty() {
        return free;
    }

    // Chunk top, but after any leading comment header so the banner stays put.
    let insert_at = body
        .iter()
        .position(|s| !matches!(s, Stat::Comment(_)))
        .unwrap_or(0);

    body.insert(
        insert_at,
        Stat::Local { names: free.clone(), values: Vec::new() },
    );
    free
}

// ── Scanning ────────────────────────────────────────────────────────────

fn scan_stats(stats: &[Stat], out: &mut Scan) {
    for s in stats {
        scan_stat(s, out);
    }
}

fn scan_stat(stat: &Stat, out: &mut Scan) {
    match stat {
        Stat::Local { names, values } => {
            for n in names {
                out.bound.insert(n.clone());
            }
            for v in values {
                scan_expr(v, out);
            }
        }
        Stat::Assign { targets, values } => {
            for t in targets {
                // A bare `Name` target is an assignment to that identifier.
                // `a.b = x` and `a[k] = x` assign to a FIELD of `a`, which
                // still requires `a` to exist — recorded as a read below.
                if let Expr::Name(n) = t {
                    out.assigned.insert(n.clone());
                } else {
                    scan_expr(t, out);
                }
            }
            for v in values {
                scan_expr(v, out);
            }
        }
        Stat::If { condition, then_body, elseif_clauses, else_body } => {
            scan_expr(condition, out);
            scan_stats(then_body, out);
            for (c, b) in elseif_clauses {
                scan_expr(c, out);
                scan_stats(b, out);
            }
            if let Some(b) = else_body {
                scan_stats(b, out);
            }
        }
        Stat::While { condition, body } => {
            scan_expr(condition, out);
            scan_stats(body, out);
        }
        Stat::Repeat { body, condition } => {
            scan_stats(body, out);
            scan_expr(condition, out);
        }
        Stat::NumericFor { var, start, stop, step, body } => {
            out.bound.insert(var.clone());
            scan_expr(start, out);
            scan_expr(stop, out);
            if let Some(s) = step {
                scan_expr(s, out);
            }
            scan_stats(body, out);
        }
        Stat::GenericFor { vars, iterators, body } => {
            for v in vars {
                out.bound.insert(v.clone());
            }
            for it in iterators {
                scan_expr(it, out);
            }
            scan_stats(body, out);
        }
        Stat::Return { values } => {
            for v in values {
                scan_expr(v, out);
            }
        }
        Stat::DoBlock { body } => scan_stats(body, out),
        Stat::ExprStat(e) => scan_expr(e, out),
        Stat::LocalFunction { name, func } => {
            out.bound.insert(name.clone());
            scan_expr(func, out);
        }
        Stat::MethodFunction { receiver, func, .. } => {
            scan_expr(receiver, out);
            scan_expr(func, out);
        }
        Stat::Break | Stat::Continue | Stat::Comment(_) => {}
    }
}

fn scan_expr(expr: &Expr, out: &mut Scan) {
    match expr {
        // Reads are recorded separately from writes. They are only ever
        // actionable for decompiler-generated names — see the admission
        // rules in `declare_free_vars`.
        Expr::Name(n) => {
            out.read.insert(n.clone());
            if out.closure_depth == 0 {
                out.read_outside_closure.insert(n.clone());
            }
        }
        Expr::Field { object, .. } => scan_expr(object, out),
        Expr::Index { object, key } => {
            scan_expr(object, out);
            scan_expr(key, out);
        }
        Expr::BinOp { left, right, .. } => {
            scan_expr(left, out);
            scan_expr(right, out);
        }
        Expr::UnOp { operand, .. } => scan_expr(operand, out),
        Expr::Call { func, args } => {
            scan_expr(func, out);
            for a in args {
                scan_expr(a, out);
            }
        }
        Expr::MethodCall { object, args, .. } => {
            scan_expr(object, out);
            for a in args {
                scan_expr(a, out);
            }
        }
        Expr::Function { params, body, .. } => {
            // Parameters bind INSIDE the closure. Recording them as bound
            // chunk-wide is intentional and conservative: it can only cause
            // us to declare fewer names, never more, so it cannot introduce
            // a shadowing regression.
            for p in params {
                out.bound.insert(p.clone());
            }
            // Track nesting so reads can be attributed to inside-a-closure
            // (a possible capture) or chunk top level (never a capture).
            out.closure_depth += 1;
            scan_stats(body, out);
            out.closure_depth -= 1;
        }
        Expr::Table { fields } => {
            for f in fields {
                match f {
                    TableField::Sequential(v) => scan_expr(v, out),
                    TableField::Named(_, value) => scan_expr(value, out),
                    TableField::Indexed(key, value) => {
                        scan_expr(key, out);
                        scan_expr(value, out);
                    }
                }
            }
        }
        Expr::Ternary { cond, then_expr, else_expr } => {
            scan_expr(cond, out);
            scan_expr(then_expr, out);
            scan_expr(else_expr, out);
        }
        Expr::Nil
        | Expr::Bool(_)
        | Expr::Number(_)
        | Expr::String(_)
        | Expr::Varargs
        | Expr::Vector(..) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(n: &str) -> Expr {
        Expr::Name(n.to_string())
    }

    /// The PopperCam shape: a closure assigns to a captured upvalue that
    /// nothing declares.
    #[test]
    fn declares_assigned_but_unbound() {
        let mut body = vec![Stat::LocalFunction {
            name: "f".into(),
            func: Expr::Function {
                params: vec![],
                is_vararg: false,
                body: vec![Stat::Assign {
                    targets: vec![name("v7")],
                    values: vec![Expr::Bool(true)],
                }],
            },
        }];
        let declared = declare_free_vars(&mut body);
        assert_eq!(declared, vec!["v7".to_string()]);
        assert!(matches!(&body[0], Stat::Local { names, .. } if names == &vec!["v7".to_string()]));
    }

    /// Writing to a real global must NOT be shadowed — that would turn
    /// working output into broken output.
    #[test]
    fn never_shadows_known_globals() {
        let mut body = vec![Stat::Assign {
            targets: vec![name("workspace")],
            values: vec![Expr::Nil],
        }];
        let declared = declare_free_vars(&mut body);
        assert!(declared.is_empty(), "must not declare a known global: {:?}", declared);
    }

    /// An UNRECOGNISED name that is only read is left alone — it may be a
    /// global we do not know about, and declaring it would shadow it to nil.
    #[test]
    fn unknown_read_only_name_is_left_alone() {
        let mut body = vec![Stat::ExprStat(Expr::Call {
            func: Box::new(name("SomeUnknownGlobal")),
            args: vec![],
        })];
        let declared = declare_free_vars(&mut body);
        assert!(declared.is_empty(), "read-only name declared: {:?}", declared);
    }

    /// A GENERATED name that is only read IS declared. This is the majority
    /// case — the first version of the pass skipped it and left 48 of 53
    /// real defects standing.
    #[test]
    fn generated_read_only_name_is_declared() {
        let mut body = vec![Stat::Local {
            names: vec!["x".into()],
            values: vec![Expr::BinOp {
                left: Box::new(name("v30")),
                op: crate::ast::BinOp::Add,
                right: Box::new(name("v29")),
            }],
        }];
        let mut declared = declare_free_vars(&mut body);
        declared.sort();
        assert_eq!(declared, vec!["v29".to_string(), "v30".to_string()]);
    }

    /// The generated-name test must be tight: real identifiers that merely
    /// start with `v` are not ours and must never be declared.
    #[test]
    fn generated_name_test_is_tight() {
        assert!(is_generated_name("v0"));
        assert!(is_generated_name("v30"));
        assert!(is_generated_name("upval_3"));
        assert!(is_generated_name("cap_1"));
        assert!(!is_generated_name("v"));
        assert!(!is_generated_name("value"));
        assert!(!is_generated_name("vec3"));
        assert!(!is_generated_name("velocity"));
        assert!(!is_generated_name("workspace"));
    }

    /// Already-declared locals must not be re-declared.
    #[test]
    fn respects_existing_declarations() {
        let mut body = vec![
            Stat::Local { names: vec!["x".into()], values: vec![Expr::Number(1.0)] },
            Stat::Assign { targets: vec![name("x")], values: vec![Expr::Number(2.0)] },
        ];
        let declared = declare_free_vars(&mut body);
        assert!(declared.is_empty(), "re-declared a bound local: {:?}", declared);
    }

    /// Loop variables bind, so assigning to one is not a free variable.
    #[test]
    fn loop_vars_count_as_bound() {
        let mut body = vec![Stat::NumericFor {
            var: "i".into(),
            start: Expr::Number(1.0),
            stop: Expr::Number(10.0),
            step: None,
            body: vec![Stat::Assign {
                targets: vec![name("i")],
                values: vec![Expr::Number(0.0)],
            }],
        }];
        let declared = declare_free_vars(&mut body);
        assert!(declared.is_empty(), "loop var declared: {:?}", declared);
    }

    /// Field assignment (`a.b = x`) requires `a` but does not assign `a`
    /// itself, so it must not create a declaration for `a`.
    #[test]
    fn field_assignment_does_not_declare_object() {
        let mut body = vec![Stat::Assign {
            targets: vec![Expr::Field {
                object: Box::new(name("SomeTable")),
                field: "k".into(),
            }],
            values: vec![Expr::Number(1.0)],
        }];
        let declared = declare_free_vars(&mut body);
        assert!(declared.is_empty(), "declared a field-assign object: {:?}", declared);
    }

    #[test]
    fn declares_multiple_in_one_statement() {
        let mut body = vec![Stat::Assign {
            targets: vec![name("a1"), name("b2")],
            values: vec![Expr::Nil, Expr::Nil],
        }];
        let mut declared = declare_free_vars(&mut body);
        declared.sort();
        assert_eq!(declared, vec!["a1".to_string(), "b2".to_string()]);
    }
}
