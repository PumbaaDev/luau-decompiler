# Fix plan for the 1143-script live corpus

**Written:** 2026-08-10 01:32 +01:00 (live clock, `Get-Date`).
**Baseline this plan is ordered against:** 1143 distinct scripts pulled from a live
Roblox client, 1142 decompile. Compile gate **1140/1142 (99.8%)**, semantic
**1082/1142 clean (94.7%)**, **125 defects**.
**Inputs:** six diagnoses, each with a patch spec and a stated breakage.

The standard is not the defect count. This project has already rejected a fix
that improved the count while corrupting output (`docs/MERGE_REGS_SCOPE_RISK.md`,
and commit `ae52a34` "merge_regs really was hiding what it fixed"). That
rejection was correct and it sets the bar used below: a patch earns its rank
from `(confidence x gain) / risk`, where confidence is the evidence tier under
the mechanism, not the size of the class it claims.

## Evidence tiers used throughout

| Tier | Meaning |
|---|---|
| 1 | Our own source, or a raw proto/constant/debug table read with no opcode decode involved |
| 2 | Bytecode instruction decode (requires a correct opcode map, see the trap below) |
| 3 | Independent oracle decompiler, corroboration only, never ground truth |
| 4 | Our semantic checker |
| 5 | Reasoning over our own output text |

---

## Tooling correction that re-scores four of the six diagnoses (Tier 1)

`docs/DEFECT_CENSUS_218.md` records, in its own method note, that
`disassemble --opmap` **consults only the pooled `--opmap-cache` store and never
the opmap database**. With no store present it silently falls back to solo
detection and prints an incoherent listing. Diagnosis 6 rediscovered the same
trap independently (45 of ~83 opcodes pinned by the DB entry alone, fixed by
supplying the corpus-pooled cache), which is a second, independent confirmation
of a documented in-repo failure mode.

Applying that to the six diagnoses:

| Diagnosis | Tier 2 claim rests on | Verdict |
|---|---|---|
| 1 (branch_pc) | instruction decode of 609_ParticleTools | **Provisional.** Re-derive with `--opmap-cache` before citing. The patch does not depend on it, see P2. |
| 2 (checker FPs) | attempted, desynced, self-declared VOID | **Reopenable, not dead.** The desync is the documented trap, not a property of the protos. See "Dead ends". |
| 3 (canonical AUX) | constant table dump only | **Survives.** Constant tables are not opcode-decoded. |
| 4 (name_body) | proto 21 debug_name + its constant pool | **Survives.** Neither is opcode-decoded. |
| 5 (is_plausible_field_name) | constant table dump only | **Survives.** |
| 6 (merge point) | instruction decode **with** the pooled cache | **Survives**, and is the reason the trap is documented here. |

Nothing below cites a bare `disassemble --opmap --opmap-db` instruction listing
as load-bearing evidence.

---

## The order

| Rank | Patch | Source | Counted defects fixed | Confidence in the count | Risk | Verdict |
|---|---|---|---:|---|---|---|
| **P1** | Canonical AUX resolution bypasses the control-byte filter | D3 | 7 (+1 semantic row, so 7 or 8) | High. Blast radius enumerated, not estimated | Very low | **APPLY** |
| **P2** | Store the CFG's correct branch PC, delete `find_branch_pc` | D1 | 9 | Medium-high on the number, high on the mechanism | Low to medium | **APPLY** |
| **P3** | Narrow `check_declared_but_never_assigned`: field-aware tokenizer + scoped read set | D2 parts 1, 2a | up to 60 findings, **0 real defects** | Medium | No output risk, recall risk | **APPLY, with a per-instance audit** |
| **P4** | Narrow `check_name_body_agreement` on ancestor containment | D4 | 1 finding, 0 real defects | High | Very low | **APPLY** |
| **P5** | Prefer the then-arm's own escape JUMP over the BFS merge point | D6 | 2 confirmed, stated as a floor | Low on the number, medium on the mechanism | **High** | **APPLY LAST, behind a flag, with full byte-level diff** |
| R1 | Exempt length-1 strings inside `is_plausible_field_name` | D5 | duplicate of P1 | n/a | Widens all five tiers and NAMECALL | **REJECT (superseded by P1)** |
| R2 | Suppress findings whose only read is `name or ...` | D2 part 3 | unknown | Low | Discards recall exactly where real bugs hide | **REJECT for now** |
| R3 | Scope the `assigned` scan by indentation | D2 part 2b | 0 | n/a | Can **fabricate** findings, not purely suppressive | **DEFER, unbundle from P3** |

Note the ordering is not by defect count. The 60-count patch (P3) sits below two
patches worth 7 and 9, because P3 fixes zero real defects and its gain is
precision in the thermometer, while P1 and P2 change output that is objectively
wrong today.

