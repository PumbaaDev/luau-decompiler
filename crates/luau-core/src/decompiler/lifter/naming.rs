//! Register-to-local-name tracking and classification.
//!
//! Extracted from `lifter.rs` as part of Phase B0.52P6 (part 3/3 of the split).
//! Contains the primitive types (`RegVal`, `WriteKind`, `LocalTracker`) and
//! the `is_semantic_local_name` helper used throughout the lifter to decide
//! how to emit register writes.
//!
//! All items are `pub(super)` so the rest of the `lifter` module can use them,
//! but nothing is exposed outside the module tree.

use std::collections::{HashMap, HashSet};

use crate::ast::Expr;

/// Register tracking during decompilation
#[derive(Debug, Clone)]
pub(crate) enum RegVal {
    Unknown,
    Expr(Expr),
    /// A GenericFor loop variable seeded by Phase B0.10.
    ///
    /// Behaves like `Expr(Expr::Name(s))` for read operations inside the loop
    /// body — `reg_expr` returns `Expr::Name(s)`, and `store_complex` detects
    /// self-mutation against the name.
    ///
    /// Critically, the CALL vararg boundary scanner (B=0 path, lines ~2083–2090)
    /// uses `_ => break` for any non-`Expr` variant, so `LoopVar` registers are
    /// NOT picked up as trailing call arguments.  This fixes the regression
    /// introduced by Phase B0.10 where seeding loop variable registers with
    /// `Expr(Name)` caused adjacent vararg CALLs to absorb them as extra args.
    LoopVar(String),
}

/// Phase B0.49 — classifies a register-write emission into one of three kinds.
///
/// * `FirstDecl` — this register has never been written before in this proto;
///   emit `local <name> = <value>`.
/// * `Shadow`    — this register was previously declared with a DIFFERENT
///   semantic name; emit `local <name> = <value>` to shadow the old local.
///   Luau allows shadowing (the new binding masks the old within its scope),
///   so this is semantically correct.  Reads of the register after the shadow
///   resolve to the NEW name because subsequent register-commit sites overwrite
///   `regs[reg]` with `Expr::Name(new_name)`.
/// * `Reassign` — this register was previously declared with the SAME name
///   (or the new name looks generic such as `v\d+`, in which case we prefer
///   to keep using the existing name rather than churn); emit
///   `<existing_name> = <value>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WriteKind {
    FirstDecl,
    Shadow,
    Reassign,
}

/// Tracks which registers have been declared as locals.
///
/// Scoping invariant: Luau `local` is block-scoped but bytecode registers are
/// function-scoped.  When a register is first written inside a loop body we
/// must hoist the declaration before the loop.  Use `snapshot()`/`new_since()`
/// to detect registers first declared in a nested scope, then hoist them.
///
/// Phase B0.49: additionally tracks the CURRENT local name per register so we
/// can emit a shadow-local declaration when a subsequent write gives the
/// register a new semantic name (e.g., two consecutive `NEWCLOSURE` writes to
/// R1 with distinct debug_names — the second used to be emitted as a global
/// assignment because R1 was "already declared").  See the test module
/// `phase_b049_shadow_local_tests` for exhaustive coverage.
// Visible up to `crate::decompiler` because several `pub(super)` lifter helpers
// (ensure_table_reg_declared, store_complex, …) take it in their signatures.
#[derive(Clone)]
pub(in crate::decompiler) struct LocalTracker {
    pub(super) declared: HashSet<usize>,
    param_count: usize,
    /// Phase B0.49 — the name currently bound to each register as a local.
    /// Populated/updated by `classify_write`.  Reads of the register during
    /// lifting resolve via `regs[]` (which mirrors this name), so this map
    /// is only used to decide whether a subsequent write should Shadow or
    /// Reassign.
    current_names: HashMap<usize, String>,
}

impl LocalTracker {
    pub(super) fn new(param_count: usize) -> Self {
        let mut declared = HashSet::new();
        for i in 0..param_count {
            declared.insert(i);
        }
        Self { declared, param_count, current_names: HashMap::new() }
    }

    /// Returns true if this register needs a `local` declaration (first write).
    pub(super) fn needs_local(&mut self, reg: usize) -> bool {
        if reg < self.param_count {
            return false;
        }
        self.declared.insert(reg)
    }

    /// Phase B0.51 — returns true iff `reg` has never been declared as a local
    /// in this proto AND is not a parameter slot.  Used by
    /// `ensure_table_reg_declared` to decide whether an Unknown table register
    /// needs to be materialized as `local vN = {}`.
    pub(super) fn is_undeclared_non_param(&self, reg: usize) -> bool {
        reg >= self.param_count && !self.declared.contains(&reg)
    }

    /// Snapshot the current declared set before entering a nested scope.
    pub(super) fn snapshot(&self) -> HashSet<usize> {
        self.declared.clone()
    }

    /// Return registers newly declared since the snapshot.
    pub(super) fn new_since(&self, snap: &HashSet<usize>) -> Vec<usize> {
        let mut v: Vec<usize> = self.declared.difference(snap).copied().collect();
        v.sort_unstable();
        v
    }

    /// Pre-declare a register without emitting a local statement.
    pub(super) fn pre_declare(&mut self, reg: usize) {
        self.declared.insert(reg);
    }

    /// The local name currently bound to `reg`, if it holds a live binding.
    pub(super) fn current_name(&self, reg: usize) -> Option<&str> {
        self.current_names.get(&reg).map(|s| s.as_str())
    }

