# Decompiler journal — drive to 100%

Baseline at start (clean tree, commit 1448044):
- decode    1,142 / 1,142  (100%)
- semantic  1,097 / 1,142  clean (96.1%), 97 defects reported
- sweep       793 / 1,142  clean (69.4%), 249 broken, 95 suspect, 5 error

Rules in force: measure on a clean out-dir (resumed=0); mutation-test every fix;
revert anything that lowers the clean count and journal the numbers.

---

## Mechanism index

| id | mechanism | state |
|---|---|---|
| A | declared_never_assigned (bare `local X`, never assigned) | ACTIVE |
| B | dropped call / block after RETURN in a multi-block span | PARKED (1) |
| C | dropped conditional SETTABLEKS when a table folds to a literal | OPEN |
| D | name_body_mismatch (1 file) | OPEN |
| E | undefined_local (1 file) | OPEN |

---

## 2026-08-19 — the reported count is CAPPED

`semantic_check.rs:530-563`: `reported` counts every bare-never-assigned name,
but only the first 6 emit an individual Finding; the rest collapse into one
"... and N further names" Finding. So a file with 44 such names contributes 7
findings, not 44.

Consequence: the headline "97 defects" is a count of FINDINGS, not of defects.
The true number of always-nil names is higher and must be recovered before any
claim about progress on mechanism A.

## 2026-08-19 — TRUE baseline recovered

Probe (`tests/_tmp_true.rs`, throwaway) expands the "... and N further" summaries:

    FILES_WITH_FINDINGS 45
    REPORTED_FINDINGS   97
    TRUE_DEFECTS        349
      declared_never_assigned  95 findings  -> ~347 true
      name_body_mismatch        1
      undefined_local           1

So mechanism A is 99.4% of all semantic defects. Everything else is 2 defects in
2 files. The target is A; D and E are rounding.

(Coincidence worth not confusing: the sweep also has 349 non-clean FILES. Unrelated
number, same value.)

### Mechanism A — what is already ruled out
- `hoist_escaping_locals` (mod.rs:3150) matches only single-name-single-init
  `Stat::Local`, so it cannot emit the multi-name bare blocks these sit in.
- The loop hoist (`hoist_loop_locals`, mod.rs ~4043) was fixed earlier today
  (commit 5b90280) and is NOT this: it now only hoists names new to the loop.

### Next question
Which pass emits a MULTI-NAME bare `local a, b, c`? Candidates seen so far:
mod.rs:1907 (`values: vec![]` in the value-join fallback) emits ONE name per
statement. So either something coalesces adjacent single-name Locals, or a
different site emits the multi-name form directly.

## 2026-08-19 — Mechanism A: emitter identified and the discriminant located

MEASURED, not inferred:
- Disabling `free_var_decls` (decompiler/mod.rs:1983) removes the bare
  `local Transparency3, v6` block from 215_CollectiblesAnimator entirely.
  So that pass emits mechanism A's declarations. Confirmed by rebuild + diff.
- `LUAU_MINT_TRACE=1` on the same file:

      MINT  ASSIGNED  Transparency3  inclosure=true  sites=NONE
      MINT  READONLY  v6             inclosure=true  sites=REG_EXPR_UNKNOWN+UNK_MERGE_COMPOUND

`Transparency3` came from the ASSIGNED branch and IS assigned (:153) - defensible.
`v6` came from the READONLY branch (free_var_decls.rs:194-202), whose discriminant
is "read only inside a closure => consistent with a capture".

THE DISCRIMINANT IS TOO WEAK. The pass's own comment (:158-192) states the two
cases it must separate:
  * genuine captured upvalue missing its declaration -> declaring is RIGHT
  * a value the lifter dropped                       -> declaring is WRONG, and
    worse than the defect it replaces because it is SILENT
v6 satisfies "read only inside a closure" and is nonetheless a dropped value:
its mint sites are REG_EXPR_UNKNOWN + UNK_MERGE_COMPOUND, i.e. the name was
invented because a register held no known value at a merge.

KEY: the pass ALREADY has the better discriminant and only logs it.
`mint_trace::sites_for(n)` is consulted solely inside the `if enabled()` block.
A name minted at REG_EXPR_UNKNOWN / UNK_MERGE_COMPOUND is a hole by construction,
not a capture.

NEXT: are mint sites recorded unconditionally, or only when LUAU_MINT_TRACE is
set? If gated, the site record must be made always-on (cheap) before it can be
used as a decision input.

## 2026-08-19 — Mechanism A splits into A1 (masking) and A2 (root cause)

`mint_trace::note` early-returns unless LUAU_MINT_TRACE is set, so sites are NOT
available as a decision input without making recording unconditional.

MORE IMPORTANT — the obvious fix does not reach the goal. Refusing to declare a
READONLY hole leaves the name undeclared, which converts a
`declared_never_assigned` finding into an `undefined_local` finding. Both are
semantic defects. The clean count would not move; the label would.

So:
  A1  free_var_decls declaring dropped values, masking them silently.
      Real, and the pass's own comment says so. But fixing A1 ALONE cannot
      raise the clean rate - it only makes the same holes visible.
  A2  THE ROOT CAUSE: the lifter drops the value, minting a name at
      REG_EXPR_UNKNOWN / UNK_MERGE_COMPOUND (mod.rs:3448) instead of carrying
      the register's real value through the merge.

A2 is the target for 100%. A1 is a correctness/honesty fix to make afterwards,
once the holes it would expose are actually gone.

State: A -> A2 ACTIVE, A1 OPEN (deliberately deferred, reason above).

## 2026-08-19 — A2 traced to the exact decision point

Chain, every step measured:

1. `free_var_decls` emits the bare block (proved: disabling it removes the block).
2. `v6` enters via the READONLY branch, sites `REG_EXPR_UNKNOWN+UNK_MERGE_COMPOUND`
   (proved: LUAU_MINT_TRACE=1).
3. `UNK_MERGE_COMPOUND` is `merge_regs`' terminal reset (mod.rs:3448) - reached
   only when neither side is a simple `Name` AND the two sides are not
   structurally equal (B0.80 equality is checked first, mod.rs:3362-3369).
4. Registers left `Unknown` there are meant to be repaired by the phi passes:
   - ternary form (mod.rs:2117) - needs pure-value arms, no side effects, leaves
   - statement form (mod.rs:2191) - needs BOTH arms to hold `RegVal::Expr`
5. The statement form's own measurement: of 225 defect names cleared at that
   reset, 202 had both arms holding an Expr and 193 were blocked purely by
   `expr_is_leaf` - which is why the statement form was added.

So the residue is the ~23 where ONE ARM IS NOT AN EXPR. For those the code
deliberately declines, with a measured justification: assigning on a single path
is the declaration-only hoist, tested and rejected (1699/303 -> 1674/335).

### A2 PARKED (1) - with the unblocking question

PROVED: v6 is cleared at the merge_regs terminal reset and repaired by neither
phi path.
RULED OUT: it is not an equality failure (B0.80 runs first); not the ternary
path's leaf cap (statement form has no leaf cap); not the one-path hoist (that
was measured and rejected, and re-adding it would regress).
SMALLEST REPRODUCER: 215_CollectiblesAnimator, name `v6`, one bare `local`.

THE ONE QUESTION THAT UNBLOCKS IT:
  For the registers that reach the terminal reset with ONE arm not holding an
  Expr - what is that arm actually holding, and WHERE was it cleared?
  Answerable cheaply: the reject reason is already reachable. Add a
  `note_unknown` at each `continue` in the statement-form loop (mod.rs:2196-2201)
  tagging which case rejected the register, then read the tags for v6. That
  names the upstream site that emptied the arm, which is the real A2 root.

NEXT ACTION when resumed: do exactly that, then attack THAT site rather than
merge_regs - merge_regs is the messenger.

## 2026-08-19 — ROTATION 1 COMPLETE. Baseline held at 1097/1142.

Tried the journaled next action: tagged every `continue` in the statement-form
phi loop (mod.rs:2196) with a PHIREJ_* reason and re-traced v6.

RESULT: no PHIREJ tag appeared for v6, and the tag is INCONCLUSIVE rather than
proof. Reason found by reading mint_trace:
  * `note_unknown(site, reg)` is keyed by REGISTER and OVERWRITES.
  * `sites_for(name)` reads MINTS, which is keyed by NAME and written at MINT
    time.
  * So a name's recorded sites are frozen when the name is minted. A PHIREJ tag
    applied to the register AFTER that mint would never appear in the name's set.
Therefore "no PHIREJ tag" does not establish that the phi loop skipped v6.

Instrumentation reverted. Clean re-measure on an empty out-dir confirms the
baseline is untouched: 1097/1142 clean (96.1%), 97 defects.

### Corrected next question for A2 (supersedes the one above)
The register->name association must be resolved at the SAME instant for both
facts, or the evidence cannot be trusted. Concretely:
  Instrument at the MINT point, not the reject point: when a name is minted for
  an Unknown register, record `unknown_origin(reg)` AND whether the phi loop had
  already run for that merge. That yields, per minted name, the site that
  cleared it and whether repair was even attempted.
Only then is it sound to say which merges lack repair.

### Mechanism states after rotation 1
| id | mechanism | state |
|---|---|---|
| A1 | free_var_decls masks dropped values (READONLY branch) | OPEN - deliberately deferred; fixing alone relabels, does not raise clean rate |
| A2 | lifter drops the value; register Unknown at merge_regs terminal reset | PARKED (1) - question above |
| B  | block after RETURN abandoned in a multi-block span | PARKED (1) - naive fix cost 13 files |
| C  | conditional SETTABLEKS lost when a table folds to a literal | OPEN - cause named, not attempted |
| D  | name_body_mismatch (1 file) | OPEN - untouched |
| E  | undefined_local (1 file) | OPEN - untouched |

Nothing regressed. No fix has been applied this rotation - the work was
establishing the true defect count (349, not 97) and tracing A to its decision
point.

## 2026-08-19 — ROTATION 2: mechanism A's ROOT CAUSE FOUND (supersedes A2)

### The census that redirected everything
`LUAU_MINT_TRACE=1` over the whole corpus: 590 minted names total.
    563 ASSIGNED   (490 sites=NONE, 72 GEN_VAR_NOHINT)
     27 READONLY   (11 UNK_CALL_CLEAR, 5 UNK_MERGE_COMPOUND, 3 UNK_MERGE_ARM_NAMES, ...)
Mechanism A is ~347 defects. The READONLY branch is 27 names corpus-wide, so the
whole A2 thread (merge_regs terminal reset) was chasing a MINORITY path.
A2 candidate fix (adopt the Expr when one side is Unknown) measured on a verified
fresh binary: 1097/1142, 97 defects - a NO-OP. Reverted. A2 is disproved as the
lever; it is real but marginal.

