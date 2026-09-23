//! Pins the `inline_single_use_temps` adjacency exception added alongside
//! the `upvalue_capture` family fix (campaign3/upvalue_capture_b).
//!
//! Background: `is_side_effect_call` blocks `inline_single_use_temps` from
//! nesting a recognized side-effecting call (`pcall`, `require`, `assert`,
//! `:Connect`, …) into its single use site, to avoid reordering it relative
//! to other code. But when that call's original bytecode CALL was compiled
//! with `results=multret` feeding directly into the NEXT statement's sole
//! argument list — the exact `pack(pcall(f, ...))` shape — leaving it split
//! is not merely less readable, it is WRONG: Lua's `local x = pcall(...)`
//! truncates the multret result down to ONE value, so `pack(x)` silently
//! drops every result after the first (pcall's error message on failure,
//! or all of luau_execute's return values on success).
//!
//! Reproduced on real corpus files fixed by this change: PetSimulator99
//! `ReplicatedStorage_Library_Modules_Fiu.lua` (`pack(pcall(luau_execute,
//! ...))`), `ReplicatedStorage_Library_Functions_QueuedLock.lua`
//! (`table.pack(pcall(arg1, ...))`), and dozens of `janitor:add(x:Connect(f))`
//! / `require(script:WaitForChild(...))` sites across every game in the
//! 11-game corpus. Confirmed against bytecode ground truth for the
//! `AdoptMe/ReplicatedStorage_new_modules_PetEntryHelper.lua`
//! `get_attachments_ancestor` case: disassembly shows
//! `CALL R5 args=3 results=multret` (FindFirstChild) immediately followed by
//! `RETURN R5 multret` — the un-fixed baseline returned a re-evaluated field
//! path instead of the call's result, a wrong-value bug the semantic checker
//! never flagged.
//!
//! The fix (post_passes.rs, `stmt_uses_name_as_sole_call_arg`) is
//! deliberately narrow: it only waives `is_side_effect_call` when the temp's
//! single read is in the VERY NEXT statement (adjacency — nothing else runs
//! between the two forms) AND the temp is the SOLE argument of that
//! statement's call/return (no sibling expression to reorder against). Every
//! other existing safety check in `inline_single_use_temps_pinned` (read
//! count, intervening writes, loop re-evaluation, arity-pinned tail
//! rejection, …) still applies unmodified afterward.

use super::super::inline_single_use_temps;
use crate::ast::{Expr, Stat};

fn name(s: &str) -> Expr { Expr::Name(s.to_string()) }

fn local(n: &str, e: Expr) -> Stat {
    Stat::Local { names: vec![n.to_string()], values: vec![e] }
}

fn call(func: Expr, args: Vec<Expr>) -> Expr {
    Expr::Call { func: Box::new(func), args }
}

fn ret(values: Vec<Expr>) -> Stat {
    Stat::Return { values }
}

/// `local x = pcall(f); local y = pack(x)` — sole use, adjacent, single
/// argument. Must collapse to `local y = pack(pcall(f))`, restoring pcall's
/// multret forwarding instead of truncating it to one value.
#[test]
fn multret_forward_pcall_sole_arg_adjacent_inlines() {
    let mut stmts = vec![
        local("x", call(name("pcall"), vec![name("f")])),
        local("y", call(name("pack"), vec![name("x")])),
    ];
    inline_single_use_temps(&mut stmts);
    assert_eq!(stmts.len(), 1, "expected the two statements to collapse into one: {:?}", stmts);
    match &stmts[0] {
        Stat::Local { names, values } => {
            assert_eq!(names, &["y".to_string()]);
            match &values[0] {
                Expr::Call { func, args } => {
                    assert!(matches!(func.as_ref(), Expr::Name(n) if n == "pack"));
                    assert_eq!(args.len(), 1);
                    match &args[0] {
                        Expr::Call { func: inner_func, args: inner_args } => {
                            assert!(matches!(inner_func.as_ref(), Expr::Name(n) if n == "pcall"));
                            assert_eq!(inner_args.len(), 1);
                            assert!(matches!(&inner_args[0], Expr::Name(n) if n == "f"));
                        }
                        other => panic!("expected pack's sole arg to be the nested pcall(f) call, got {:?}", other),
                    }
                }
                other => panic!("expected pack(...) call, got {:?}", other),
            }
        }
        other => panic!("expected a Local statement, got {:?}", other),
    }
}

/// `return pcall(f)`'s temp used as the sole return value, adjacent — the
/// `Stat::Return` sole-value arm of `stmt_uses_name_as_sole_call_arg`.
#[test]
fn multret_forward_pcall_sole_return_value_inlines() {
    let mut stmts = vec![
        local("x", call(name("pcall"), vec![name("f")])),
        ret(vec![name("x")]),
    ];
    inline_single_use_temps(&mut stmts);
    assert_eq!(stmts.len(), 1, "expected the two statements to collapse into one: {:?}", stmts);
    match &stmts[0] {
        Stat::Return { values } => {
            assert_eq!(values.len(), 1);
            match &values[0] {
                Expr::Call { func, args } => {
                    assert!(matches!(func.as_ref(), Expr::Name(n) if n == "pcall"));
                    assert_eq!(args.len(), 1);
                    assert!(matches!(&args[0], Expr::Name(n) if n == "f"));
                }
                other => panic!("expected pcall(f) inlined into the return, got {:?}", other),
            }
        }
        other => panic!("expected a Return statement, got {:?}", other),
    }
}

/// `local x = pcall(f); local y = pack(x, other())` — `x` is NOT the sole
/// argument (there is a sibling `other()`), so inlining would reorder
/// `pcall(f)` to run AFTER `other()` instead of before it. Must stay split.
#[test]
fn multret_forward_pcall_not_sole_arg_stays_split() {
    let mut stmts = vec![
        local("x", call(name("pcall"), vec![name("f")])),
        local("y", call(name("pack"), vec![name("x"), call(name("other"), vec![])])),
    ];
    inline_single_use_temps(&mut stmts);
    assert_eq!(stmts.len(), 2,
        "pcall must stay a separate statement when it is not the sole argument \
         (inlining would reorder it after the sibling `other()` call): {:?}", stmts);
}

/// `local x = pcall(f); local z = 1; local y = pack(x)` — the read is not in
/// the VERY NEXT statement. Must stay split (the general adjacency
/// requirement, independent of the sole-arg shape).
#[test]
fn multret_forward_pcall_non_adjacent_stays_split() {
    let mut stmts = vec![
        local("x", call(name("pcall"), vec![name("f")])),
        local("z", Expr::Number(1.0)),
        local("y", call(name("pack"), vec![name("x")])),
    ];
    inline_single_use_temps(&mut stmts);
    assert_eq!(stmts.len(), 3,
        "pcall must stay split when its use is not the immediately following statement: {:?}", stmts);
}

/// A non-side-effecting call (not in `is_side_effect_call`'s list) is
/// unaffected by this exception — it already inlines via the pre-existing
/// general path regardless of sole-arg/adjacency, confirming the new check
/// only ever WIDENS the side-effect-call gate, never narrows anything else.
#[test]
fn multret_forward_non_side_effect_call_still_inlines_as_before() {
    let mut stmts = vec![
        local("x", call(name("compute"), vec![name("f")])),
        local("y", call(name("wrap"), vec![name("x"), call(name("other"), vec![])])),
    ];
    inline_single_use_temps(&mut stmts);
    assert_eq!(stmts.len(), 1,
        "a non-side-effecting single-use temp should inline as before: {:?}", stmts);
}