---

## P1. Canonical AUX resolution bypasses the control-byte filter

**Source:** diagnosis 3. **File:** `crates/luau-core/src/decompiler/lifter/mod.rs`,
`get_table_string_from_aux` (line 7330).

### Why rank 1

Highest `(confidence x gain) / risk` of the six by a wide margin.

- Mechanism is Tier 1 verified by reading the function. `get_table_string_from_aux`
  pipes **every** resolution tier, including the canonical `proto.constants[aux]`
  hit, through `is_plausible_field_name` (line 7347), whose own doc comment says
  it exists to "reject garbage strings resolved by **the fallback tiers**". It is
  wired to gate the primary tier as well. That is a one-line scope error in the
  code, visible without running anything.
- The evidence that survives the opmap trap: the constant table dump, which is
  not opcode-decoded.
- Blast radius is enumerated rather than estimated: `field_\d+` occurs in exactly
  one file of 1142, and `method_\d+` in zero.
- It is a real correctness fix, not a count fix. `pprint`'s escape table is
  currently emitted with seven fabricated keys, so
  `self:gsub("([%c\\])", tbl)` misses every control character at runtime. The
  seven `undefined_local` findings are the visible half; the broken table is the
  half the checker cannot see.

### Expected defects fixed and confidence

7 counted `undefined_local` rows (`field_39/41/43/45/47/49/51`, all in
919_ReplicatedStorage_pprint.lua lines 9-15). Diagnosis 5 says 8 for the same
site, counting the table-semantics row separately. **Confidence: high** on 7,
medium on whether the eighth row is a distinct counted defect. Corpus effect:
one file moves from broken to clean, so semantic goes 1082 to 1083 of 1142
unless something else regresses.

### Regression risk, concretely

The shape of a currently-clean file this could break:

1. A file where a GETTABLEKS/SETTABLEKS AUX word is **not** actually a constant
   index (a misdecoded or shuffled instruction) and the raw value happens to land
   on a short unrelated string constant that contains a control byte. Today that
   coincidence is caught by the filter and degrades to a visible `field_N`. After
   the patch it silently becomes a plausible-looking wrong key. Nothing in the
   corpus currently exercises this, so it is stated as residual, not measured.
2. A file where the canonical slot holds a **non-string** constant.
   `resolve_aux_string_at` Strategy 1 calls `const_to_string`, which can
   stringify things that are not `Constant::String`. **Hardening required:** the
   new `resolve_aux_string_canonical` must match on `Constant::String(_)`
   directly rather than reusing `const_to_string`. This is a tightening relative
   to the diagnosis's own spec and costs nothing.
3. A file that newly emits a bracketed control-byte key and trips the compile
   gate. Checked at Tier 1: `TableField::Named` (emit.rs:561) and `Expr::Field`
   (emit.rs:382) both gate on `is_valid_identifier` and fall back to
   `emit_string`, which zero-pads control bytes to `\007`. Valid Luau. Risk is
   low but the gate must still be run on 919 specifically, because that file has
   never exercised this emission path before.

### Verdict

**Apply.** Keep the non-empty and `<= 200` sanity checks on the canonical path.
Do not touch `is_plausible_field_name` itself, so `get_method_string_from_aux`
and the four speculative tiers keep today's behaviour exactly.

### Measure immediately after

1. `grep -rn "field_[0-9]" target/<out>/*.lua` must return **zero** hits corpus-wide
   (was 7 in one file). `method_[0-9]` must stay at zero.
2. 919_ReplicatedStorage_pprint.lua must contain seven bracketed control-byte
   keys and must pass the compile gate individually.
3. Per-file SHA-256 manifest diff over all 1142 outputs: **exactly one file may
   change.** If a second file changes, stop, because the canonical tier is firing
   somewhere the analysis did not predict.
4. Full gate block (below).

---

## P2. Store the CFG's correct branch PC and delete `find_branch_pc`

**Source:** diagnosis 1. **Files:** `crates/luau-core/src/analysis/cfg.rs`
(`BasicBlock`, `ControlFlowGraph::build`), `crates/luau-core/src/decompiler/lifter/mod.rs`
(`find_branch_pc` at 3376 and its six call sites at 1920, 1923, 1966, 2158,
2363, 2403, all confirmed present).

### Why rank 2

The mechanism does not need the disassembly at all. It is provable at Tier 1
from two facts in our own tree:

- `find_branch_pc` (mod.rs:3376-3391) steps **backward** from `block_end` and
  asks whether the word at `block_end - 2` `has_aux()`. That question is not
  decidable backward, because the word may itself be an AUX payload whose value
  decodes as an aux-carrying opcode.