### THE ACTUAL ROOT CAUSE — closure parameters emitted as parent-scope locals
623_Beesmas2024BQuests, the densest file (35 findings), emits:

    local v12 = "Gingerbread House"
    local tbl2          -- bare
    local tbl3          -- bare
    local v13           -- bare
    local v14           -- bare
    local function Cutscene()                    -- NO PARAMETER LIST
        tbl2 = tbl2 or "CameraTarget"            -- optional-parameter idiom
        v12  = Toys2[v12]
        tbl3 = tbl3 or "CameraPos"
        ... ZoomDisplay(..., v13 or 1, v14 or 9999999999999)
    end

`x = x or default` is the Lua optional-parameter idiom. The bytecode agrees:

    ; === Proto 1 "getToyCutscene" ===
    ; params=5 stack=6 upvalues=0 vararg=false line=9

FIVE declared parameters. The output emits a ZERO-parameter function and hoists
all five into the enclosing scope as bare `local`s: v12, tbl2, tbl3, v13, v14.
Two of them (tbl2, tbl3) are then assigned inside the body so the checker does
not flag those; the read-only ones (v13, v14) are flagged. That is why the
defect count is large and concentrated in files full of small closures.

Also noted: the emitted function is named `Cutscene`, the proto is
`getToyCutscene`. Same site, probably the same attribution bug.

### Why this explains the distribution
- It is per-closure, not per-file, so files dense in small closures dominate.
- It produces BOTH `declared_never_assigned` (read-only params) and silently
  wrong behaviour (a parameter that is always nil, so `or default` always wins).
- It is invisible to the decode gate: the output parses and runs.

### State
A2  DISPROVED as the lever (no-op measured). Mechanism kept in the journal.
A3  NEW, ACTIVE: closure parameter list lost; params hoisted to parent scope.
    Smallest reproducer so far: 623_Beesmas2024BQuests Proto 1 `getToyCutscene`,
    params=5, emitted as `Cutscene()` with 4 bare locals + 1 assigned.
    NEXT: find where NEWCLOSURE/DUPCLOSURE emits the function head and why
    `params` from the child proto header is not used to build its parameter list.

## 2026-08-19 — ROTATION 3. CORRECTION: rotation 2's root cause was WRONG.

### What I got wrong
Rotation 2 claimed "closure parameters emitted as parent-scope locals", inferred
from `Proto 1 getToyCutscene params=5` next to an emitted `local function
Cutscene()` with no params. I matched proto to function BY NAME without checking
the output, and the file contains BOTH:

    15: local function getToyCutscene(arg1, arg2, arg3, arg4, arg5)   <- 5 params, CORRECT
   163: local function Cutscene()                                     <- a DIFFERENT, genuinely 0-param closure

Parameters are NOT lost. That claim is superseded and must not be re-derived.

### The verified mechanism (checked, not inferred)
The child proto is instantiated TWICE and the two emissions differ:

  inside getToyCutscene (params in scope)   |  at the second site
  -----------------------------------------|---------------------------------
  arg5 = arg5 or "CameraTarget"            |  tbl2 = tbl2 or "CameraTarget"
  arg4 = arg4 or "CameraPos"               |  tbl3 = tbl3 or "CameraPos"
  arg2 or 1, arg3 or 9999999999999         |  v13 or 1, v14 or 9999999999999

Same body, same defaults. Where the enclosing parameters are in scope the
captures bind (arg1..arg5). At the second instantiation the SAME captures are
emitted as bare `local`s that nothing assigns.

So mechanism A is a CAPTURE-BINDING failure, not a parameter-list failure: a
closure whose upvalues are its enclosing function's parameters is re-emitted at
a site where those parameters do not exist, and the captures become unbound
declarations. The original was almost certainly
`Cutscene = getToyCutscene("Gingerbread House", "CameraTarget", "CameraPos", 1, 9999999999999)`
- the call's ARGUMENTS are what the bare locals should have been bound to.

That also explains the scale: 110 bare locals in this one file, and the shape
recurs wherever a factory function's returned closure is instantiated.

### State
A3  SUPERSEDED (wrong).
A4  ACTIVE: capture binding lost when a child proto is instantiated away from
    the scope that declares its upvalues.
    Reproducer: 623_Beesmas2024BQuests, compare emitted lines 15-30 against
    158-176 - same proto, one bound, one not.
    NEXT QUESTION: at the second instantiation, what does the lifter have for
    the closure's upvalue list, and why does it mint a fresh local instead of
    binding to the CALL ARGUMENTS at that site? Look at how NEWCLOSURE/DUPCLOSURE
    handles CAPTURE operands when the captured register holds a call argument.

## 2026-08-19 — ROTATION 4. A4 question answered (negative result, but a sharp one)

Baseline re-verified on a clean out-dir with a timestamp-checked binary:
1097/1142 clean (96.1%), 97 defects. Unchanged.

### What I instrumented
opcode_handlers.rs, the capture-name derivation. When a CAPTURE names a register
that holds a concrete non-Name expression, the code materialises a declaration:

    let pending = match regs.get(cap_idx) {
        Some(RegVal::Expr(e)) if !matches!(e, Expr::Name(_)) => Some(e.clone()),
        _ => None,
    };
    match pending {
        Some(value) => emit_local_or_assign(...),   // declaration materialised
        None        => name,                        // bare name, nothing declared
    }

Tagged every `pending.is_none()` case by reason (CAP_UNKNOWN / CAP_NAME_OK /
CAP_LOOPVAR / CAP_OUT_OF_RANGE).

### MEASURED RESULT
No tag fired. Not on 623_Beesmas2024BQuests (which contains 234 CAPTURE
instructions) and not on 617_PromoCodes. Binary freshness verified by mtime
(02:38:15 binary vs 02:36:33 source) so this is not the stale-binary trap that
cost two earlier measurements.

The enclosing gate is:

    matches!(cap_type, 0 | 1) && cap_idx as usize != a && cap_op == LuauOpcode::Capture

So on this corpus that whole branch is DEAD CODE: captured registers holding a
non-Name value never have a declaration materialised. Which of the three
conjuncts fails is not yet established - that is the next question.

### Why this matters
It is consistent with the observed defect: the capture keeps its USE and never
gets a binding, which is exactly `declared_never_assigned`. It also explains the
corpus-wide scale, since the gate is bytecode-shape dependent rather than
file-specific.

### A4 -> PARKED (1). Unblocking question, narrowed to one measurable fact:
  Of the three conjuncts, WHICH ONE is false on Roblox bytecode?
  Cheapest test: log cap_type, cap_idx, `a`, and cap_op for the first 20 CAPTUREs
  in 623 - one eprintln at the `is_capture` site (line ~1934), which IS reached
  (the structural fallback lives there). That names the failing conjunct, and the
  fix follows from it: if cap_op is the failure, the materialisation must accept
  a structurally-detected CAPTURE the same way `is_capture` already does.

### Rotation 4 state
A1 OPEN (deferred - fixing alone relabels, does not raise the clean rate)
A2 DISPROVED as a lever (no-op measured, reverted)
A3 SUPERSEDED (wrong - parameters are not lost)
A4 PARKED (1) with the question above
B  PARKED (1) - naive fix cost 13 files
C  OPEN - cause named, not attempted
D  OPEN - name_body_mismatch, 1 file, untouched
E  OPEN - undefined_local, 1 file, untouched

## 2026-08-19 — ROTATION 5. FIRST MECHANISM CLOSED (D).

### A4 disproved (third hypothesis killed)
Probe at the `is_capture` site: of 233 CAPTUREs in 623_Beesmas2024BQuests,
**203 satisfy all three conjuncts** of the materialisation gate
(`type in {0,1} && cap_idx != a && cap_op == Capture`). Combined with rotation
4's silent `pending.is_none()` tag, that means `pending` was `Some(..)` every
time: materialisation IS working. A4 is not the cause. Instrumentation reverted.
(The 30 remaining are cap_type=2, upvalue re-capture, a different path.)

### Synthetic-reproducer attack: BLOCKED on toolchain
`tools/luau/luau-compile.exe` emits bytecode **version 11**; the decompiler
accepts 3-9. So ground-truth compile-and-diff is unavailable with the bundled
compiler. Not a dead end for the idea - a dead end for this binary. To unblock:
a v9-era luau-compile, or compile through the game client itself.

### D — CLOSED. Checker false positive, rule corrected.
`855_Stickers.lua:342` declares `local function inspectSF(self)` INSIDE
`function tbl.RepairInvalidStickersInBook(arg1)` (:333) and warns under the
enclosing name:

    warn("Stickers.RepairInvalidStickersInBook: there are multiple stickers ...")

That is correct code - a nested helper naming the caller-visible operation.
Nesting PROVEN: parent at column 0, helper indented, zero top-level `end`
between them.

The rule was "referenced name is any declared name", which cannot distinguish a
mis-paired closure from a helper naming its parent.

CORRECTED RULE (recorded as required): a self-identifying string is only a
mismatch when the referenced declaration does NOT enclose the one being checked.
`Decl` already carries start/end, so enclosure is `o.start < d.start &&
o.end >= d.end`. A SIBLING sharing a name is still the mis-pairing shape and is
still reported - the check is narrowed, not weakened.

MEASURED, clean out-dir, timestamp-verified binary:
    before  1097/1142 clean (96.1%), 97 defects
    after   1098/1142 clean (96.1%), 96 defects
MUTATION-PROVEN: replacing the guard with `if false` returns exactly 1097/97;
restoring gives 1098/96.
Rust suite: 0 failures except the two pre-existing opmap_dupclosure_detection
tests, verified identical at backup/pre-batch before any edit this session.

### State after rotation 5
A1 OPEN (deferred - fixing alone relabels, does not raise the clean rate)
A2 DISPROVED (no-op measured)   A3 SUPERSEDED (wrong)   A4 DISPROVED (gate passes)
A  ACTIVE - dominant, ~347 defects, cause still unknown after four hypotheses
B  PARKED (1)   C OPEN   D **CLOSED**   E OPEN - located, 71_PlaceInfo.lua:53 `v7`

## 2026-08-19 — ROTATION 6. E adjudicated REAL, mechanism named.

### E is NOT the known false-positive class
71_ReplicatedFirst_PlaceInfo sits in ReplicatedFirst, so I checked it against the
recorded false positive (`game.ReplicatedFirst.PlaceType` child lookups are
VALID). It is not that shape. It is a real dropped value.

