//! Register-to-local-name tracking and classification.
//!
//! Extracted from `lifter.rs` as part of Phase B0.52P6 (part 3/3 of the split).
//! Contains the primitive types (`RegVal`, `WriteKind`, `LocalTracker`) and
//! the `is_semantic_local_name` helper used throughout the lifter to decide
//! how to emit register writes.
//!
//! All items are `pub(super)` so the rest of the `lifter` module can use them,
//! but nothing is exposed outside the module tree.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

thread_local! {
    /// Fresh names minted when an upvalue alias was retired. A local with one of
    /// these names that nothing reads is a discarded temporary, not a real value.
    /// Keyed by (proto identity, fresh name) and cleared at the start of every
    /// decompile, so no result depends on files lifted earlier on this thread.
    static RETIRED_FRESH: RefCell<HashMap<(usize, String), String>> = RefCell::new(HashMap::new());
}

/// Forget every retired name; called once per decompile.
pub(super) fn clear_retired_fresh() {
    RETIRED_FRESH.with(|map| map.borrow_mut().clear());
}

/// The upvalue alias name a retired temporary `name` was split off from.
/// Looked up for the proto currently being post-processed.
pub(super) fn retired_fresh_alias(name: &str) -> Option<String> {
    let proto = crate::decompiler::lifter::current_dce_proto_id();
    RETIRED_FRESH.with(|map| map.borrow().get(&(proto, name.to_string())).cloned())
}

/// Every retired fresh name of the proto being post-processed, with the
/// upvalue alias it was split off from.
pub(super) fn retired_fresh_names() -> HashMap<String, String> {
    let proto = crate::decompiler::lifter::current_dce_proto_id();
    RETIRED_FRESH.with(|map| {
        map.borrow()
            .iter()
            .filter(|((p, _), _)| *p == proto)
            .map(|((_, fresh), alias)| (fresh.clone(), alias.clone()))
            .collect()
    })
}

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
    /// Registers whose `current_names` entry is a read-only ALIAS of an outer
    /// binding (an upvalue read via GETUPVAL), NOT this register's own declared
    /// value. When such a register is later given its own value, `classify_write`
    /// must NOT preserve the alias name: reusing e.g. `v9` (a captured upvalue's
    /// own name) for an unrelated later CALL result collides with the real
    /// upvalue and makes dead-store elimination delete real statements (lv_0452).
    alias_regs: HashSet<usize>,
    /// For each alias register, the upvalue index it was loaded from and the
    /// pc of that GETUPVAL.
    alias_upval: HashMap<usize, (usize, usize)>,
    /// The destination register of the instruction being lifted, when it is an
    /// ALIAS register being overwritten with a value of its own and no SETUPVAL
    /// stores that value back: `(reg, alias_name, fresh_name)`. Set by the
    /// dispatch loop before the handler runs and cleared before the next one,
    /// so it can only ever affect the one write it was computed for.
    alias_retire: Option<(usize, String, String)>,
    /// Registers whose upvalue alias was retired by a write of their own, with
    /// the alias name they carried. A branch-merge seed that is just that
    /// alias name must not be assigned back (see `bind_arm_merge_seeds`).
    retired_alias: HashMap<usize, String>,
}