- `cfg.rs` lines 172-180 already carry a comment describing **this exact failure
  mode by name**, including the same example shape (`GETTABLEKS <aux> JUMPIF`),
  and `ControlFlowGraph::build` was rewritten to walk **forward**,
  instruction-aligned, to avoid it. The correct `last_pc` is computed at lines
  181-190 and then discarded instead of being stored on the block.

So the repo has already diagnosed and fixed this bug once, in one of the two
places that needs it, and left the other place unfixed. That is a much stronger
warrant than any single-file reconstruction, and it holds even if the
609_ParticleTools instance evidence has to be re-derived with the pooled cache.

The 609 reconstruction (AUX 31 misread as opcode 31 JumpIfNotLE, the real
JUMPIFNOT word 0x1A consumed as its aux operand and printed as `v26`,
zero-extended upper byte printed as register 0 / `arg1`) is arithmetically
self-consistent and Tier 3 corroborated, but it is an instruction-level claim
and is therefore **provisional** until re-run with `--opmap-cache`. Rank does
not depend on it.

Diagnosis 1's reclassification is also correct and worth recording: this is not
a `free_var_decls` defect. `free_var_decls.rs` is applying its own documented,
tested contract to corrupt upstream input. Fixing the checker or the declaration
heuristic here would have been the wrong move.

### Expected defects fixed and confidence

9 counted `declared_never_assigned` rows in the line-7 hoist cluster.
**Confidence: medium-high.** The mechanism is high confidence; the mapping from
mechanism to exactly nine rows is a grouping judgement over the census. Expect
the true number to move in either direction, and expect **uncounted** wins:
every fabricated `if arg1 <= v26 then` that this removes was a wrong branch
condition the checker never scored.

### Regression risk, concretely

The shape of a currently-clean file this could break:

1. **A file whose output is currently correct only because a second bug
   compensates for the wrong branch PC.** This project has precedent for exactly
   that shape (`merge_regs`). A block that today accidentally starts its
   condition extraction one word early may be paired with a downstream handler
   that has been tuned to that offset. Symptom after the patch: a file that was
   clean becomes broken, or a branch condition inverts.
2. **Blocks with no branch terminator.** `branch_pc` must be `Option<usize>` and
   must be populated without panicking for blocks ending in RETURN or falling
   through to proto end. Mirror the existing `if block_end == 0 { continue }`
   graceful-skip style already used at cfg.rs:169.
3. **Call sites that pass a block end that is not a real CFG block.** Two of the
   six call sites take `cb.end` and `cond_block.end` rather than `block.end`.
   Confirm those objects are the same `BasicBlock` type carrying the new field,
   and do not silently fall back to `block.end - 1` for them, because a silent
   fallback reintroduces the old wrong answer for a subset of sites and makes
   the measurement uninterpretable. Log the fallback.

### Verdict

**Apply.** Order it before P5, because P5's patch reads the last real
instruction of the then-arm and P2 changes which PC that is judged to be.

### Measure immediately after

1. Count of the fallback log line (Option is None) across the corpus. Expect a
   small, explainable number. A large number means the field is not being
   populated where the lifter needs it.
2. Corpus-wide count of emitted comparison operators against generated
   registers. Specifically: does any file still emit a comparison whose
   right-hand side is a `vN` that never appears on a left-hand side? That
   pattern was the fingerprint of the fabricated condition.
3. Per-file SHA-256 manifest diff. Enumerate every changed file. For each file
   that was **clean before and changed**, read the diff. Net counts alone are
   not admissible here.
4. 609_ReplicatedStorage_ParticleTools.lua `MakeHoneyText` must render a
   single-operand truthy test, not a comparison.
5. Full gate block.

---

## P3. Narrow `check_declared_but_never_assigned`

**Source:** diagnosis 2, parts 1 and 2a only.
**File:** `crates/luau-core/src/decompiler/semantic_check.rs`, function at line 347.

### Why rank 3 despite the largest count

60 of the 125 defects is the biggest single number in the plan, and it ranks
third because **it fixes zero real defects**. The gain is precision in the
measuring instrument. That is worth having before the riskiest patch lands, but
it is not worth having before the two patches that repair objectively wrong
output.

### What is actually wrong (Tier 1, read from the function)

Confirmed by reading lines 347-455:

- Step 3 (line 413) builds the `read` set by splitting every line in the
  **entire file** on non-alphanumerics, discarding the delimiter. So `math.sin`,
  `table.insert`, `x.CFrame` all insert bare `sin`, `insert`, `CFrame` into
  `read`, and an unrelated correctly-scoped `arg1` or `self` in any other closure
  in the file counts as a read of the flagged one. The printed "read N times"
  totals (146, 134) are whole-file aggregates and are meaningless as evidence of
  anything.