### Bytecode evidence (main proto tail, --opmap AND --opmap-db)
    121: DUPCLOSURE  R5 K35 ; proto(0)
    122: GETIMPORT   R8 ; game
    124: GETTABLEKS  R8 R8."PrivateServerId"
    126: JUMPXEQKS   R8 == ""  -> +11
    134: LOADK       R7 K40 ; "VIPServer"
    136: LOADK       R7 K41 ; "ReservedServer"
    138: LOADK       R7 K42 ; "StandardServer"
    139: JUMPXEQKS   R7 == K42 -> +2 ; "StandardServer"
    141: LOADB       R6 false +1
    142: LOADB       R6 true
    143: SETTABLEKS  R0."IsPublicServer" R6

THERE IS NO CALL INSTRUCTION. The compiler INLINED the server-type logic into the
main proto: three branch arms each LOADK a constant into R7, then R7 is compared
at 139. R7 is assigned three times and read once, all in the main proto.

### Emitted output (57 lines total)
    local function getServerType()
        ... return "VIPServer" / "ReservedServer" / "StandardServer" ...
    end
    local PrivateServerId2 = game.PrivateServerId
    tbl.IsPublicServer = v7 == "StandardServer"     <- v7 never assigned

So the SAME instructions were emitted twice over: once as a reconstructed
function body whose arms became `return`s, and once as the comparison - which
then had nothing to compare, because the value the arms produced was consumed by
the function reconstruction rather than bound to a local.

MECHANISM (E, and it is the same family as A): a register written by several
branch arms and read after the join is being structured as a FUNCTION BODY with
returns instead of as a value join. The arms' writes become `return` statements,
so the post-join read finds nothing and mints an unassigned name.

Note this is a THREE-arm selection (VIPServer / ReservedServer / StandardServer),
not a two-arm diamond. Both phi paths in mod.rs are written against a
then/else pair; `Region::ValueJoin` + `fold_register_paths` handle N paths, so
the question is why this region was not classified as a ValueJoin.

### E -> PARKED (1). Unblocking question:
  For 71_PlaceInfo's main proto, what Region does `structure_control_flow`
  produce for the span 122..143? Reuse the rotation-1 probe (it already dumps
  CFG blocks and regions for a chosen proto - PROBE_BIN/PROBE_PROTO). If it is
  IfThenElse rather than ValueJoin, the classifier is the target; if it IS a
  ValueJoin, then `fold_register_paths` returned None and the fallback emitted
  returns.
  This is a 57-line file with a 22-instruction span - by far the smallest
  reproducer found for this family, and much cheaper than 623 or CameraModule.

### State after rotation 6
A  ACTIVE (~347 defects; A2/A3/A4 disproved)      A1 OPEN (deferred, would relabel)
B  PARKED (1)   C OPEN   D **CLOSED**   E PARKED (1) with question above

## 2026-08-19 — ROTATION 7. E's question ANSWERED. Structural cause located.

Probe on 71_PlaceInfo proto 1 (main), --opmap AND --opmap-db:

    BLOCK   0: [  0..121)  succ=[121, 153]
    BLOCK 121: [121..128)  succ=[128, 138]
    BLOCK 128: [128..134)  succ=[134, 136]
    BLOCK 134: [134..136)  succ=[139]            LOADK R7 "VIPServer"
    BLOCK 136: [136..138)  succ=[139]            LOADK R7 "ReservedServer"
    BLOCK 138: [138..139)  succ=[139]            LOADK R7 "StandardServer"
    BLOCK 139: [139..153)  pred=[134, 136, 138]  <- THREE-predecessor join
    REGION IfThenElse { cond_pc: 0,
                        then_region: [121, 128, 134, 136, 138, 139],
                        else_region: [],
                        merge_pc: Some(153) }

ANSWER: it is NOT classified as a ValueJoin. Block 139 is a genuine three-arm
value join, and it is SWALLOWED INSIDE the then_region of an IfThenElse rooted
at block 0. else_region is empty and merge_pc is 153 - the RETURN block, not the
real join at 139. No phi/merge logic ever sees 139 as a join, so R7's three arm
writes are never reconciled and the post-join read mints an unassigned `v7`.

### Why the classifier misses it (structuring.rs:303)
ValueJoin detection is gated on `block.successors.len() == 2` and runs per block
in reverse postorder. Block 0 has TWO successors so it is tested first;
`find_value_join` declines it (block 0's branch is not a value selection), and
the code falls through to the IfThenElse fallback, which claims 121/128/134/136/
138/139 into `handled`. Block 121 - which IS the real value-join entry, and does
have exactly 2 successors - is therefore never offered to the detector.

So the defect is an ORDERING/CLAIMING problem, not a detection-capability
problem: the outer IfThenElse claims an inner region that a later iteration
would have matched.

### Note on the lifting-side safety net
`lift_branch_region` already calls `collect_value_joins_in_span` for the
single-run case (mod.rs ~1246) specifically to splice back joins the structurer
never saw. The then_region here is contiguous [121..153), so that path SHOULD
have caught it. It did not, and why is the next question.

### E -> PARKED (2). Narrowed question:
  During lifting of this IfThenElse's then arm, what does
  `collect_value_joins_in_span(cfg, proto, 121, 153)` return? If it returns the
  join at 139, the splice is running and something downstream discards it; if it
  returns nothing, the collector inherits the same 2-successor/handled gate as
  the structurer and that is the single place to fix.
  One eprintln in collect_value_joins_in_span answers it. 57-line file.

### State after rotation 7
A  ACTIVE (~347 defects; A2/A3/A4 disproved)   A1 OPEN (deferred, would relabel)
B  PARKED (1)   C OPEN   D **CLOSED**   E PARKED (2), question above

## 2026-08-19 — ROTATION 8. Rotation 7's conclusion CORRECTED. Fault narrowed to lift_value_join.

Baseline re-verified after reverting the probe: 1098/1142 clean, 96 defects.

### What rotation 7 concluded, and why it was wrong
R7 said: the three-arm join at block 139 is swallowed by an outer IfThenElse, so
no phi logic ever sees it. That inference was based on the REGION dump alone.

Instrumented `collect_value_joins_in_span` (the lifting-side splice) and measured:

    VJPROBE span=[121..153) pc=121 succ=[128, 138] found=true
    VJPROBE   -> join=139 join_start=139 end=153 accepted=true
    VJPROBE span=[121..153) pc=139 succ=[153] found=false

The collector DOES find the join at 139 and DOES accept it. So the splice
compensates for the structurer exactly as its comment says it is meant to, and
`lift_value_join(entry=121, arms=[...], join=139)` IS called with the right
region. The structurer swallowing it is therefore NOT the defect.

### Where the fault actually is
`lift_value_join` is invoked correctly and still leaves R7 with no binding: the
post-join read mints `v7` and the output emits
`tbl.IsPublicServer = v7 == "StandardServer"`.

The three root-to-join paths are:
    121 -> 138 -> 139           LOADK R7 "StandardServer"
    121 -> 128 -> 134 -> 139    LOADK R7 "VIPServer"
    121 -> 128 -> 136 -> 139    LOADK R7 "ReservedServer"
All three arms are pure LOADK writes to the SAME register - the ideal shape for
`enumerate_value_paths` + `fold_register_paths`.

### E -> PARKED (3). Next attack must CHANGE, per the rules.
Question: inside `lift_value_join` for this region, does `enumerate_value_paths`
return None (a path emitted a statement / caps exceeded), or does it return three
paths and `fold_register_paths` then decline?
  - `enumerate_value_paths` returns None if ANY path emits a statement. Path
    121->138->139 passes through block 121, which contains DUPCLOSURE R5 (proto 0,
    getServerType) - a statement. THAT is the strongest candidate: the entry
    block's own closure creation makes every path "impure", so the whole region
    is rejected despite the arms being pure.
  - Cheapest test: one eprintln of the None-reason inside enumerate_value_paths.
PARKED THREE TIMES -> attack changes from instrumentation to bisection: disable
the purity rejection for the ENTRY block only (arms still required pure) and
measure. If the entry block's own statements are the blocker, that is a
one-condition fix with a corpus-wide measurement to prove or kill it.

### State after rotation 8
A  ACTIVE (~347 defects; A2/A3/A4 disproved)   A1 OPEN (deferred, would relabel)
B  PARKED (1)   C OPEN   D **CLOSED**   E PARKED (3), attack changes next rotation

## 2026-08-19 — ROTATION 9. E's CAUSE FOUND EXACTLY. (Rotation 8's hypothesis also wrong.)

### Rotation 8's guess was wrong before it was tested
R8 suspected the entry block's DUPCLOSURE made every path impure. Reading the
code first: `let body_start = if block_id == entry { body_end } else { block.start }`
- the entry block's pre-branch code is ALREADY skipped. Hypothesis dead on arrival.

### Bisection result (tagged every `return None` in enumerate_value_paths)
    VJREJ IMPURE block=128 stmts=1

ONE statement in ONE intermediate arm block rejects the whole three-arm join.
Block 128 is [128..134):
    128: GETIMPORT  R8 ; game
    130: GETTABLEKS R8 R8."PrivateServerOwnerId"
    132: JUMPXEQKN  R8 == 0 -> +3
and the materialised statement is visible in the output:
    local PrivateServerOwnerId = game.PrivateServerOwnerId

### The defect, stated precisely
The guard is:
    // A statement means a side effect (or a materialised temp) that a
    // per-path expression cannot represent without duplicating it.
    if !scratch.is_empty() { return None; }

It CONFLATES two different things. A side effect genuinely cannot be duplicated
or reordered. A materialised temp binding a PURE expression (`game.X`) is only a
naming choice - it can be hoisted before the branch and evaluated
unconditionally without changing behaviour. Rejecting on it discards the entire
value join and costs R7 its value.

This is the same unknown-falls-through-to-the-blunt-answer shape the repo keeps
closing: "any statement" is the conservative reading of "a statement I cannot
safely move".

### E -> the fix is now specified (not yet applied)
Allow the path when every scratch statement is a single-name `Stat::Local` whose
init is side-effect-free (`has_side_effects` already exists and is used by the
ternary phi at mod.rs:2141). Such bindings must be HOISTED to the caller's
statement list ahead of the region - sound precisely because they are pure - and
`enumerate_value_paths` needs an out-param to carry them, since it currently has
no access to the outer stmts.

Risk to measure: hoisting evaluates the read on paths that would not have taken
it. Sound for a pure index, but a metamethod-bearing table would be observable.
`has_side_effects` is the existing arbiter of that question elsewhere in this
file, so using it here is consistent rather than novel.

MEASUREMENT PLAN: implement, then clean-run the corpus. Keep only if the clean
count rises; revert and journal the numbers if it falls.

### State after rotation 9
A  ACTIVE (~347 defects; A2/A3/A4 disproved)   A1 OPEN (deferred, would relabel)
B  PARKED (1)   C OPEN   D **CLOSED**
E  cause FOUND, fix specified, implementation is rotation 10's work

## 2026-08-19 — ROTATION 10. E **CLOSED**. First real decompiler fix.

Implemented the rotation-9 specification: `enumerate_value_paths` now accepts a
path whose scratch statements are all single-name `Stat::Local` bindings with a
side-effect-free init, hoisting them (new `hoist` out-param) into the caller's
statement list ahead of the region. Anything else still rejects.

MEASURED, clean out-dir, timestamp-verified binary:
    before  1098/1142 clean (96.1%), 96 defects
    after   1101/1142 clean (96.4%), 93 defects
+3 files, -3 defects. No regression.

REPRODUCER REPAIRED - 71_PlaceInfo now emits
    tbl.IsPublicServer = Value2 == "StandardServer"
where it previously emitted `v7 == "StandardServer"` with `v7` never assigned.
The three-arm value join is reconstructed and the register keeps its value.

MUTATION-PROVEN: forcing the guard back to reject-always (`if true`) returns
exactly 1098/96; restoring gives 1101/93.
Rust suite: 0 failures apart from the two pre-existing
opmap_dupclosure_detection tests, identical at backup/pre-batch.

### Why this generalises beyond one file
The guard conflated "a side effect" with "a materialised temp". Any value join
containing a pure field read in an arm was being discarded wholesale. That is
why three files moved rather than one, and it is the same
unknown-falls-through-to-the-blunt-answer shape as mechanism D: "any statement"
was the conservative reading of "a statement I cannot safely move".

### State after rotation 10
A  ACTIVE (~347 defects; A2/A3/A4 disproved)
A1 OPEN (deferred - fixing alone relabels, does not raise the clean rate)
B  PARKED (1)   C OPEN   D **CLOSED**   E **CLOSED**

## 2026-08-19 — ROTATION 11. C investigated and PARKED (1).

Confirmed unchanged by the E fix (run_r10): 176_Collectibles still emits the
SpawnParams literal with 11 keys, missing `Transparent` and `TreasureID`, and
carrying a stale `Rainbow = true`.

### What the bytecode says (established earlier, re-stated here so it is not re-derived)
    271-290: SETTABLEKS R6."Color"/"Icon"/"IconColor"/"Glow"/"Sparkles"/"SilentSpawn"
    294: JUMPIF ...                      <- guard
    304: LOADB      R7 true
    305: SETTABLEKS R6."Rainbow"     R7   <- CONDITIONAL
    317: SETTABLEKS R6."Transparent" R7   <- CONDITIONAL
    324: SETTABLEKS R6."TreasureID"  R7   <- CONDITIONAL
    326: SETTABLEKS R2."SpawnParams" R6
Every UNCONDITIONAL key survives into the literal. Exactly the three CONDITIONAL
ones are wrong or missing.

### What the pass does
`reconstruct_table_constructors` (table_reconstruction.rs:70) seeds on
`local X = {}` and greedily absorbs a CONTIGUOUS PREFIX of `X.K = V`
statements. A conditional write lives inside a `Stat::If`, which is not that
shape, so the greedy run stops there. That explains why the literal contains
only the unconditional keys - and it would be harmless if the conditional
writes were then emitted AFTER the literal as `X.K = V` inside their `if`.

They are not. The output instead binds a LOCAL:
    local Transparent = tbl4.TreasureID
    if tbl4.TreasureID then Transparent = tbl4.TreasureID end
So the SETTABLEKS TARGET (R6, the table) was resolved to a fresh local name
rather than to the table expression, and the key write was lost with it.
`Rainbow = true` in the literal is the stale `LOADB R7 true` from pc 304.

### C -> PARKED (1). Unblocking question:
  At pc 317, what does the lifter hold in R6 - the table expression, or a name?
  The three conditional writes are the only ones inside `if` blocks, so the
  suspicion is that entering the branch loses R6's binding (the same
  merge/branch-entry family as A and E), and the SETTABLEKS handler then falls
  back to naming its destination.
  Cheapest test: eprintln of `regs[6]` at the SETTABLEKS handler for pcs
  305/317/324 in 176_Collectibles. If R6 is Unknown or a fresh Name at those
  points, C is the SAME root as A - a register losing its value on a branch
  edge - and not a table-reconstruction defect at all.