    /// Is `name` the local currently bound to some register in this proto?
    ///
    /// Copy-propagated registers (a plain `MOVE` emits no statement) are not in
    /// `declared`, so a register-keyed check cannot tell that a call argument is
    /// an alias of a real binding. Checking the NAME does.
    pub(super) fn is_bound_name(&self, name: &str) -> bool {
        self.current_names.values().any(|n| n == name)
    }

    /// Record the current local name for a register.  Callers that emit a
    /// `Stat::Local` via the existing `needs_local`/push flow (e.g., multi-
    /// return CALL, GETVARARGS) can call this to keep `current_names` in
    /// sync with the emitted bindings.
    pub(super) fn record_name(&mut self, reg: usize, name: &str) {
        self.current_names.insert(reg, name.to_string());
    }

    /// Phase B0.49 — decide how to emit a single-register write.
    ///
    /// Returns a `(WriteKind, name_to_use)` pair.  For `FirstDecl` and
    /// `Shadow`, `name_to_use` is the caller's `new_name` (and the tracker
    /// records it as the current binding).  For `Reassign`, `name_to_use`
    /// is the EXISTING declared name — callers MUST use this, not the
    /// freshly-computed `new_name`, to avoid emitting writes to an
    /// undeclared identifier (which Luau parses as a global assignment).
    ///
    /// Narrowness gate: we only shadow when BOTH current and new names are
    /// "semantic" (not `v\d+` fallbacks).  This prevents per-write churn
    /// caused by `ctx.reg_name`'s unique-suffix counter and avoids the
    /// Phase B0.44A regression (which broadly rewrote writes via a
    /// write-tracking map and inflated `v\d+` by +51%).
    pub(super) fn classify_write(&mut self, reg: usize, new_name: &str) -> (WriteKind, String) {
        // Params always reassign — the register IS the parameter, never
        // re-declared.  Mirrors `needs_local`'s behavior.
        if reg < self.param_count {
            return (WriteKind::Reassign, new_name.to_string());
        }
        if !self.declared.contains(&reg) {
            self.declared.insert(reg);
            self.current_names.insert(reg, new_name.to_string());
            return (WriteKind::FirstDecl, new_name.to_string());
        }
        // Already declared.  Decide Shadow vs Reassign.
        let old_name = self.current_names.get(&reg).cloned();
        match old_name {
            Some(old) if old == new_name => {
                // Same name → simple reassignment.
                (WriteKind::Reassign, old)
            }
            Some(old) => {
                // B0.130: "self" is a NAMECALL artifact — never preserve
                // it as a reassignment target for non-self values.  Without
                // this, NEWCLOSURE on a register previously holding "self"
                // emits `self = function()...end` (60 remaining instances).
                if old == "self" && new_name != "self" {
                    self.current_names.insert(reg, new_name.to_string());
                    return (WriteKind::Shadow, new_name.to_string());
                }
                // Names differ.  Only shadow when BOTH are semantic
                // (non-`v\d+`).  Otherwise reuse the old name to avoid
                // counter-bump churn.
                if is_semantic_local_name(&old) && is_semantic_local_name(new_name) {
                    self.current_names.insert(reg, new_name.to_string());
                    (WriteKind::Shadow, new_name.to_string())
                } else {
                    // One side is a generic fallback → keep existing name
                    // so emitted code stays `<existing> = value` (valid
                    // reassignment to the already-declared local) rather
                    // than creating a global write to a churned `v{N}`.
                    (WriteKind::Reassign, old)
                }
            }
            None => {
                // Declared without a recorded name (e.g., via pre_declare
                // for CALL multi-return, GETVARARGS, or loop-var seeding).
                // Record the new name and treat as plain reassignment —
                // we can't know whether the pre-declared name matched.
                self.current_names.insert(reg, new_name.to_string());
                (WriteKind::Reassign, new_name.to_string())
            }
        }
    }
}

/// Phase B0.49 helper — is `name` a non-generic, semantic identifier that
/// looks like a hint-derived name (import / global / debug_name / field /
/// call result), as opposed to the `v\d+` fallback that `synthesize_name`
/// produces when no hint applies?
///
/// Rules (conservative — only treats pure `v\d+` as generic):
///   * `name` starts with `v` followed by one or more ASCII digits and nothing
///     else  →  generic  →  returns `false`.
///   * Anything else (including `v1x`, `value`, `verb2`, `arg1`, `self`,
///     `tbl`, `arithmetic`, …)  →  semantic  →  returns `true`.
///
/// We intentionally do NOT blacklist other common fallbacks (e.g., `arg1`,
/// `tbl`, `fn`, `i`, `k`, `v`) because those are still hint-derived and
/// plausibly intentional; shadowing between them is safe and preserves the
/// narrative of the bytecode (e.g., a `fn` closure register getting
/// re-declared as a `tbl` is semantically meaningful).
pub(super) fn is_semantic_local_name(name: &str) -> bool {
    if name.is_empty() { return false; }
    let mut chars = name.chars();
    if chars.next() != Some('v') { return true; }
    // Must be at least one digit after the leading 'v' to qualify as generic
    let mut saw_digit = false;
    for c in chars {
        if c.is_ascii_digit() {
            saw_digit = true;
        } else {
            return true; // Non-digit after 'v' → semantic (e.g., "value")
        }
    }
    // "v" alone with no digits is semantic (loop var fallback); "v12" is generic
    !saw_digit
}