- There is no scope tracking anywhere in the function.

That is a defect in the checker's own logic, provable without looking at a
single decompiled file.

### What to apply, and what to unbundle

**Apply part 1:** record the delimiter preceding each token and do not insert a
token into `read` when that delimiter is `.` or `:`. Kills the
`sin`/`cos`/`floor`/`insert`/`CFrame` class. Purely suppressive.

**Apply part 2a only:** scope the **read** scan to lines at indentation greater
than or equal to the declaration's own indentation, walking forward until a line
of strictly lesser indentation. Kills the `arg1`/`self` cross-closure class.
Purely suppressive. Note the scan currently iterates trimmed lines, so the
indentation must be taken from `raw`, not `t`.

**Defer part 2b (R3):** the diagnosis proposes scoping the `assigned` scan too,
and claims parts 1 and 2 "can only suppress existing findings, never fabricate
new ones". **That claim is false for the assigned half.** The finding fires when
a name is bare-declared, **not in `assigned`**, and in `read`. Shrinking `read`
can only suppress. Shrinking `assigned` removes names from the exclusion set and
can therefore **create** findings. It is also not needed: the `arg1` and `self`
false positives are killed by the read-scan narrowing alone. Unbundle it, and if
it is ever wanted, measure it as its own step.

**Reject part 3 (R2):** suppressing findings whose only read is
`name or ...` / `name and ...` / `if name then`. Three reasons. It is the one
change that would suppress a real defect class, because a dropped assignment
read through a defensive guard looks identical. Its supporting evidence is Tier
3 only, since the Tier 2 attempt desynced and was voided. And the desync has a
known reopen path (below), so the honest move is to get the evidence rather than
to widen the suppression.

### Expected defects fixed and confidence

Up to 60 findings removed, **0 real defects fixed**, and only if every one of
them is a false positive. **Confidence: medium.** The two mechanisms are proven;
the attribution of all 60 rows to those two mechanisms is not. Treat 60 as a
ceiling, not an estimate.

### Regression risk, concretely

Nothing in decompiled output can change: this function only emits `Finding`s.
The risk is recall, and one measurement-integrity risk that matters more:

1. **Indentation as a proxy for scope.** A currently-flagged genuine defect
   inside output where the emitter's indentation is irregular (a collapsed
   one-line block, a long-string literal whose continuation lines start at
   column 0, an inline table spanning lines) would fall outside the computed
   window and go unflagged. Long strings are the concrete shape: a line inside
   `[[ ... ]]` at column 0 terminates the window early.
2. **Do not refactor the tokenizers together.** Confirmed at Tier 1:
   `check_undefined_locals` (line 275) and `check_declared_but_never_assigned`
   (line 413) each carry their **own** copy of the same split expression. They
   are duplicated, not shared. Merging them into one helper while implementing
   P3 would silently apply the narrowing to `check_undefined_locals` too, which
   would suppress the `field_39..51` rows that P1 exists to fix, and could hide
   real `undefined_local` defects. Keep them separate. Say so in the commit.
3. **Metric discontinuity.** After P3 the 94.7% number is measured by a different
   instrument. Every subsequent report must carry the checker revision, and the
   post-P3 baseline must be published under both the old and the new checker so
   the trend line stays comparable.

### Verdict

**Apply parts 1 and 2a.** Defer 2b. Reject part 3. Gate on the audit below.

### Measure immediately after

1. **Per-instance audit, mandatory before landing:** dump the full list of
   findings that disappear, all ~60. Each must be classified as a proven false
   positive with the reason (field access, cross-scope name reuse). Any finding
   that disappears for a reason not on that list blocks the patch. This is the
   step that separates narrowing from gaming.
2. Byte-identical output: the SHA-256 manifest over all 1142 outputs must show
   **zero** changed files. If any file changes, the patch touched something it
   should not have.
3. Findings that **appear** must be zero. If any appear, part 2b leaked in.
4. The existing checker unit tests must all keep their current expected outcomes.
5. Full gate block, reported under both checker revisions.

---

## P4. Narrow `check_name_body_agreement` on ancestor containment

**Source:** diagnosis 4. **File:** `crates/luau-core/src/decompiler/semantic_check.rs`,
`check_name_body_agreement` (line 100), plus `Decl` to make containment queryable.

### Why rank 4

High confidence, tiny risk, near-zero value. It removes one finding and fixes
zero defects. It ranks above P5 only because P5 can corrupt files and this
cannot. The same conclusion is already recorded in `docs/DEFECT_CENSUS_218.md`
("the check tripped on a convention, not a mispairing"), so this patch is that
prior verdict being implemented, now with Tier 2 backing that survives the opmap
trap: proto 21's debug name is `inspectSF` and its own constant pool carries the
`"Stickers.RepairInvalidStickersInBook: ..."` strings. Neither debug names nor
constant pools are opcode-decoded.