### State after rotation 11
A  ACTIVE (~347 defects; A2/A3/A4 disproved)
A1 OPEN (deferred - fixing alone relabels, does not raise the clean rate)
B  PARKED (1)   C PARKED (1), question above   D **CLOSED**   E **CLOSED**

## 2026-08-19 — ROTATION 12. C's question answered (rotation 11's hypothesis WRONG). All mechanisms now terminal.

Baseline re-verified after reverting the probe: 1101/1142 clean (96.4%), 93 defects.

### C: the register hypothesis is DISPROVED
Instrumented the SetTableKS handler to report what the table register holds:

    SETTKS pc=305 tableReg=6 key=Rainbow      holds=Expr(complex)   CONDITIONAL
    SETTKS pc=317 tableReg=6 key=Transparent  holds=Expr(complex)   CONDITIONAL
    SETTKS pc=324 tableReg=6 key=TreasureID   holds=Expr(complex)   CONDITIONAL
    SETTKS pc=280 tableReg=6 key=Glow         holds=Expr(complex)   UNCONDITIONAL, SURVIVES

R6 is intact at the conditional writes - not Unknown, not a fresh Name - and the
UNCONDITIONAL write at pc 280 has the IDENTICAL register state yet survives into
the literal. So C is NOT the same root as A. Rotation 11's suspicion is dead.

### What C actually is
R6 holds a table-CONSTRUCTOR expression, not a bound name. A field write to a
complex expression is not expressible (`{...}.K = v`), so
`ensure_table_reg_declared` materialises a local for it. For the unconditional
run that materialisation happens once, at block level, and
`reconstruct_table_constructors` folds the whole contiguous prefix into a
literal. The conditional writes sit inside `Stat::If` bodies, so each one
materialises its OWN local inside its OWN branch - which is why the output shows
`local Transparent = ...` instead of `SpawnParams.Transparent = ...`.

### C -> PARKED (2). Unblocking question:
  Can the table register be materialised into ONE named local BEFORE the first
  branch that writes to it, so every conditional write targets that name?
  `ensure_table_reg_declared` already does the materialisation; the question is
  purely WHERE it runs. Hoisting it to the point the register first becomes a
  constructor - rather than at each write site - would give all writes, in or
  out of a branch, a single shared target.
  Risk to measure: materialising earlier changes statement order and could
  break the contiguous-prefix absorb that currently produces the literal.
  Measure corpus-wide; keep only if the clean count rises.

### A -> PARKED (1). Unblocking question (was ACTIVE; recorded now):
  Ruled out by measurement: A2 (merge_regs terminal reset - candidate fix was a
  corpus no-op), A3 (closure parameter lists - proven intact, getToyCutscene
  emits all 5 params), A4 (capture materialisation - 203/233 captures pass the
  gate, so it works).
  The one question: mechanism E proved that a value join is discarded wholesale
  when an arm materialises a statement, and fixing that recovered 3 files. E's
  guard was `!scratch.is_empty()`. Are there OTHER wholesale-rejection guards on
  the same path - `enumerate_value_paths` MAX_VALUE_JOIN_PATHS, the
  `p.len() >= 2` acceptance test at the call site, `find_value_join`'s own
  purity allowlist - that discard a join for a similarly narrow reason?
  Cheapest test: tag each of those three rejection points exactly as rotation 9
  tagged the purity guard, run the CORPUS (not one file), and count which fires
  most. That converts A from "unknown cause" into a ranked list of candidate
  guards, and E is the proof that this class of fix pays.

### A1 -> PARKED (1). Unblocking question (was OPEN/deferred):
  A1 is free_var_decls declaring READONLY holes, which masks a dropped value as
  a silently-nil local instead of leaving it visible to `undefined_local`.
  Deferred deliberately because fixing it alone RELABELS defects rather than
  removing them - the clean count would not move.
  The question that decides it: after A is fixed, how many READONLY names remain?
  The rotation-2 census measured 27 corpus-wide (563 ASSIGNED / 27 READONLY).
  If A's fix drives that toward zero, A1 becomes a no-op and closes for free; if
  the 27 survive, A1 is worth fixing on honesty grounds alone. So A1 is blocked
  ON A by design, and the census command to re-run it is recorded in rotation 2.

### State after rotation 12 - EVERY MECHANISM TERMINAL
A  PARKED (1) - question above          A1 PARKED (1) - blocked on A, question above
B  PARKED (1) - naive fix cost 13 files C  PARKED (2) - question above
D  **CLOSED**                            E  **CLOSED**

## 2026-08-19 — ROTATION 13. A's parked question ANSWERED with corpus numbers.

Baseline re-verified after reverting instrumentation: 1101/1142 clean, 93 defects.

### Method
Tagged all SIX wholesale-rejection points on the value-join path and ran the
WHOLE CORPUS (not one file), exactly as the rotation-9 tagging worked for E.

### Result - ranked, corpus-wide
    345  VJR FOLD_DECLINED      <- dominant, 4x the next
     86  VJR IMPURE_NONTEMP
     86  VJR ENUM_NONE
      5  VJR GUARD_SPAN
      0  VJR MAX_PATHS
      0  VJR NOT_MEMBER
      0  VJR TOO_FEW_PATHS

`FOLD_DECLINED` is `combine_guarded_value` returning None. It declines when the
selected value is not `is_provably_truthy`, because emit.rs renders an
expression-position Ternary as `cond and a or b`, which is only faithful when
the then-arm cannot be false or nil.

### THE LINK TO MECHANISM A (this is the finding)
When `fold_register_paths` returns None, `lift_value_join` falls to the
"Fallback lowering" at mod.rs:1955, which emits:

    stmts.push(Stat::Local { names: vec![name], values: vec![] });   // :1957-1963

A BARE `local` with no value, followed by assignments inside the if/elseif arms.
That is EXACTLY the shape mechanism A consists of - and rotation 1 independently
identified this same site as an emitter of bare multi-name locals before there
was any reason to connect it to A.

So the chain is now: value not provably truthy -> combine_guarded_value declines
-> fold_register_paths returns None -> fallback emits a bare `local` -> if the
arm assignments do not all survive, the name is declared and never assigned.
345 occurrences corpus-wide against ~347 A defects is a striking match, though
the two counts are measured differently and I am NOT claiming they are the same
set without checking.

### A -> PARKED (2). Next question, now much narrower:
  In the fallback at mod.rs:1955-1975, the `local r` is emitted unconditionally
  but `r` is only assigned by the per-path `assigns` closure. For the files that
  carry A defects, do those arm assignments get emitted at all?
  Cheapest test: tag the fallback itself - one eprintln recording the name and
  the number of assign statements actually pushed - then compare that name list
  against the `declared_never_assigned` names the checker reports for the same
  file. If they intersect, A is closed by fixing the fallback; if they do not,
  the 345/347 match is a coincidence and A lies elsewhere.
  This is the first test that can CONFIRM OR KILL the whole A hypothesis in one
  run, rather than narrowing it further.