impl LocalTracker {
    pub(super) fn new(param_count: usize) -> Self {
        let mut declared = HashSet::new();
        for i in 0..param_count {
            declared.insert(i);
        }
        Self { declared, param_count, current_names: HashMap::new(), alias_regs: HashSet::new(), alias_upval: HashMap::new(), alias_retire: None, retired_alias: HashMap::new() }
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

    /// A register that is not one of this proto's parameters.
    pub(super) fn is_non_param(&self, reg: usize) -> bool {
        reg >= self.param_count
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

    /// Force a FRESH shadow identity onto a register, overwriting any stale
    /// declaration and name. Returns the name to use. This is for the branch
    /// seed of a register whose PRIOR lifetime is dead (its live value was
    /// clobbered back to `Unknown`) and which the Luau compiler has REUSED for
    /// a new two-arm-diamond value. `classify_write` would keep the stale
    /// generic name and route the arm writes through `Reassign`, which the
    /// seed gate then rejects, leaving the merge read unbound. Seeding a fresh
    /// name is bit-exact whenever both arms write the register and the merge
    /// reads it before redefining it — the pre-branch (dead) value is provably
    /// never observed — and it never reuses the old name, so the repurposed-slot
    /// hazard (476_BadgeMenu) cannot arise.
    pub(super) fn reseed_shadow(&mut self, reg: usize, name: &str) -> String {
        self.declared.insert(reg);
        self.current_names.insert(reg, name.to_string());
        self.alias_regs.remove(&reg);
        name.to_string()
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
        self.alias_regs.remove(&reg);
    }

    /// Record that `reg` now holds a read-only ALIAS of an outer binding (an
    /// upvalue read), under the outer binding's own `name`. Behaves like
    /// `record_name` for reads, but marks `reg` so a later write of the
    /// register's OWN value does not silently inherit the alias name and
    /// collide with the real binding (lv_0452). Cleared by the next real write.
    pub(super) fn record_alias_name(&mut self, reg: usize, name: &str) {
        self.current_names.insert(reg, name.to_string());
        self.alias_regs.insert(reg);
    }

    /// Remember which upvalue index the alias register `reg` was loaded from.
    pub(super) fn record_alias_upval(&mut self, reg: usize, upval: usize, pc: usize) {
        self.alias_upval.insert(reg, (upval, pc));
    }

    /// `(upvalue index, alias name, pc of the GETUPVAL)` if `reg` currently
    /// holds a read-only alias of an upvalue.
    pub(super) fn alias_upval_of(&self, reg: usize) -> Option<(usize, String, usize)> {
        if !self.alias_regs.contains(&reg) {
            return None;
        }
        let (idx, pc) = *self.alias_upval.get(&reg)?;
        let name = self.current_names.get(&reg)?.clone();
        Some((idx, name, pc))
    }

    /// Arm the retire for the write about to be lifted (see `alias_retire`).
    pub(super) fn begin_alias_retire(&mut self, proto_key: usize, reg: usize, alias: String, fresh: String) {
        RETIRED_FRESH.with(|map| map.borrow_mut().insert((proto_key, fresh.clone()), alias.clone()));
        self.alias_retire = Some((reg, alias, fresh));
    }

    /// The upvalue alias name `reg` carried before a write of its own retired it.
    pub(super) fn retired_alias_of(&self, reg: usize) -> Option<&str> {
        self.retired_alias.get(&reg).map(|s| s.as_str())
    }

    pub(super) fn clear_alias_retire(&mut self) {
        self.alias_retire = None;
    }

    /// Is the write of `reg` that is being lifted one that must NOT reuse the
    /// upvalue alias it currently carries?
    pub(super) fn alias_retire_pending(&self, reg: usize) -> bool {
        matches!(&self.alias_retire, Some((r, _, _)) if *r == reg)
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
        // A write of the register's OWN value into an upvalue alias that no
        // SETUPVAL stores back: the register is a fresh local, never a
        // reassignment of the upvalue. This must run BEFORE the same-name arm
        // below, because a derived result name (`value(...)` -> `value`) or a
        // generic one equals the alias name and would otherwise reassign it.
        if let Some((r, alias, fresh)) = self.alias_retire.clone() {
            if r == reg {
                self.alias_retire = None;
                self.alias_regs.remove(&reg);
                self.alias_upval.remove(&reg);
                self.retired_alias.insert(reg, alias.clone());
                let name = if new_name == alias { fresh } else { new_name.to_string() };
                self.declared.insert(reg);
                self.current_names.insert(reg, name.clone());
                return (WriteKind::Shadow, name);
            }
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
                // This write gives the register its OWN value, so it is no
                // longer an alias of the outer upvalue.
                let was_alias = self.alias_regs.remove(&reg);
                // Names differ.  Only shadow when BOTH are semantic
                // (non-`v\d+`).  Otherwise reuse the old name to avoid
                // counter-bump churn.
                if is_semantic_local_name(&old) && is_semantic_local_name(new_name) {
                    self.current_names.insert(reg, new_name.to_string());
                    (WriteKind::Shadow, new_name.to_string())
                } else if was_alias {
                    // `old` was a read-only alias of a captured upvalue (e.g.
                    // `v9`), not this register's own binding. Reusing it here
                    // would emit `v9 = <call result>` and collide with the real
                    // upvalue `v9`, which dead-store elimination then unpicks by
                    // deleting real writes (lv_0452). Mint the fresh name instead.
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
