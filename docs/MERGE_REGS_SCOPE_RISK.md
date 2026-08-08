# `merge_regs` was trading a visible defect for an invisible one — CONFIRMED, fixed

**Status: SETTLED (confirmed structurally, fix applied on top of `c77dcd1`).**
Raised by review, reasoned from code, then **confirmed by direct construction**
in `crates/luau-core/src/decompiler/lifter/tests/merge_regs_scope_probe_tests.rs`.

## The claim (now confirmed)

`c77dcd1` changed `merge_regs` so that when both predecessors of a join hold a
simple `Expr::Name` and the names *differ*, the slot kept the fall-through
(else-path) name instead of resetting to `Unknown`.

The problem is scope. The case the commit was written for — a `Shadow` write
via `classify_write` — emits `Stat::Local { name }` **inside the branch
block** (NEWTABLE handler and NEWCLOSURE handler in `opcode_handlers.rs`, plus
`emit_local_or_assign`/`store_complex`). The B0.57 pair-fix
(`hoist_escaping_locals`) hoists such declarations out of the branch, **but
only when `is_safe_hoist_init` accepts the init** — literals, pre-branch
names, and tables of safe values. A closure (`Expr::Function`) or call-result
init is NOT safe to hoist, so the declaration stays branch-local while the
merged register still names it.

## The construction that settled it

`probe_shadow_diff_names_read_after_merge_structure` builds a six-instruction
diamond: `NEWCLOSURE R1` ("alpha") pre-branch, `JUMPIFNOT`, `NEWCLOSURE R1`
("beta") in the then-arm, `NEWCLOSURE R1` ("gamma") in the else-arm, `RETURN
R1` after the join. Debug locals supply the semantic names. Under `c77dcd1`
alone the emitted source was:

```lua
local function alpha() return ... end
if cond then
    local function beta() return ... end
else
    local function gamma() return ... end
end
return gamma        -- gamma's `local` is INSIDE the else block
```

Exactly as claimed: the post-merge read references a name whose declaration is
branch-local. It **compiles clean** (bare `gamma` at top level is a legal
global read), `free_var_decls` counts the name as bound because its sets are
scope-blind, and at runtime the read resolves to a global — nil. Both gates
blind, silently wrong. A companion probe shows the same shape through a CALL
consumer (`gamma()`).

This is structurally the reverted `free_var_decls` masking bug: a name that IS
declared, so the check passes, while the use evaluates to nil. The commit's
measured −41 defects were partly this class becoming **uncountable** rather
than fixed.

## The fix applied

`merge_regs` now takes `regs_before` (the caller already had it in scope).
When both sides are simple `Name`s and they differ, the surviving name must
match the **pre-branch** binding of that register — the one whose `local` is
emitted before the `if` and is therefore still lexically live after the merge:

* self (else-path) name == pre-branch name → keep self;
* then-path name == pre-branch name → adopt the then-path value;
* **neither matches** (both names minted inside the arms) → reset to
  `Unknown`, so the read falls back to a *visible* `v{idx}` instead of a
  plausible wrong name.

This retains the half of `c77dcd1` that was right: in the common single-arm
case the fall-through side IS `regs_before`, so the kept name is the
pre-branch binding and the read stays valid
(`probe_single_arm_shadow_keeps_prebranch_name` pins this).

The probes are polarity-safe as regression tests: they assert "any branch name
surviving to a post-merge read has a top-level declaration", so a future
change that keeps the name AND hoists its declaration would still pass.

## Residual, known and accepted

* **The asymmetric B0.56 cases carry the same hazard** (one side `Name`, other
  side Unknown/compound → the Name is kept without a pre-branch check). That
  behavior predates this session by ~80 phases, is load-bearing for
  BaseCamera-style modules (where the init is a hoist-safe `{}` and B0.57
  makes the result correct), and was NOT changed here. The unsound subset is
  the same as above — non-hoistable init — but narrowing it was out of scope
  for settling `c77dcd1` and needs its own measured pass.
* The underlying coupling remains: `merge_regs` cannot see whether B0.57 will
  be able to hoist a name's declaration (init-safety lives in the emitted
  statement, not the register lattice). The pre-branch-match rule sidesteps
  this by only keeping names whose declaration provably precedes the branch.
* Keeping the pre-branch name is *lexically* sound but can still be
  *value*-wrong on the path that rewrote the register (the register holds the
  arm's value, the name denotes the pre-branch one). That failure predates
  `c77dcd1`, cannot be reached from honest compiler output of valid source
  (a live-across-join variable is not register-reused inside the arms), and
  the alternative — Unknown — is the visible-vN trade this project makes
  deliberately. Recorded here so the next reader knows it was seen.

## Also raised in the same review, unchanged by this fix

* **`9995ed2` GETUPVAL widening** — `Unknown` does not only mean "holds
  nothing"; for an opcode misidentified as GETUPVAL the widened guard can
  overwrite a live-but-Unknown local with an upval alias. Lower severity,
  unmeasured, still open.
* **`7d69bbb` NAMECALL veto is all-or-nothing** — a single false byte match in
  AUX data vetoes the true byte chunk-wide. Fails in the visible direction;
  corpus showed zero exceptions.
* `mint_trace` is behaviour-neutral when unset; two measurement-integrity
  caveats in enabled mode (`LUAU_MINT_TRACE=0` still enables; thread-local map
  breaks per-file attribution under multi-threaded decompile).
* The `None` arm of the GETUPVAL guard is dead code, not a mask.