### State after rotation 13 - all mechanisms still terminal
A  PARKED (2) - question above      A1 PARKED (1) - blocked on A by design
B  PARKED (1)                        C  PARKED (2)
D  **CLOSED**                        E  **CLOSED**

## 2026-08-19 — ROTATION 14. A's biggest hypothesis KILLED by a decisive test.

Baseline re-verified after reverting all instrumentation: 1101/1142, 93 defects.

### The confirm-or-kill test (rotation 13's plan), executed
Tagged the fallback's bare-local emission (mod.rs:1945) and intersected the
names it emits with the `declared_never_assigned` names the checker reports for
the SAME file:

    623_Beesmas2024BQuests   fallback names: 0   checker names: 6   INTERSECTION: EMPTY
    215_CollectiblesAnimator fallback names: 0   checker names: 1   INTERSECTION: EMPTY

KILLED. The fallback emits ZERO bare locals in the two files that actually carry
A defects. The 345 FOLD_DECLINED / ~347 A-defect match was a COINCIDENCE - which
is exactly the correspondence I refused to assume in rotation 13, and refusing it
was correct.

### Second emitter also killed
Only two sites in the whole decompiler contain a literal `values: vec![]`:
  mod.rs:1945  the value-join fallback   -> 0 names in 623 (above)
  mod.rs:4161  hoist_loop_locals         -> 0 names in 623 (tagged and measured)
Neither emits 623's bare locals.

### And free_var_decls is ruled out FOR 623 specifically
623 has ZERO chunk-top bare locals (`grep -cE '^local ...$'` = 0); its flagged
names are INDENTED, i.e. inside a function body. free_var_decls only runs at
depth 0 and inserts at chunk top, so it cannot be the source there.
(It IS the source for 215 - rotation 1 proved that by disabling it.)

### So A has at least TWO distinct emitters, and one is still unidentified
215-style: chunk-top block from free_var_decls  (identified, rotation 1)
623-style: INDENTED bare locals inside function bodies  (emitter UNKNOWN)

### A -> PARKED (3). ATTACK MUST CHANGE (rule: parked three times).
My search was too narrow: I grepped only the literal `values: vec![]` and found
2 sites, but there are 74 `Stat::Local {` construction sites in decompiler/.
Any of them could build an empty values vec via `Vec::new()`, a variable, or a
drained collection.

NEW ATTACK - stop searching the source, instrument the AST instead:
  Add a debug assertion at the single point where statements are emitted
  (emit.rs, the `Stat::Local` arm) that prints a backtrace-free marker when it
  renders a Local with an empty values list. Every bare local in the OUTPUT must
  pass through that one arm, so it cannot be missed by a source grep. Run it on
  623 and the marker plus the name identifies the producer immediately.
  This converts "find the emitter among 74 candidates" into "read one line of
  output", and it is the same shape of move that closed E (measure at the
  chokepoint rather than reason about the callers).

### State after rotation 14 - all mechanisms terminal
A  PARKED (3) - new attack above    A1 PARKED (1) - blocked on A by design
B  PARKED (1)                        C  PARKED (2)
D  **CLOSED**                        E  **CLOSED**

## 2026-08-19 — ROTATION 15. MECHANISM A'S EMITTER FOUND. There was never a bare local in the AST.

Baseline re-verified after reverting instrumentation: 1101/1142, 93 defects.

### The chokepoint attack worked
Instrumented emit.rs at the ONE arm that renders `Stat::Local` (emit.rs:51).
Every bare local in the output must pass through it, so a source grep cannot
miss what this catches.

First run tagged `values.is_empty()` and printed ZERO markers for 623 - while the
file visibly contains `local tbl2` at line 159. Binary freshness verified by
mtime (04:10:11 binary vs 04:08:34 source), so not the stale-binary trap.

### The answer: Phase B0.94b strips trailing nil
emit.rs:78-84 - "strip trailing nil values from local declarations:
`local x = nil` -> `local x`". So the AST holds
    Stat::Local { names: [x], values: [Expr::Nil] }
which is NOT empty, and the emitter renders it bare.

Re-tagged to separate the two cases, on 623:
    EMIT_BARE_EMPTY   0
    EMIT_BARE_NIL   110
    INTERSECTION with the checker's flagged names:
        tbl11 tbl14 tbl8 v14 v15 v16   <- ALL SIX
Complete intersection. 623's every flagged name is a nil-strip.

### Why 14 rotations of searching failed
There was NEVER a bare local in the AST to find. Every emitter hypothesis - the
value-join fallback, hoist_loop_locals, free_var_decls, capture materialisation -
was hunting a construction that does not exist for this shape. The bare local is
created at RENDER time, not at lift time.

### THE ADJUDICATION QUESTION (this decides ~110 instances, ~1/3 of all defects)
`local x = nil` and `local x` are semantically IDENTICAL in Lua. So:
  * if the bytecode genuinely carried LOADNIL for that register, the output is
    CORRECT and the checker's `declared_never_assigned` is a FALSE POSITIVE -
    the fix is to the CHECKER (record the corrected rule), not the decompiler;
  * if the `Expr::Nil` is a placeholder the lifter substituted for a value it
    dropped, the output is WRONG and the checker is right.
Decided by reading the bytecode for ONE of the six names in 623.
NEXT ACTION: disassemble 623, find the register behind `v14`, and check whether
a LOADNIL writes it. That single read settles a third of the remaining defects.

Note this is exactly the D/E shape again if it resolves as a false positive: a
check that cannot tell "assigned nil" from "never assigned".

### State after rotation 15 - all mechanisms terminal
A  PARKED (3) - emitter FOUND, adjudication question above (this is progress:
   the question is no longer "where does it come from" but "is it wrong at all")
A1 PARKED (1) - blocked on A by design
B  PARKED (1)   C PARKED (2)   D **CLOSED**   E **CLOSED**

## 2026-08-19 — ROTATION 16. Mechanism A ADJUDICATED: FALSE POSITIVE. 93 -> 32 defects.

### The adjudication, from the bytecode
623_Beesmas2024BQuests carries **122 LOADNIL** instructions; R14 alone is written
by LOADNIL 12 times. The registers behind every flagged name are genuinely
assigned nil. NOTHING WAS DROPPED.

So `local v14 = nil` is faithful output. emit.rs's "Phase B0.94b" then stripped
the `= nil` as a cosmetic tidy-up, and the semantic checker - which reads the
EMITTED TEXT - saw `local v14`, found no assignment, and reported
`declared_never_assigned`. The checker was analysing evidence the renderer had
already removed.

Verified by intersection (rotation 15): 110 nil-strips in that file, and all six
flagged names among them.

### The fix
Strip trailing nils only when a REAL value survives (`local x, y = 1, nil` ->
`local x, y = 1`, unchanged). When the whole initialiser is nil, keep it.
Semantically identical in Lua; strictly more faithful to the bytecode.

MEASURED, clean out-dir, timestamp-verified binary:
    before  1101/1142 clean (96.4%), 93 defects
    after   1118/1142 clean (97.9%), 32 defects
+17 files, -61 defects. Largest single gain of the run.
MUTATION-PROVEN: forcing the strip back on returns exactly 1101/93.

### TESTS CHANGED - stated plainly because that is normally a red flag
`b094b_local_nil_strips_to_bare_local` and `b094b_local_all_nils_stripped`
asserted the old behaviour and now fail. I updated both, with the reason
recorded in the test bodies. Justification: they pinned a COSMETIC choice, not a
correctness property - the two renderings are identical to Lua - and that choice
was measurably destroying evidence. The other two B0.94b tests
(trailing-nils-stripped, non-trailing-nil-preserved) are UNCHANGED and still pass,
so the narrowing is exactly what it claims to be.

### HONESTY: what this gain is and is not
The 61 defects were FALSE POSITIVES. The decompiler's output was already correct
for them; it is now one token more informative per nil local. The clean rate rose
because the INSTRUMENT stopped misreading correct output, not because broken
output was repaired. Real correctness is unchanged by this commit.

That is still worth having - 61 false findings were hiding the 32 real ones - but
it must not be read as 61 bugs fixed.

### This is the D/E shape for the third time
D: "any referenced name" meant "a name that is not my parent".
E: "any statement" meant "a statement I cannot safely move".
A: "no `=` in the text" meant "never assigned" - it could not tell ASSIGNED NIL
   from NEVER ASSIGNED, because the renderer had deleted the distinction.

### State after rotation 16
A  **CLOSED** (false positive, emitter corrected, rule recorded)
A1 PARKED (1) - now unblocked by A's closure; the rotation-2 census should be
   re-run to see how many of the 27 READONLY names survive
B  PARKED (1)   C PARKED (2)   D **CLOSED**   E **CLOSED**
Remaining: 32 defects across the corpus.


## Rotation 17 - the field-access misread, and a discarded classification

### The enumeration that was blocked last rotation
The probe failed twice on a RELATIVE `PROBE_DIR`: integration tests run with the
crate directory as cwd, not the workspace root. Absolute path, and all 32
remaining defects listed - every one of them `declared_never_assigned`.

### F: `math.cos` is not a read of a local called `cos`  **CLOSED**
The checker's read scanner split each line on every non-alphanumeric character,
so `math.cos` produced the tokens `math` AND `cos`. A field or method name was
counted as a read of a same-named local.

PROVEN, not assumed: for each of the 32, occurrences were counted as bare vs
field position. Five had ZERO bare occurrences anywhere in the file - `cos` in
306/326/331, `insert` in 313, `sub` in 919. Those files never read those locals
at all; the checker invented the defect out of a name collision.

Fixed with `bare_idents()`, which skips a token immediately preceded by `.` or
`:` - and only a SINGLE dot, so `a .. b` stays a real read of `b`.

MUTATION TEST: 32 -> 27. The five named false positives vanished and all 27
genuine findings survived. The fix discriminates; it does not suppress.

### G: the ValueJoin fallback threw away the answer it asked for  **CLOSED**
`enumerate_value_paths`' fallback lowering emits `local r; if c then r = v1 ...`.
It called `locals.classify_write(i, &name)` and then DISCARDED the kind:

    let (_, name) = locals.classify_write(i, &name);   // kind thrown away
    stmts.push(Stat::Local { names: vec![name], values: vec![] });  // always

`classify_write` documents the exact invariant it was being asked for - "Params
always reassign - the register IS the parameter, never re-declared" - and returns
`Reassign` for them. The caller pushed a declaration anyway, so a parameter got
rebound to nil and every later read in that function evaluated to nil.