### Expected defects fixed and confidence

1 finding removed, 0 real defects. **Confidence: high.** The check fires exactly
once in the 125.

### Regression risk, concretely

The shape that breaks: a file where `collect_declarations`'s line-based
`end`-matching miscounts (a `do ... end`, a table literal closing with `end,` or
`end)`) so that two unrelated sibling declarations **appear** nested. The
containment test would then suppress a real mispairing. The real defect class
this check exists to catch (SETTABLEKS mispairing between two `function tbl.X`
bindings) is structurally sibling-to-sibling, so containment is the right
discriminant, but only if containment is computed correctly.

Also note the check currently fires once. A narrowing that suppresses one
finding cannot be validated statistically. It is validated by reading the one
case, which has been done at Tier 2.

### Verdict

**Apply**, in the same PR as P3 but as a **separate commit with its own
measurement**, so the two checker changes never share a delta.

### Measure immediately after

1. Corpus-wide before/after list of `name_body_mismatch` findings. Exactly one
   must disappear (855_Stickers). Any second disappearance must be spot-checked
   against bytecode and oracle before the patch lands.
2. The existing `catches_name_body_mismatch` regression test must still pass
   unchanged (its `tbl.Create` / `tbl.ServerCall` pair are non-nested siblings,
   so containment is false in both directions).
3. Add `accepts_nested_helper_quoting_enclosing_name` as specified.
4. Zero changed output files in the manifest.

---

## P5. Prefer the then-arm's own escape JUMP over the BFS merge point

**Source:** diagnosis 6. **File:** `crates/luau-core/src/analysis/structuring.rs`,
the IfThenElse branch at 331-368 (confirmed: `find_merge_point` is called at
line 337, `has_else` is computed at 344-345 exactly as described).
**Explicitly not** `cfg.rs::find_merge_point`, which is shared with
`value_region.rs::find_value_join`.

### Why rank last despite being the most severe defect

This is the highest-severity class in the plan and the lowest-ranked patch, and
both of those are correct.

Severity: 897_Plane3D `FromNormalAndPoint` currently always returns
`Alignment = 0` regardless of geometry, and 530_SeeArms emits a `break` that
does not exist in the bytecode, which would terminate a loop early. Both files
**compile clean and score clean**. They are false negatives, which is a worse
failure than a counted defect.

Rank: the patch is unimplemented, unmeasured, and its blast radius is unbounded.
It reinterprets a jump target for every two-successor block in the corpus. The
diagnosis itself names the precedent: a previous fix gained 4 clean files while
corrupting 2 previously-clean ones. This patch has that exact shape. The count
(2) is explicitly a floor from a **biased** sample: 7 files manually diffed out
of a 967-file pool screened by an if/for/while divergence heuristic, of which 3
had genuine defects. That does not extrapolate, and the diagnosis says so, which
is to its credit.

### Expected defects fixed and confidence

2 confirmed instances, stated as a floor. **Confidence: low on the number,
medium on the mechanism.** Note that neither of the two is currently in the 125,
because the checker scores both files clean. So this patch's expected effect on
the headline number is **zero or negative**, and that is fine: it is the patch
where the count is the least useful signal in the whole plan.

### Regression risk, concretely

The shape of a currently-clean file this could break:

1. **A then-arm whose trailing unconditional JUMP belongs to an enclosing
   structure.** A `continue`-equivalent jump inside a loop that is not a
   JUMPBACK, or the tail of an already-correctly-lifted nested if whose own
   merge sits before this if's naive-BFS merge. Reinterpreting that target as
   this if's merge steals blocks from the enclosing region. Symptom: statements
   migrate into the wrong block, or an enclosing loop body loses its tail.
2. **Interaction with `find_value_join`.** The two paths share
   `find_merge_point`. Today `find_value_join` declines side-effecting arms and
   those fall through to the plain IfThenElse path. If the corrected merge fires
   on a shape `find_value_join` currently claims, the two corrections can stack.
3. **Interaction with P2.** P2 changes which PC is identified as a block's
   branch instruction. P5's rule inspects the last real instruction of the
   then-side. Landing P5 on a pre-P2 tree measures a different function.

### Verdict

**Apply, last, behind a feature flag, and only under these conditions:**

- P1 and P2 have landed and their measurements are clean.
- The instance list is **re-derived after P2**, because P2 may change the shape
  of 897 and 530.
- JUMPBACK is excluded from the rule, as the diagnosis specifies (a then-arm
  inside a loop legitimately ends with one).
