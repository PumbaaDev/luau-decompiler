# `merge_regs` may be trading a visible defect for an invisible one

**Status: unresolved. Applies to `c77dcd1`, which is currently on the branch.**
Raised by review, reasoned from code, **not** confirmed by measurement.

## The claim

`c77dcd1` changed `merge_regs` so that when both predecessors of a join hold a
simple `Expr::Name`, the slot keeps its value instead of resetting to `Unknown`.
When the two names *differ*, `continue` keeps whichever the caller's `self`
holds — the else-path state (`lifter/mod.rs:1765`).

The problem is scope. The very case the commit was written for — a `Shadow`
write via `classify_write` — emits `Stat::Local { name }` **inside the branch
block** (`opcode_handlers.rs:1485-1490`, `2118-2123`). So the surviving name's
declaration is branch-local, and a read after the merge emits it outside its
lexical scope.

`free_var_decls` cannot catch this: it scans with flat, scope-blind sets
(`free_var_decls.rs:106-121`), so a name declared anywhere counts as bound. It
is therefore not hoisted and not reported by `declared_never_assigned`.

At runtime that read resolves to a global (`nil`) or a stale outer binding. It
**compiles clean**, so the compile gate cannot see it either.

## Why this matters more than the numbers say

This is structurally the reverted `free_var_decls` masking bug: a name that IS
declared, so the check passes, while every use evaluates to nil. That one was
worse than the defect it replaced precisely because it was silent.

The commit measured −41 defects. If the review is right, that is the same class
of defect becoming **uncountable** rather than fixed. Both gates are blind to
it: the semantic checks by scope-blindness, the compile gate because the output
is valid Luau. A green board is not evidence here.

Note also that the two changes were measured **together**, not independently:
the GETUPVAL widening (`9995ed2`) acts on registers that this change stops
setting to `Unknown`, so their attributed gains are not separable as recorded.

## Suggested fix direction (not applied)

`regs_before` is already in scope at the call site (`lifter/mod.rs:1743`). When
both sides are `Name` and they differ, keep the name only if it matches the
**pre-branch** name — that is the binding still lexically live after the merge —
and reset to `Unknown` otherwise.

## How to settle it

Do not settle it on the gates alone; by the argument above they cannot see it.
Construct the case directly: a chunk where a register is `Shadow`-written to
different names in the two arms of an if/else and read after the merge. Check
whether the emitted read references a name whose `local` sits inside a branch
block. That is a structural property of the AST and needs no corpus.

If confirmed, either apply the pre-branch-match fix or revert `c77dcd1`. A
revert is cheap and returns to a state whose failure mode is visible `vN`
names — which this project has repeatedly found preferable to plausible-looking
wrong ones.

## Also raised, lower severity

* **`9995ed2` GETUPVAL widening.** `RegVal::Unknown` does not only mean "holds
  nothing" — `merge_regs` sets it on compound divergence and failed GETGLOBAL
  resolution. For an opcode *misidentified* as GETUPVAL (the exact case the
  B0.52 guard exists for), a declared local can be live-but-`Unknown`, and the
  widened guard now overwrites it with an upval alias. Later reads then emit a
  plausible wrong name instead of a flaggable `vN`. The commit's defence
  measured only the flagged class, which by construction cannot see a wrong-name
  substitution.
* **`7d69bbb` NAMECALL veto is all-or-nothing.** The scan walks every code word
  including AUX data of two-word instructions. A single false occurrence — an
  AUX word whose low byte matches, passing the register/string filters but with
  no matching CALL at `pc+2` — vetoes the true byte chunk-wide. It fails in the
  *visible* direction and the corpus showed zero exceptions, but it is
  data-dependent where the old scorer degraded gracefully.
* **`mint_trace` confirmed behaviour-neutral when unset** — every write and
  `eprintln!` is behind the cached `enabled()` check and the `format!`
  allocations predate the commit. Two caveats in *enabled* mode only:
  `env::var(...).is_ok()` means `LUAU_MINT_TRACE=0` still enables it, and the
  thread-local map plus stderr `FILE` markers break per-file attribution under
  any multi-threaded decompile. Measurement integrity, not correctness.
* The `None` arm of the GETUPVAL guard is **dead code**, not a mask: `insn_a`
  returns `u8` and the register file is at least 256 entries, so `regs.get(a)`
  is never `None` there.

No memory-safety, panic, unwrap, or loop-termination issues were found in any of
the four commits. 894/894 lib tests pass on `9fcf0b1`.