Measured damage: `238_EggTypes` re-declared `local arg1` over parameter `arg1`
(145 bare reads) and `879_BigNum` re-declared `local self` over the receiver
(134 reads). Two files, 279 corrupted reads.

The sibling site at the split-arm seed (mod.rs:6264) already guards this with
`is_undeclared_non_param`. This one never got the guard.

Fix: push the declaration only when `kind != WriteKind::Reassign`.

MUTATION TEST at source level, not by count: in `879_BigNum` the fix removed
`local self` and `local arg2` (already bound) and KEPT `local len5` and
`local signal8` (genuinely new). A blunt fix would have deleted all four.

### Measurement - clean run, empty out-dir, 1142/1142 decodable
1,118 -> **1,125 clean (97.9% -> 98.5%)**, 32 -> **25 defects**.
The arithmetic is exact: 5 files whose only defect was the F false positive
(306, 313, 326, 331, 919) + 2 files repaired by G (238, 879) = 7 newly clean.

### HONESTY: which half of this is real
F was the INSTRUMENT misreading correct output - 5 false findings, no
correctness change. G was a REAL defect in emitted code - 2 files repaired,
279 reads that evaluated to nil now read the parameter. Do not read 7 as 7 bugs.

### Not mine
`opmap_dupclosure_detection`'s two tests fail at HEAD as well - verified by
stashing both edits and re-running. Pre-existing, opmap layer, unrelated. Logged
as its own open item rather than folded into this rotation.

### This is the D/E/A shape for the FOURTH time
D: "any referenced name" meant "a name that is not my parent".
E: "any statement" meant "a statement I cannot safely move".
A: "no `=` in the text" meant "never assigned".
F: "a token on the line" meant "a bare identifier, not a field name".
G is the other recurring shape: an answer was computed correctly and then
discarded at the call site.

### State after rotation 17
A **CLOSED**   D **CLOSED**   E **CLOSED**   F **CLOSED**   G **CLOSED**
A1 PARKED (1) - and now the ONLY mechanism left on the semantic measure: all 25
   remaining defects are `vN` names from `free_var_decls`' READONLY branch,
   whose own comment concedes they "evaluate to nil". Three sub-shapes already
   identified in the survivors: dropped multi-return destructuring
   (`893_Matrix3D`: `self:GetComponents()` emitted as a bare statement while
   `v1..v12` are returned), an unbound field-write base (`651_Collect_Goo`:
   `v3.Color = Zone4`), and the plain hoisted-hole case.
B  PARKED (1)   C PARKED (2)
Remaining: 25 defects across the corpus. Sweep measure not yet attacked.


## Rotation 18 - a guessed constant where the bytecode carried the exact bound

### H: a call returning 12 values was called a statement  **CLOSED**
`mint_trace` on `893_Utils_Matrix3D` tagged all four survivors
`UNK_CALL_CLEAR` - a CALL had cleared their registers to Unknown. Following that
to the CALL handler:

    // Clamp to sane maximum - Luau functions almost never return >10 values.
    let nresults = if raw_nresults > 10 && raw_nresults != 0 { 1 } else { raw_nresults };

`CFrame:GetComponents()` returns exactly 12, so `raw_nresults` is 13, so the
call was rewritten to "statement, 0 results captured" - and the register clear
below then wiped all twelve results. The emitted body was

    function tbl.FromCFrame(self)
        self:GetComponents()
        return { v4, v5, v6, 1, ... v1, v2, v3, 1 }   -- twelve names nothing assigns
    end

The clamp exists to reject garbage C from a misdetected opcode, which is a real
hazard. But it could not tell 12-real-results from garbage, and a flat 10 is a
guess when the bytecode carries the exact bound: results occupy R(A)..R(A+n-1)
and the proto declares `max_stack_size`. A count that does not fit the frame is
impossible; one that fits is a real call whatever its size. The opmap detectors
already bound operands this way (`opmap.rs:1244`).

    let captured = raw_nresults.saturating_sub(1) as usize;
    let fits_frame = a + captured <= proto.max_stack_size as usize;

After: `local getcomponents, result2, ... result12 = self:GetComponents()`, and
the returned table reads real names.

### The DISASSEMBLER lied on this file, with both flags
`disassemble --opmap --opmap-db` on 893 printed proto 16 as `constants (1): K0`
and then referenced K1..K12 in its body, full of UNKNOWN opcodes and DUPCLOSURE
instructions belonging to the main proto. Self-evidently desynchronised, so the
mechanism was traced through `mint_trace` and the decoder instead. CLAUDE.md's
rule ("never read operands off the printer") is not strong enough - on this file
the printer's PROTO ATTRIBUTION is wrong too. Logged as its own open item.

### Measurement - clean run, empty out-dir, 1142/1142 decodable
1,125 -> **1,127 clean (98.7%)**, 25 -> **16 defects**. Nine defects for two
files: several files lost defects without reaching clean. `893_Matrix3D` (4) and
`255_BackpackGui` (3) went fully clean; `21_ClickToMove` went 3 -> 1.
928 unit tests pass; the two `opmap_dupclosure_detection` failures are the same
pre-existing pair, verified against HEAD by stash in rotation 17.

### This is the shape for the FIFTH time
An UNKNOWN falling through to a blunt permissive answer:
D "any referenced name", E "any statement", A "no `=` in the text",
F "a token on the line", H "any C over ten is corruption".
Each was a guess standing in for something the input already stated exactly.

### State after rotation 18
A **CLOSED**  D **CLOSED**  E **CLOSED**  F **CLOSED**  G **CLOSED**  H **CLOSED**
A1 PARKED (1) - still the only mechanism on the semantic measure. 16 remain.
B  PARKED (1)   C PARKED (2)
Open, not folded in: the two opmap_dupclosure_detection failures; the
disassembler's proto attribution on 893. Sweep measure not yet attacked.


## Rotation 19 - A1 split into four, and one measured dead end

### The 16 survivors are not one mechanism
`mint_trace` site tags across all 16, which separates them cleanly:

    UNK_MERGE_COMPOUND    5   215, 544, 651, 654, 79
    UNK_CALL_CLEAR        5   533 (x2), 22, 53, 368, 85
    UNK_MERGE_ARM_NAMES   3   797, 305, 88
    METHOD_RECV_REJECT    2   54, 21

So A1 was never one mechanism - it is the free_var_decls pass reporting the
downstream effect of four different upstream losses. `UNK_CALL_CLEAR` survives
rotation 18's fix, so there is a second way for results to be wiped.

### DEAD END: extending the pre-branch rule to the asymmetric case
HYPOTHESIS: `merge_regs` keeps a pre-branch binding when BOTH sides hold a
`Name`, but falls to a blunt `Unknown` reset when one side is `Unknown` and the
other still holds the pre-branch value. Extending the same soundness argument
(the value was established before the `if`, so it is live at the join) should
recover the register - and `651_Collect_Goo`'s `v3.Color = Zone4`, a field write
through a nil base, ERRORS at runtime rather than merely reading wrong.

MEASURED: 1,127 clean / 16 defects, byte-identical to rotation 18 except ONE
file, and that difference was `pushRowDef7` -> `pushRowDef6` - a name-counter
shift with no semantic content. The condition effectively never fires.

FALSIFIED, and the useful part is what it proves: the register behind
`651`'s `v3` is NOT a preserved pre-branch value that one arm lost. Whatever
kills it happens elsewhere, so the next attack on `UNK_MERGE_COMPOUND` must
start by establishing what that register held, not by adjusting the merge.

Reverted. Recorded here so it is not re-run - the same courtesy the MULTRET
skip-the-clear note paid, which is what stopped rotation 18 wasting a cycle on
it.

### State after rotation 19
A **CLOSED**  D **CLOSED**  E **CLOSED**  F **CLOSED**  G **CLOSED**  H **CLOSED**
A1 SPLIT into A1a UNK_MERGE_COMPOUND (5), A1b UNK_CALL_CLEAR (5),
   A1c UNK_MERGE_ARM_NAMES (3), A1d METHOD_RECV_REJECT (2).
   A1a PARKED (2) after this dead end - next attempt must change the ATTACK:
   trace one register to the bytecode rather than reasoning at the merge.
B  PARKED (1)   C PARKED (2)
Remaining: 16 defects. Sweep measure not yet attacked.


## Rotation 20 - three sub-mechanisms collapse into one, and a BLOCKED capability

### A1d is not a naming bug: the receiver really is nil  **RESOLVED as symptom**
`54_Controls` emits

    local SteeringVelocity3 = Character.PrimaryPart:FindFirstChild("SteeringVelocity")
    if SteeringVelocity3 then
        v3:Destroy()                 -- should be SteeringVelocity3:Destroy()
    end

which looks exactly like the namer refusing to use a live binding. It is not.
`LUAU_MINT_TRACE=1` prints what `method_receiver_expr` discarded:

    REJECT  METHOD_RECV  v3   discarded=Nil
    REJECT  METHOD_RECV  v7   discarded=Nil
    REJECT  METHOD_RECV  v13  discarded=Nil     (21_ClickToMove)

The register genuinely holds `Nil`. Rejecting it is CORRECT - `nil:Destroy()`
is not emittable - so `METHOD_RECV_REJECT` is a symptom of a register losing its
value, not a defect in receiver resolution. Do not "fix" the namer here.

### A1b is not one mechanism either
Instrumented the MULTRET path (`nresults == 0`) and counted clears per file:

    533  30 clears     22  11     368  1     85  1     53  ZERO

`53_PathDisplay` has NO multret clear at all, so its `v7` cannot be the
assumes-one-result hole. Its emitted `renderPoint` shows the actual damage: an
`if` whose ELSE ARM IS MISSING, with the pooled-object assignment gone from both
arms. Different mechanism, wrongly grouped by the tag.

For 368 and 85 the single clear DOES cover the defect register (clear_start=1
with reg 19; clear_start=5 with reg 13).

### The tags are correlational, not causal
`note_unknown` is REGISTER-KEYED AND OVERWRITING, so a tag records a site that
touched the register last, not the site that caused the loss. Rotation 19's
census was read as four mechanisms; that reading is too strong. Corrected rule:
use the tags to GROUP candidates, never to name a cause.

### The consolidation
A1a (MERGE_COMPOUND), A1c (MERGE_ARM_NAMES), A1d (METHOD_RECV_REJECT) and
`53`'s `v7` are all the SAME question: a register holding a live value around an
`if` loses it. That is one mechanism with four downstream reporters, which is
why fixing at the reporters kept failing.

### BLOCKED: no v9-emitting compiler, so no synthetic reproducer
The escalation rule says build a sub-20-line reproducer, compile it, diff. Tried:

    local function destroySteering(char)
        local sv = char.PrimaryPart:FindFirstChild("SteeringVelocity")
        if sv then sv:Destroy() end
    end