- The acceptance criterion is **not** the net clean count. It is: every file
  whose bytes change is enumerated, and every file that was clean before and
  changed is read and manually confirmed correct. If that review cannot be
  completed, the flag stays off and the patch does not count.

### Measure immediately after

1. Manifest diff first, before any gate number. Enumerate changed files.
   Partition into (clean -> clean, bytes changed), (clean -> broken),
   (broken -> clean), (broken -> broken). The second bucket must be empty. The
   first bucket must be read file by file.
2. `value_region.rs`'s own tests (`find_value_join` ladder, diamond, and the two
   negative cases at lines 307-419) must pass unchanged.
3. 897_Plane3D must emit if/elseif/elseif/else with per-arm `Alignment` writes
   and **no** trailing unconditional `tbl2.Alignment = 0`.
4. 530_SeeArms must contain **no** `break` token.
5. Corpus-wide delta in the count of emitted `break` statements and in the count
   of top-level sibling `if` blocks. Both are direct fingerprints of this bug
   class; a large swing in either direction is a signal to stop and read.
6. Full gate block.

---

## Rejected outright

### R1. `is_plausible_field_name` length-1 exemption (diagnosis 5)

**Reject as a patch. Keep its evidence.** It targets the same seven defects in
the same file as P1 and cannot be applied alongside P1.

Diagnosis 5's evidence is good and partly better stated than diagnosis 3's: the
constant-table pairing (K38/K39, K40/K41 and so on), the observation that the
eighth entry `"\\"` (0x5C, printable) resolves correctly and sits exactly on the
filter's boundary, the ruling-out of the DUPTABLE path, and the explicit refusal
to cite the garbled instruction listing. All of that is adopted into P1.

The patch is the wrong instrument:

- `is_plausible_field_name` is shared. Editing it changes behaviour for
  `get_method_string_from_aux` (NAMECALL) as well, which the analysis says has
  zero occurrences in the corpus and therefore does not need touching.
- It widens **all five** resolution tiers, including Tier E hash reverse-lookup,
  which is the exact source of the garbage the guard was added for (B0.113,
  `%*\n%*`, 94 occurrences in ClientScript). A spurious length-1 hash collision
  would now be accepted and emitted as a plausible-looking key. That is worse
  than `field_N`, because `field_N` is visibly wrong and a bogus `["\4"]` is not.
- P1 achieves the same result while touching only the one tier the bytecode spec
  makes canonical.

### R2. Suppress findings read only through `or` / `and` / `if` (diagnosis 2 part 3)

**Reject for now.** Reasons in P3. It is the only proposed change that would
suppress a real defect class, and its supporting bytecode evidence does not
exist yet. It is reopenable, see the dead-ends section.

### R3. Scope the `assigned` scan (diagnosis 2 part 2b)

**Defer and unbundle.** It is not required by any documented false positive, and
unlike the rest of P3 it can create findings rather than only suppress them. The
diagnosis's own "can only suppress, never fabricate" claim does not hold for
this half.

---

## Patches that touch the same code path

1. **P2 and P5.** Both sit on the if/else structuring path. P2 changes which PC
   the lifter reads as a block's branch instruction; P5 changes which blocks form
   the then and else regions and where the merge is. They feed the same
   `Region::IfThenElse` construction. **Order: P2, then re-derive P5's instance
   list, then P5.** Do not measure P5 against a pre-P2 baseline.
2. **P1 and R1.** Same function, same seven defects, mutually exclusive
   implementations. Only one may land, and it is P1.
3. **P5 and `value_region.rs`.** Shared `find_merge_point` primitive. P5
   deliberately avoids editing it, which is the right call, but the two
   corrections can stack on shapes where `find_value_join` starts claiming or
   declining differently. `value_region`'s tests are a required gate on P5.
4. **P2 and P3.** The line-7 rows P2 fixes are counted by the same checker
   function P3 narrows. Freeze the checker at its pre-P3 revision while measuring
   P2, otherwise the two deltas are not separable.
5. **P1 and P3.** `check_undefined_locals` (line 275) and
   `check_declared_but_never_assigned` (line 413) contain **duplicate**
   tokenizers. P3 must edit only the second. Any tidy-up that unifies them would
   apply P3's narrowing to the exact findings P1 is supposed to eliminate by
   fixing output, which would make P1 unmeasurable and could hide real
   `undefined_local` defects elsewhere.
6. **P3 and P4** both edit `semantic_check.rs`. Separate commits, separate
   measurements.

---

## Which patches narrow the checker, and why that is legitimate here

**P3 and P4 are checker narrowings. They change zero bytes of decompiled output.**

The legitimacy argument, plainly:

A `Finding` is a claim about the emitted `.lua` file. For `declared_never_assigned`
the claim is: "this local is bare-declared, never assigned, and read, therefore
nil at every read." For the `sin` / `insert` / `CFrame` rows that claim is
**factually false about the file**, and it is false for a reason visible in our
own source: the checker counts the tail of `math.sin` as a read of a local named
`sin`. For the `arg1` / `self` rows it is false because the checker has no scope
tracking and aggregates every same-named variable in the file. Deleting a false
claim raises precision. It does not move the thing being measured.

The line between narrowing and gaming, and where each proposal falls:

| Test | P3 parts 1, 2a | P4 | R2 (rejected) |
|---|---|---|---|
| Justified by a defect in the checker's own logic, not by which files it flags | Yes, Tier 1 | Yes, Tier 2 | No, justified by an idiom the flagged files happen to use |
| Every suppressed finding individually audited | Required and gated | Yes, n = 1, read at Tier 2 | Not possible, evidence voided |
| Output bytes unchanged | Yes, gated on the manifest | Yes | Yes |
| The suppressed instances proven non-defective | Yes for the two named mechanisms | Yes | **No** |
| Paired with a widening where a false negative is proven | See below | See below | n/a |

That last row is the one that keeps this honest. **A plan that only narrows
checkers is gaming the metric even when each narrowing is individually correct.**
The counterweight is already in hand and is recorded here as an obligation, not
an option:

- The checker reported **zero** `bodies_dropped` on this run, while
  396_TadpoleTailInit demonstrably drops 4 of 7 proto bodies and truncates a
  fifth, confirmed at Tier 1 from the raw proto table via `luau-decompiler info`
  with no opmap involved. That is a proven false negative in a check that is
  currently scoring zero.
- P5's entire class (897, 530) is scored clean by the checker while the output is
  semantically wrong.

So the same PR series that removes ~61 false positives must also record, in the
census, that at least two proven false-negative classes exist and that the
94.7% number is an overestimate in both directions. Report the narrowed number
and the old number side by side, with the checker revision attached.

---

## Dead ends, recorded so they are not retried

Hypotheses that were tested and failed. Each is listed with its tier and whether
it can be reopened.

**Closed, do not retry:**

1. `free_var_decls.rs`'s read-only-inside-a-closure heuristic is the cause of the
   line-7 cluster. **Ruled out (Tier 1).** It implements its documented contract
   correctly on corrupt upstream input.
2. The line-7 and non-line-7 `declared_never_assigned` rows share one mechanism.
   **Ruled out by shape.** Line-7 rows are always the chunk's first statement;
   non-line-7 rows are scattered mid-function and are checker artifacts.
3. `arg1` in 238_EggTypes is genuinely read 146 times in one scope, and `self` in
   879_BigNum 134 times. **Ruled out.** Whole-file aggregation artifacts.
4. 609_ParticleTools is an instance of the documented if/else-arms-delete-their-
   for-loops class, or of the rare 0x6F LOADK/JUMPIFNOT opcode-remap swap
   (113_JumpPower, 258_SunBear2Init). **Ruled out**, it is neither.
5. The opmap mis-decoded the 609 branch. **Ruled out** for that PC, measured
   database entry, no rescue heuristic invoked.
6. `cap_0` in 893_Matrix3D and `v7` in 71_PlaceInfo are the same mechanism as the
   `field_N` class. **Ruled out** by reading each site. `cap_0` looks like the
   CAPTURE / child_protos class; `v7` looks like the branch-merge class. Neither
   is fixed by P1, and P1 must not be credited with them.
7. The DUPTABLE / `constant_to_expr` path (mod.rs:2053) produced the pprint
   escape table. **Ruled out**: that path has no `field_N` fallback at all, it
   drops unresolvable entries silently, so the literal string `field_39` in the
   output proves the SETTABLEKS/GETTABLEKS path fired.
8. The 855_Stickers `name_body_mismatch` is a genuine SETTABLEKS mispairing.
   **Ruled out (Tier 2)**: proto 21's own debug name is `inspectSF`.
9. Our lifter invented or misattributed the `"Stickers.RepairInvalidStickersInBook: ..."`
   strings. **Ruled out (Tier 2)**: they are proto 21's own constant-pool entries.
10. The garbled `disassemble --opmap` output for 897 and 530 reflects a bug in
    those files' logic. **Ruled out**: printer artifact from incomplete opcode
    coverage. See the tooling correction at the top.
11. The opmap DB entry is a wrong-build permutation for 897 and 530. **Ruled
    out**: 77 opcodes pinned by genuine anchor match, not a version heuristic.
    The problem was coverage, not identity.
12. 740, 719, 703 and 233's if-count divergence from the oracle indicates
    semantic defects. **Ruled out by reading**: 233 is formatting only; 740 and
    719 are correctly-nested if/else not flattened to `elseif` keyword form by
    `collapse_elseif_chains`; 703's arms all end in `return`, which sidesteps the
    class entirely. Do not re-flag these.