`tools/luau/luau-compile.exe` emits **bytecode version 11**; the corpus and the
lifter are **v9**. Widening the gate to 3..=11 as an experiment decoded that
function into two module-level locals - the whole function body gone - which is
the version mismatch, not a lifter defect. Any bug "found" this way would be an
artifact. Gate reverted to 3..=9.

MISSING CAPABILITY, stated precisely: a compiler that emits v9. Until there is
one, the reproducer step of the escalation rule is unavailable and the
substitute is the corpus itself - find a file where the same construct
decompiles CORRECTLY and diff the two. Candidate sources for the capability: a
Roblox client's own compiler (the same route `probe align` already uses to read
opcode permutations), or an older Luau build pinned to v9.

### State after rotation 20
A **CLOSED**  D **CLOSED**  E **CLOSED**  F **CLOSED**  G **CLOSED**  H **CLOSED**
A1d **RESOLVED as symptom** (receiver correctly rejected; cause is upstream)
A1* MERGED into **A2: a register loses a live value across an `if`** - PARKED (1)
    under the new framing, with the attack changed from "adjust the merge" to
    "find a corpus file where the construct survives, and diff".
A1b partially separated: 368/85 remain multret-plausible; 53 is the
    missing-else-arm mechanism and is regrouped with B.
B  PARKED (1)   C PARKED (2)
Remaining: 16 defects. Sweep re-run against run_r18 IN PROGRESS.


## Rotation 21 - A2 gets its reproducer, from the corpus instead of a compiler

### The census: 20 correct, 4 broken, same construct
Scanned all 1,142 outputs for `local x = <recv>:FindFirstChild(k)` followed by
`if x then x:method() end`, comparing the guard name against the receiver name:

    CORRECT (receiver == guard)   20
    BROKEN  (receiver != guard)    4
        394_LocalBeequip   BeequipPart      -> self
        531_Morpher        Transparency3    -> self
        54_Controls        SteeringVelocity3-> v3
        54_Controls        SteeringVelocity4-> v7

The construct decompiles correctly five times out of six, so this is not a
missing feature - it is a condition that occasionally destroys the receiver.
The broken receiver is `self` twice and a minted `vN` twice, and the `vN` cases
are the ones `method_receiver_expr` reports, i.e. the register held `Nil`.

### THE REPRODUCER - two adjacent blocks, one file, one correct one broken
`531_Morpher`, lines 146-153, is the minimal case the missing v9 compiler could
not give me:

    local PreMorphTransparency2 = self:FindFirstChild("PreMorphTransparency")
    if PreMorphTransparency2 then
        self.Transparency = PreMorphTransparency2.Value
        PreMorphTransparency2:Destroy()      -- CORRECT
    end
    local Transparency3 = self:FindFirstChild("IsMorph")
    if Transparency3 then
        self:Destroy()                        -- WRONG: Transparency3:Destroy()
    end

Same file, same function, same construct, same NAMECALL receiver (`self`),
adjacent. Everything global is held constant, so whatever differs between these
two blocks IS the mechanism. The visible difference: the working block's body
reads `PreMorphTransparency2.Value` BEFORE the method call; the broken block's
body contains the method call ALONE.

That makes the candidate mechanism precise: when the guard register's only use
inside the arm is as a NAMECALL receiver, the receiver resolves to the stale
NAMECALL self-slot (or to Nil) instead of the guard's binding. A read of the
register earlier in the arm appears to be what keeps it alive.

### Why this is the right artifact to resume on
It needs no compiler, it is four lines, it holds every confound fixed, and it
predicts a discriminating test: add a read of the guard before the method call
in a broken case and the defect should disappear. That prediction has NOT been
run yet - it is the next step, and it is cheap.

### State after rotation 21
A **CLOSED**  D **CLOSED**  E **CLOSED**  F **CLOSED**  G **CLOSED**  H **CLOSED**
A1d **RESOLVED as symptom**
A2 PARKED (2) WITH A REPRODUCER (531_Morpher:146-153) and a falsifiable
   prediction. Unblocking question: in the broken block, what does the
   receiver register hold at the NAMECALL, and which instruction last wrote it -
   the guard's CALL result, or the enclosing NAMECALL's self slot?
B  PARKED (1)   C PARKED (2)
Remaining: 16 semantic defects. Sweep re-run against run_r18 in progress.


## Rotation 21 CORRECTION - I invented two of the four defects

### SUPERSEDES the rotation 21 census above. Read this with it.
The claim "20 correct, 4 broken" is WRONG. The heuristic behind it - that in
`local x = recv:FindFirstChild(k); if x then <recv2>:m() end` the receiver must
equal the guard - is not a property of correct Luau. `if x then self:m() end` is
ordinary code.

`LUAU_RECV_TRACE` on `531_Morpher` printed what each NAMECALL receiver resolved
to:

    RECV idx=1 holds=Name("PreMorphTransparency2")   <- the "working" block
    RECV idx=0 holds=Name("self")                    <- the "broken" block

Register 0 is `self` and it genuinely holds `self`. The emitted `self:Destroy()`
is what the bytecode says, and as game logic it is right: a Morpher part that
finds an `IsMorph` marker destroys ITSELF. `394_LocalBees` is the same shape -
`local BeequipPart = self:FindFirstChild("BeequipPart"); if BeequipPart then
self:Destroy() end`.

CONFIRMATION FROM THE INSTRUMENT: the semantic checker never flagged 531 or 394.
It flagged only `54_Controls`, where the receiver is literally `Nil`. The
checker was right and my census was wrong.

### What actually remains
Of the four, TWO were my invention and two are real - and the two real ones
(`54_Controls` v3 and v7) are already inside the 16 the checker reports. This
census added no new information; it added noise, and cost a rotation.

### The rule this breaks, and the corrected one
CLAUDE.md already warns that models invent defects on this corpus, and names
Roblox child-instance lookups as the classic false positive. I wrote a NEW
heuristic and trusted it without checking it against a case I knew to be
correct. The corrected rule: **a hand-written census is an instrument too, and
an instrument that disagrees with the checker must be validated before its
disagreements are treated as findings** - the same standard applied to the
checker itself in rotation 17, where five findings were proven false before the
scanner was touched.

The prediction offered in rotation 21 ("a read of the guard before the method
call keeps it alive") was already falsified by data in hand when it was written:
`260_BeePopUp`'s `if GiftedSong2 then GiftedSong2:Play() end` has no prior read
and is correct. It should not have been recorded as open.

### State after the correction
A2 remains PARKED (2). Its evidence is now ONLY `54_Controls` v3/v7 - a receiver
register holding `Nil` where the source has a live `FindFirstChild` result. The
`self` flavour does not exist. `531_Morpher` is NOT a reproducer and must not be
resumed on.
No code was changed by rotation 21; the instrumentation was reverted.
Remaining: 16 semantic defects, unchanged. Sweep re-run in progress.


## Rotation 22 - the SWEEP measure, first attack: the grader ignores its own rule

### The composition of the sweep gap
`sweep_full1143.jsonl`: 781 clean, 79 suspect, 246 broken, 5 error, 712 defects.

    name_collision      388  (55%)
    undefined_register  171
    dead_store           68
    dropped_call         39
    truncated_body       38
    duplicate_call        8

### Adjudicating name_collision by hand - 6 sampled, 2 real
    230_DigitalBeeConstants  a bool from a comparison named `...Count8`,
                             correctly computed and returned      COSMETIC
    694_Take_Damage          `Amount6` called as a function - Amount is
                             legitimately number-or-function      COSMETIC
    12_FuzzyRow              claims `BackgroundColor3` redefined at line 18;
                             line 18 declares `BackgroundColor34`  MISREAD
    268_BeequipRow           `game = game.ReplicatedStorage`       REAL
    699_RBCUpgrades          `local Color3 = ...` after `Color3.fromRGB` on the
                             line above - Lua scoping resolves both correctly,
                             fragile but not wrong                 COSMETIC
    59_LocalCubs             `v4[v4] = {...}` with v4 an Instance - the
                             `table.remove(tbl4, tbl4)` shape      REAL

### The finding: the grader breaks the rule it was given
Reading the prompt closes this properly. It DEFINES the kind as cosmetic -
"Misleading variable names are name_collision, not broken logic, when the
surrounding code is structurally correct" - and states the verdict rule:
"suspect if only naming/redundancy issues". So four of the six are not
mis-CATEGORISED at all; they are correctly categorised cosmetic findings.

The defect is in the VERDICT. Measured across the corpus:

    246 broken files
      147 carry a correctness kind
       99 carry NOTHING but name_collision / dead_store   <- 40%

Forty per cent of "broken" is a file graded non-runnable on the strength of an
ugly name, against the grader's own written rule.

### FIXED at the decoder, not the prompt
The script already refuses to trust the model on `kind` - the enum is enforced
at the decoder because free-string kinds produced `Badge Awarding` and
`RemoteEvent_Execution`. The verdict needed the same treatment for the same
reason. `enforce_verdict()` downgrades broken -> suspect ONLY when EVERY defect
in the file is cosmetic, keeps `model_verdict` alongside so the disagreement
stays auditable, and leaves every defect in the record with its line intact.

MUTATION TEST, 7/7: cosmetic-only downgrades; cosmetic + one correctness kind
STAYS broken; dropped_call stays broken; clean and suspect are untouched;
broken with an empty defect list is left alone. It downgrades only, and it
cannot silence a finding.

Applied to the existing data:

    clean            781 -> 781
    suspect           79 -> 178
    broken           246 -> 147
    clean-or-suspect 860 -> 959   of 1111

### HONESTY: what this is and is not
This did NOT repair any output. It corrected a grader that was contradicting
its own instructions, exactly as rotation 17 corrected the semantic checker's
read scanner. The 99 files still carry their findings; they are now graded as
what the prompt says they are. Real correctness is unchanged by this commit.

The 147 remaining broken files are the honest sweep target, and they are the
ones with `undefined_register`, `dropped_call`, `truncated_body` and
`duplicate_call` - the kinds that describe lost values rather than ugly ones.

### Note on the in-flight run
The sweep re-run against `run_r18` was launched BEFORE this change and Python
had already loaded the script, so its output carries un-enforced verdicts.
Post-process it with `enforce_verdict` before reading its numbers.

### State after rotation 22
SEMANTIC: A, D, E, F, G, H **CLOSED**; A1d resolved as symptom; A2 PARKED (2),
  evidence narrowed to 54_Controls v3/v7; B PARKED (1); C PARKED (2).
  16 defects remain.
SWEEP: grader verdict rule **CLOSED** (false-positive class, rule enforced and
  recorded). name_collision adjudicated as cosmetic BY THE PROMPT'S OWN
  DEFINITION - 388 findings reclassified, not silenced. 147 files remain as the
  real target, unadjudicated.


## Rotation 23 - two corrections to my own instruments, and the family they were hiding

### CORRECTION 1: my undefined_register census had 8 false positives
Wrote an independent mechanical pass for "bare vN read but never assigned" over
run_r18: 26 names in 23 files, against the checker's 16 in 15. The 8 extra files
(1024, 1100, 348, 372, 381, 490, 83, 879) all turned out to be lines like

    local v2

appearing EXACTLY ONCE in the whole file - declared, never assigned, never read.
My pass counted the declaration itself as a read. The checker skips `local`
lines without `=` and was right; I was wrong. The checker's recall for this
class is confirmed, not suspect.

Residue worth noting: a `local vN` with zero uses anywhere is still emitter
noise, present in 8 files. Harmless to run, but the sweep model reads it as
`undefined_register`, so it manufactures sweep findings.

### CORRECTION 2: dead_store is NOT cosmetic, and I downgraded 18 files wrongly
Rotation 22 put `dead_store` in COSMETIC_KINDS alongside `name_collision`. That
was an overreach. `734_StatModifiers_BaseConversionRate`:

    local Op3 = Op .. " Convert Amount"
    if arg2 then
        if arg2.Color then
            local Op4 = ((Op3 .. " (") .. arg2.Color) .. " Bees)"
        end
    end
    return Op3                    -- returns the SHORT string

The original reassigned `Op3`. The dead local IS the visible symptom of a lost
reassignment and the function returns the wrong value. Removed from the set.
Measured effect of the correction: broken 147 -> 165, suspect 178 -> 160.
Only `name_collision` remains cosmetic, and that is the prompt's own wording.

### CORRECTION 3: my first count of the family was over-broad by 7x
First heuristic - "dead local whose RHS mentions a later-used name" - gave 1,161
in 367 files. It is junk: `local Players = game:GetService("Players")` matches
because `game` is read later, and that is an ordinary unused local. Tightening
to the actual signature - the RHS must EXTEND a base that is still live after
the line (`^base <op>`) - gives **162 in 89 files**. The 1,161 was never
reported as a finding and must not be.

### THE FAMILY, verified by hand
`213_Collectibles_Tornado`:

    local Lvl3 = Lvl2 + GetStacksBySource2 / 3.2
    if 0 < GetStacksBySource then
        local Lvl4 = Lvl3 + 2        -- computed, DISCARDED
    end
    local Lvl5 = Lvl3 * Multiplier   -- uses Lvl3, never the +2

The `+2` is silently lost. This is CLAUDE.md's 26b3b9f shape - a value assigned
in a branch arm to a fresh `local`, discarded at the merge - and the sweep's own
kind enum already has a name reserved for it: `branch_local_discarded`.

### WHY THE TWO MEASURES DISAGREE - the real answer
The semantic checker has NO CHECK for this shape. Its checks are
declared_never_assigned, undefined_locals, body_recovered,
discarded_table_writes, property_called_as_method, name_body_agreement. A value
computed into a fresh local and dropped is invisible to every one of them.

That is why semantic reads 98.7% while the sweep reads far worse. The sweep is
not noisier here - it is seeing something the checker is blind to. 98.7% is an
overstatement of correctness, and the honest move is to make the checker see the
family even though doing so LOWERS the number.

### truncated_body adjudicated by bytecode size
All 36 old-sweep `truncated_body` files are >= 300 bytes of bytecode, so none is
the trivial-script false positive I expected (`153_Indicators` is 44 bytes and
correctly emits `return {}`; `1003_ServerEffect` is 65 bytes and correctly emits
an empty function). Bytecode size is an objective arbiter for this class and it
says the 36 are REAL. Corpus-wide there are 37 `return {}` stubs and 194 empty
function bodies; the trivial ones among those are correct output.

### State after rotation 23
SEMANTIC: A, D, E, F, G, H CLOSED. A2 PARKED (2). B PARKED (1). C PARKED (2).
  16 defects by the current checks - but the checker is BLIND to
  branch_local_discarded, so this number overstates correctness.
SWEEP: verdict rule CLOSED with a correction (dead_store restored to
  correctness). truncated_body ADJUDICATED REAL (36 files, bytecode-size
  arbiter). undefined_register census validated the checker.
NEW MECHANISM **I: branch_local_discarded** - 162 sites in 89 files, verified by
  hand on 213_Tornado and 734_BaseConversionRate. Next action: add the semantic
  check FIRST so the family is measurable, then fix the lifter. Expect the
  semantic clean count to FALL when the check lands; that is the check working.


## Rotation 24 - the check that made the measure honest, and it went DOWN

### Added: `branch_local_discarded`
Wired into `check()` on the blanked source. Three conditions, all load-bearing:
the local is mentioned exactly once in the file; its initialiser EXTENDS a base
(`base ..`, `+`, `*`, `/`) rather than being a fresh value; and that base is
still read after the line. Condition 2 is what separates 162 real sites from the
1,161 candidates the loose version produced.

MUTATION TESTS, 5 added, 933 total passing. Each negative removes exactly ONE
condition so a later widening turns a test red instead of quietly inflating
findings:
  - the Tornado shape fires
  - an unused `local Players = game.Players` does NOT (condition 2)
  - a local that is read later does NOT (condition 1)
  - an extension whose base dies with it does NOT (condition 3)
  - a string mentioning the name does NOT suppress a real finding

### Measurement - clean run, empty out-dir, 1142/1142 decodable
    1,127 -> 1,045 clean   (98.7% -> 91.5%)
    16    -> 153 defects

**The number went down by 82 files and that is the point.** Those 137 dropped
values were in the output the whole time; no check looked for them. 98.7% was
never true. Nothing regressed - the same binary, the same output, a wider
instrument. The two `opmap_dupclosure_detection` failures are the same
pre-existing pair.

This is the inverse of rotations 17 and 22, where the instrument was too LOOSE
and the count rose once it was corrected. Same discipline, opposite direction:
the measure follows the evidence, not the target.

### Why the sweep was right and the semantic checker was wrong
The sweep has flagged this family all along as `dead_store`, and its kind enum
even reserves `branch_local_discarded`. Rotation 22 dismissed `dead_store` as
cosmetic and rotation 23 had to take that back. The lesson is specific: when two
instruments disagree, the one reporting MORE is not automatically the noisy one.
Check what it sees before deciding which to trust.

### State after rotation 24
SEMANTIC: 1,045/1,142 (91.5%), 153 defects - an honest baseline for the first
  time. 16 are the old families; ~137 are mechanism I, newly visible.
  A, D, E, F, G, H CLOSED. A2 PARKED (2). B PARKED (1). C PARKED (2).
MECHANISM **I: branch_local_discarded** - MEASURED (162 sites, 89 files),
  checked, mutation-tested. NOT YET FIXED. The fix is in the lifter: an arm that
  writes a register already bound before the branch must emit `Stat::Assign` to
  the existing name, not a fresh `Stat::Local`. That is the same shape as
  rotation 17's mechanism G, where `classify_write` already computes the right
  answer (`Reassign`) and the caller discarded it - so the first place to look
  is every remaining site that ignores the returned `WriteKind`.
SWEEP: re-run against run_r18 still in flight.


## Rotation 25 - mechanism I FIXED: `Lvl3` and `Lvl4` were never two variables

### Where it was NOT
Mechanism G was a site that discarded `classify_write`'s answer, so the first
move was to audit every call site. All of them destructure `(kind, name)`
correctly. The fault was in `classify_write` itself.

### The mechanism
Look at the names in the family: `Lvl3 -> Lvl4`, `Op3 -> Op4`,
`Theme6 -> Theme7`, `GetStat -> GetStat2`. Those are ONE debug name with
different uniquifying counter suffixes, handed out by `ctx.reg_name`. The
shadow gate asks only whether both names are "semantic" (non-`vN`), which they
are, so it returns `Shadow` and the caller correctly emits a fresh
`local Lvl4 = Lvl3 + 2` inside the arm. The value then never reaches the `Lvl3`
the code goes on to use.

Fix: in `classify_write`, when the two names share an alphabetic stem, this is a
reassignment of one variable, not a new binding.

### THE GROUND TRUTH CAUGHT MY FIRST VERSION
The first version compared stems only and returned the OLD name. Full suite:

    corescripts_recover_their_published_functions ... FAILED
    RbxCharacterSounds: recovered 8/9, MISSING `map`

`map` is in the published source, so this is a definite defect, not a matter of
opinion. The cause: `mapN -> map` also matches on stem, but that direction is
the lifter RECOVERING the canonical debug name, and answering `Reassign` there
keeps the numbered variant and deletes the real name.

Narrowed to the direction the counter actually moves - `Reassign` only when the
new suffix is strictly GREATER than the old. `Lvl3 -> Lvl4` (4 > 3) and
`GetStat -> GetStat2` (2 > 0) still reassign; `mapN -> map` (0 > N false) falls
through to Shadow exactly as before.

Re-measured after narrowing: corpus numbers IDENTICAL and ground truth restored.
The narrowing cost nothing.

### Measurement - clean run, empty out-dir, 1142/1142 decodable
    1,045 -> 1,104 clean   (91.5% -> 96.7%)
    153   -> 54 defects

Verified at source level, not by count:

    213_Tornado    if 0 < GetStacksBySource then Lvl3 += 2 end
                   Lvl3 *= Multiplier            -- the +2 now arrives
    734_BaseConv   Op ..= " Convert Amount"
                   ... Op = ((Op .. " (") .. arg2.Color) .. " Bees)"
                   return Op                     -- the appended text arrives

933 unit tests pass; the two `opmap_dupclosure_detection` failures are the same
pre-existing pair.

### The arc across rotations 23-25, and what it cost
23 found the family and mis-classified `dead_store` as cosmetic on the way.
24 added the check and the measure FELL 98.7% -> 91.5%, which was the instrument
   becoming honest.
25 fixed the mechanism and it rose to 96.7%.
Net against the pre-24 baseline: 1,127 -> 1,104 clean. That looks like a
regression and is not - 1,127 was measured by an instrument blind to 137 real
defects, 99 of which are now genuinely repaired.

### State after rotation 25
SEMANTIC: 1,104/1,142 (96.7%), 54 defects, measured honestly.
  A, D, E, F, G, H, **I** CLOSED. A2 PARKED (2). B PARKED (1). C PARKED (2).
SWEEP: re-run against run_r18 COMPLETE (1,111 files) - but r18 PREDATES this
  fix, so its numbers are already stale and understate the current build.
  Post-process with `enforce_verdict`, then re-run against run_r26.