**Reopenable, with the key:**

13. **Bytecode confirmation of diagnosis 2's sub-mechanism B** (the
    never-assigned-upvalue-read-only-through-`or` idiom). The attempt disassembled
    238_EggTypes `pushType` and 620_Beesmas2020Quests `Cutscene` and desynced into
    UNKNOWN opcodes and impossible operands (`SETUPVAL R60 U224` against
    stack=14 / upvalues=5) a few instructions past a zero-offset JUMPIFNOT, and
    the diagnosis correctly declared everything past that point void.
    **That desync is the documented `--opmap-cache` trap, not a property of those
    protos.** `docs/DEFECT_CENSUS_218.md` records that `disassemble --opmap`
    never consults the DB, and diagnosis 6 reproduced the fix by supplying the
    corpus-pooled cache. **Reopen key:** re-run via `tests/census_disasm.rs`
    (which applies the DB entry verbatim through `OpcodeMap::from_exact_map`,
    i.e. the decode the lifter actually saw), or with
    `--opmap-cache <pooled .opmap_pool.jsonl from the same batch run>`. If that
    produces a clean stream, R2 becomes decidable on evidence rather than on
    idiom-matching. Until then R2 stays rejected.
14. **The 396_TadpoleTailInit dropped bodies.** Real (Tier 1, raw proto table:
    `initSegment` 52 insns, `destroyWelds` 35, a 16-insn callback,
    `monitorSegments` 41, plus a 69-insn closure truncated to ~6), but not root
    caused by any of the six diagnoses, and overlapping a `bodies_dropped`
    investigation this project already records as having two measured dead ends.
    **Not a patch in this plan.** Recorded here as (a) an open defect and (b)
    proof that `bodies_dropped` reporting zero on this run is a false negative.

---

## Standing gate block, run after every single patch

Per the project's clean-run rule: **empty the output directory first and confirm
`resumed=0`**, otherwise the numbers mix old-map files and the measurement is
void.

1. `cargo test -p luau-core` (library tests).
2. Compile gate: `our_output_compiles` in `crates/luau-core/tests/compile_gate.rs`.
   Baseline 1140/1142.
3. CoreScript ground truth: `crates/luau-core/tests/corescript_ground_truth.rs`,
   9/9. This is the only place with real source to compare against; it must never
   regress.
4. Semantic scan: `scan_whole_corpus` in
   `crates/luau-core/tests/semantic_corpus_scan.rs`. Baseline 1082/1142 clean,
   125 defects.
5. **Per-file SHA-256 manifest of all 1142 outputs, before and after, with the
   changed-file list enumerated.** This is the step the `merge_regs` episode
   exists to justify. Net gate counts are not admissible on their own for P2 or
   P5, because a patch can gain N files and corrupt M others and still look
   positive.
6. Tag `backup/pre-<patch>` before each patch, per the project's backup rule.

**Stop conditions.** Any of these halts the sequence and reverts the patch:

- CoreScript truth drops below 9/9.
- Any file moves from clean to broken and is not explained within the same step.
- A file's bytes change that the patch's blast-radius analysis predicted would
  not change (P1 predicts exactly one file; P3 and P4 predict zero).
- The compile gate drops below 1140/1142.

---

## Summary of what this plan claims

- **Two patches repair objectively wrong output** and should land first: P1
  (7 defects, one file, mechanism visible in the source, blast radius
  enumerated) and P2 (9 defects, mechanism proven at Tier 1 by a comment in our
  own tree that describes the bug and fixes it in the other of the two places
  that needed it).
- **Two patches narrow the checker** and fix zero defects: P3 (up to 60 findings)
  and P4 (1 finding). Legitimate, because the suppressed claims are provably
  false about the files, and gated on a per-instance audit plus dual reporting.
  Not legitimate if landed without the paired admission that `bodies_dropped`
  and the P5 class are proven false negatives.
- **One patch is high-severity and high-risk** and goes last behind a flag: P5,
  worth 2 confirmed defects that the checker cannot see, with the same blast
  shape as a fix this project already had to reject.
- **Three proposals are rejected or deferred:** R1 as superseded and too wide,
  R2 for suppressing exactly where real bugs hide on evidence that was voided,
  R3 because it can fabricate findings and no documented false positive needs it.
- **The headline number will move down before it moves up in truth.** Expect
  roughly 125 -> 116 from P1 and P2 (real), then -> ~56 from P3 and P4
  (precision, not repair), while the honest count of known-wrong files goes
  **up** by at least the three files (897, 530, 396) that score clean today and
  are not.
