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


## Rotation 26 - the stem rule was a REGRESSION. Reverted.

### What rotation 25 actually did
It raised the semantic measure 1,045 -> 1,104 and it made real files worse. Both
are true, and the second only became visible by READING the output.

`25_CameraScript_RootCamera`, `rk4Integrator`, before and after:

    BEFORE (r18)                          AFTER (r25/r26)
    local v18 = v19 + (arg3 + v44*max9*arg4)   ... v44 * max * arg4
    local max12 = v18 * (arg4 / 6)             max = v18 * (arg4 / 6)
    local v22 = 2 * (v44 * max6)               local v22 = 2 * (v44 * max)
    v21 = v44 * max3 + v22                     v21 = v44 * max + v22
    v22 = 2 * (v44 * max9)                     v22 = 2 * (v44 * max)
    v19 = v21 + v22 + v44 * max11              v19 = v21 + v22 + v44 * max

Five distinct values - `max3`, `max6`, `max9`, `max11`, `max12` - collapsed onto
one `max`. RK4 needs four distinct k terms; the fixed build sums ONE value four
times. It also clobbers `local max = math.max` while the nested `acceleration`
still calls `max(1, ...)`.

### Why the rule was wrong
`classify_write` is keyed on a register, so a stem match does mean the same
REGISTER - but a register is REUSED, and the original source had distinct locals
for each occupancy. Shadowing preserves that distinction; reassigning destroys
it whenever an earlier value is still read later. `Lvl3 += 2` was safe because
nothing needed the old `Lvl3` afterwards. RK4 needs every one of them.

Same stem does not mean same value.

### The instrument was blind to the damage, AGAIN
The semantic checker has no check for a value collapse, so it scored the
collapse as a 99-defect improvement. Rotation 24 taught that a rising number can
mean a blind instrument; this is the same lesson pointing the other way, and it
was caught only because I read `rk4Integrator` rather than trusting the count.

The checker rule that follows: a fix that raises the score is not verified until
its OUTPUT has been read on a file it changed.

### Reverted
`git checkout backup/pre-stem-rule -- naming.rs`. Clean run confirms the
restoration: 1,104 -> **1,045 clean (91.5%), 153 defects**, and RootCamera again
emits `max6` and `max9` as separate values. Only the two pre-existing
`opmap_dupclosure_detection` failures remain.

The `branch_local_discarded` CHECK stays - it is honest and the family is real.
It is the FIX that was wrong.

### Mechanism I, corrected hypothesis
The discriminator is not the name. It is whether the initialiser is built FROM
the register's own current value:

    local Lvl4 = Lvl3 + 2          Lvl3 IS this register's current name -> accumulate
    local max5 = (arg2 + max)/3.3  `max` is ANOTHER register's name    -> new value

An accumulate can safely become `Lvl3 += 2`. A fresh value must keep its own
binding. `classify_write` cannot make this call because it never sees the
initialiser; the decision belongs at the CALL SITE, which has the RHS in hand.

### State after rotation 26
SEMANTIC: 1,045/1,142 (91.5%), 153 defects - honest, and unchanged from the
  rotation-24 baseline.
  A, D, E, F, G, H CLOSED. **I RE-OPENED, PARKED (1)** with the corrected
  hypothesis above and a named next step: move the decision to the call site and
  gate on "RHS references this register's current name".
  A2 PARKED (2). B PARKED (1). C PARKED (2).
POSSIBLE NEW CHECK, noted not built: the collapse itself is detectable - the
  same statement emitted twice with identical text where the algorithm needs
  different values (`v22 = 2 * (v44 * max)` twice). The sweep already has a kind
  for this shape, `duplicate_call`.
SWEEP: r26 run ABANDONED (it targeted the reverted build). Re-running against
  run_r27.


## Rotation 27 - second hypothesis for mechanism I, also falsified

### The attempt
Rotation 26's corrected hypothesis: the discriminator is not the NAME but the
INITIALISER - `local Lvl4 = Lvl3 + 2` is built from this register's own current
value and is therefore an accumulate, while `local max5 = (arg2 + max) / 3.3`
reads a different register and must keep its binding. Implemented in
`emit_local_or_assign`, which is the one site holding both the register and the
value: on `Shadow`, if `expr_references_name(&value, prev)` then emit
`Assign` to `prev` instead.

### MEASURED: no gain, and a regression
Clean run: **1,045 clean / 153 defects - identical to the baseline.** It did not
fix `213_Tornado` at all (`local Lvl4 = Lvl3 + 2` unchanged), which proves
`emit_local_or_assign` is not the path that emits the family.

It changed 9 files, and reading one - the rule rotation 26 wrote down - shows it
made `149_ReplicatedStorage_Class` wrong:

    BEFORE                                  AFTER
    local tbl = self or {}                  local tbl = self or {}
    local tbl2 = {                          tbl = {
        __metatable = tbl,                      __metatable = tbl,
        __index = tbl }                         __index = tbl }
    ...                                     ...
    tbl.new = tbl.new or new                tbl.new = tbl.new or new
    return tbl2                             return tbl

Before, `tbl` is the class table and `tbl2` the metatable pointing at it, so
`tbl.new = ...` adds methods to the CLASS. After, `tbl` has been overwritten by
the metatable, the methods land on the wrong table, and `__index = tbl` still
captures the old one - both objects are live and the rule merged them.

### What both failures share
    Lvl3 = Lvl3 + 2        old value CONSUMED, dead afterwards   -> safe
    tbl  = {__index = tbl} old value CAPTURED, still live        -> unsafe
    max9 = f(other regs)   unrelated value                       -> unsafe

Neither the name (rotation 25) nor the initialiser (rotation 27) distinguishes
these. Both are proxies for the property that actually matters: **is the
register's OLD value read after this write?** If it is dead, reassigning is
safe; if it is live, the arm needs its own binding.

### Mechanism I: PARKED (2), attack must change
UNBLOCKING QUESTION, stated exactly: at the point of a `Shadow` write to
register R at pc P, is R's previous value read anywhere reachable from P before
being overwritten? The lifter tracks `regs` and has the proto's instruction
stream, so a backward scan from P over the remaining code of the enclosing
region should answer it without a full dataflow pass. If that is too coarse
inside branches, the fallback is to restrict the rewrite to the case where the
write and every subsequent read lie in the SAME region.

Reverted to `backup/pre-accumulate-rule`. `149_Class` confirmed restored:
1,045 clean / 153 defects.

### Rule earned across 25-27, worth keeping
Rotation 25's fix raised the score and broke files. Rotation 27's fix changed
nothing and broke a file. In both cases the semantic checker was content.
**Read the diff on a changed file before believing any measurement** - the count
is necessary and never sufficient.

### State after rotation 27
SEMANTIC: 1,045/1,142 (91.5%), 153 defects.
  A, D, E, F, G, H CLOSED. **I PARKED (2)** with a liveness question and two
  falsified hypotheses recorded so neither is retried.
  A2 PARKED (2). B PARKED (1). C PARKED (2).
SWEEP: running against run_r27, which is byte-identical to run_r29 for this
  purpose (both are the reverted build).


## Rotation 28 - mechanism I CLOSED, on the third predicate

### Finding the path that actually emits it
Rotation 27 proved `emit_local_or_assign` is not it - patching there left
`213_Tornado` untouched. The real site is the REBIND PEEK inside
`store_complex`:

    let do_peek = !carried_is_loopvar && !ctx.is_pinned_reg(reg as u8, pc);
    let rebind_name = if do_peek {
        let fresh = ctx.reg_name(proto, reg as u8, pc);
        if fresh != name && is_semantic_local_name(&fresh) { Some(fresh) } else { None }
    } ...

On `Lvl3 = Lvl3 + 2` the peek asks for a fresh hint, the uniquifier answers
`Lvl4`, that differs and is semantic, so it reads as a rebind -> Shadow ->
`local Lvl4 = Lvl3 + 2` inside the arm. `store_complex` ALREADY detects this
shape for loops (Phase B0.3, in its own header) and its `else` branch keeps the
carried name for exactly this reason. The peek simply ran first.

### Three predicates, two of them wrong - and the third is the checker's
    1. NAME STEMS (rotation 25)   `Lvl3`~`Lvl4` -> collapsed max3/max6/max9 in
                                  rk4Integrator into one `max`. REVERTED.
    2. ANY reference (this rotation, first cut)
                                  `X = X.field` counted, so
                                  `local VREnabled6 = VRService.VREnabled`
                                  became `VRService = VRService.VREnabled` and
                                  destroyed the service reference. Also
                                  `Players = Players.LocalPlayer`.
    3. BinOp reference            `matches!(value, Expr::BinOp{..})
                                  && expr_references_name(value, name)`

Predicate 3 is deliberately the SAME one `check_branch_local_discarded` uses to
recognise the family. The checker and the fix now agree on what the defect is,
which is why the fix lands exactly on what the check reports.

A field read narrows one value into another and the old value stays live; a
BinOp on the carried name consumes it. That is the liveness distinction rotation
27 asked for, expressible without a liveness pass because the SHAPE carries it.

### Measurement - clean run, empty out-dir, 1142/1142 decodable
    1,045 -> **1,115 clean (91.5% -> 97.6%)**, 153 -> **35 defects**

VERIFIED BY READING THE OUTPUT, per the rule earned in rotations 25-27, on the
target AND on both previous regression victims:

    213_Tornado    `Lvl3 += 2` inside the arm; `Lvl3 *= Multiplier` after   FIXED
    25_RootCamera  `local VREnabled = VRService.VREnabled` intact           SAFE
                   max3 / max6 / max9 / max12 still distinct                SAFE
    149_Class      byte-identical                                          SAFE

933 unit tests plus 6 new ones; CoreScript ground truth passes; only the two
pre-existing `opmap_dupclosure_detection` failures remain.

### Mutation tests - `tests/self_mutation_peek_tests.rs`
Six cases pinning the predicate, each negative reproducing a real regression so
neither dead hypothesis can be reintroduced silently:
  arithmetic on itself / concat onto itself / nested reference   -> accumulate
  a FIELD read of itself (the RootCamera regression)             -> not
  arithmetic on a DIFFERENT name (the max5 case)                 -> not
  a fresh value                                                  -> not

### State after rotation 28
SEMANTIC: **1,115/1,142 (97.6%), 35 defects.**
  A, D, E, F, G, H, **I** CLOSED - I on the third attempt, with both failures
  recorded and pinned by tests.
  A2 PARKED (2). B PARKED (1). C PARKED (2).
SWEEP: running against run_r27. NOTE it now predates this fix, so its numbers
  will understate the build again; re-run after it lands.


## Rotation 29 - what survives mechanism I, split into two sub-families

Remaining 35: **21 branch_local_discarded, 14 declared_never_assigned.**

### J: the accumulator is copied to a scratch register and never written back
`819_StatReqs_Completed_Quests`, whole function:

    local Name2 = ("Complete " .. require(NumberCommas)(self.Amount or 1)) .. " "
    local Pool = self.Pool
    if self.Pool then
        Pool = Name2                          -- accumulator COPIED to a scratch reg
        local Name3 = (Pool .. self.Pool) .. " "   -- extended THERE
    else
        ... Pool = Name2; local Name4 = (Pool .. self.VisiblePoolName) .. " " ...
    end
    Name2 ..= "Quest"                         -- Name2 never received any of it

The source is plainly `desc = desc .. self.Pool .. " "` in each arm. Rotation
28's guard correctly does NOT fire: the carried name at the write is the scratch
(`Pool`), not the accumulator (`Name2`), so the value is not built from the
register's own previous value. The defect is one level up - the copy in and the
missing copy out.

Same shape at `165_QuestListener` (`Theme5 <- Theme4`), `263_BeeTypeStatsBox`
(`value35 <- value33`), `919_pprint` (`found8 <- found7`).

UNBLOCKING QUESTION for J: when an arm opens with `scratch = <accumulator>` and
then writes `scratch` from a BinOp on itself, is the bytecode a MOVE-in without
a MOVE-out (so the lifter dropped the write-back), or does the CONCAT/ADD target
the accumulator's register directly and the lifter mis-assigned its destination?
That is answerable from the instruction stream for one file and decides whether
the fix belongs in the MOVE handling or in the arithmetic destination.

### K: a value extended off a PARAMETER
`v34 <- arg1`, `Value7 <- arg2`, `Size <- arg3`, `pow31 <- arg4`, `abs4 <- arg1`
- 9 of the 21. `local v34 = arg1 + 0.1` where `v34` is never read.

NOT yet adjudicated, and it must not be assumed real: a dead local in the
ORIGINAL source would look identical. `80_AgeString`'s
`local floor12 = floor3 * 0x3C` sits beside branches that use `floor3` directly
and may simply be dead code the game shipped. Deciding this needs the bytecode,
not the emitted text.

### State after rotation 29
SEMANTIC: 1,115/1,142 (97.6%), 35 defects.
  A, D, E, F, G, H, I CLOSED. A2 PARKED (2). B PARKED (1). C PARKED (2).
  **J PARKED (0)** with the question above. **K UNADJUDICATED (9 sites)** -
  next action is to read the bytecode for one and decide real vs. shipped dead
  code, because the emitted text cannot tell them apart.
SWEEP: still running against run_r27, which predates rotation 28.


## Rotation 30 - MINUS is genuinely passthrough, and K is half false positive

### The lead
`885_Utils_Easing` implements Penner easing, whose closed forms are known, so it
is usable as ground truth. `outExpo` should be
`c * 1.001 * (-2^(-10t/d) + 1) + b`, and the emitted body reads

    arg3 / 2 * 1.001 * (pow(2, -10 * (arg1 * 2) / arg4) + 1) + arg2

with no negation on the `pow` term. Every file header says
`minus=passthrough`, and commit 26a191f had already found the probe WRONG about
LENGTH ("the probe said LENGTH was passthrough, the bytecode said otherwise").
The DB entry confirms the asymmetry:

    semantics = {unary_length: "operator", unary_minus: "passthrough",
                 unary_not: "passthrough"}

### MEASURED, and decisively negative
Flipped `unary_minus` to `operator` in a COPY of the DB and ran the corpus:

    1,115 -> **743 clean (97.6% -> 65.1%)**, 35 -> **1,126 defects**

MINUS as a real operator is catastrophically wrong. B0.73's passthrough decision
is not an inference artefact - it survives measurement against the current
MEASURED opmap, which is the strongest test available. The 150 corpus hits it
cited (`v8.Parent = -v0`, `(-v0):Destroy()`) stand.

Consequence: whatever produces the missing negation in Easing, it is NOT the
MINUS opcode. Experiment DB deleted; the real DB was never edited.

This is worth the rotation on its own - it converts a decision made under
inferred opmaps into one verified under a measured opmap, and closes a lead that
looked compelling.

### K adjudicated: 8 of 20 lose nothing
`885_Easing`, `outInExpo`:

    if arg1 < arg4 / 2 then          -- d/2 computed HERE
        local pow31 = arg4 / 2       -- and again, dead

The value is not lost - it is on the line above, and the original source has no
`d/2` local at all. So this is EMITTER NOISE reported as a dropped value:
`check_branch_local_discarded` says "the arm's value was written to a fresh
local instead of back to the base", and here there was no value to write back.
CONFIRMED false positive for `885_Easing` x3 (pow31, pow34, abs4).

A crude test - does the initialiser's text appear elsewhere in the file? - puts
**8 of 20 in this class and 12 as unique**. The 12 include `819_StatReqs`,
`165_QuestListener`, `263_BeeTypeStatsBox`, `859_StickerPlacer`, which are the
mechanism-J shape and are real.

### The checker is NOT being weakened on this
Substring matching is not sound evidence, and a rule that suppresses findings on
a crude test is exactly the mistake rotations 25, 27 and 22 each made in a
different costume. Recorded as: 3 CONFIRMED false positives, 5 CANDIDATES
needing per-case confirmation, 12 real. The sound version of the refinement
needs expression-level comparison, not text.

### State after rotation 30
SEMANTIC: 1,115/1,142 (97.6%), 35 defects - of which 3 are now proven checker
  false positives, so the true defect count is 32.
  A, D, E, F, G, H, I CLOSED. **MINUS lead CLOSED as a measured dead end.**
  A2 PARKED (2). B PARKED (1). C PARKED (2). J PARKED (0).
  K PART-ADJUDICATED: 3 confirmed FP, 5 candidates, 12 real (they are J).
SWEEP: still running against run_r27.


## Rotation 31 - mechanism J CLOSED: accumulating through a MOVE alias

### The answer to J's question
Luau's CONCAT needs its operands in CONSECUTIVE registers, so
`desc = desc .. x .. " "` first MOVEs the accumulator into the start of a
scratch range. The arm therefore reads

    Pool = Name2                              -- the MOVE
    local Name3 = (Pool .. self.Pool) .. " "  -- extends the SCRATCH name

Rotation 28's guard asks whether the value references the CARRIED name; here it
references the alias, so it could not fire. Not a lost write-back and not a
mis-assigned destination - a name-visibility gap, which is the third option J's
question did not list.

### The proof was inside one of the changed files
`748_StatModifiers_CogsPerRound` has both shapes in one function:

    if self.Op == "Add" then
        Op3 = Op
        local Op5 = ((((Op3 .. " (") .. FromUpgrade) .. " + ") ... )   -- DROPPED
        return Op5
    end
    if self.Op == "Mul" then
        Op = ((((Op .. " (") .. FromUpgrade) .. " + ") ... )           -- correct
    end

The Mul branch needs no alias, so rotation 28 already fixed it. The Add branch
goes through `Op3 = Op` and did not. The two branches disagreeing about their
own accumulator IS the defect, and after the fix they are identical.

### The rule
One level, and only when the MOVE is the IMMEDIATELY preceding statement. A
longer chain is not evidence, and matching loosely is how rotations 25 and 27
broke files. The BinOp gate still applies, so `X = X.field` cannot be smuggled
through an alias.

### Measurement - clean run, empty out-dir, 1142/1142 decodable
    1,115 -> **1,118 clean (97.9%)**, 35 -> **31 defects**

16 files changed; read five, all correct write-backs, including
`708_ReadableTimeString` where the following `floor14 = floor15` was updated to
`floor14 = floor11` consistently.

    819_StatReqs   `Name2 = (Pool .. self.Pool) .. " "`, then `Name2 ..= "Quest"`
    748_CogsPerRound  Add and Mul branches now agree

939 unit tests plus 4 new alias cases (12 in the file); CoreScript ground truth
passes; only the two pre-existing `opmap_dupclosure_detection` failures remain.

### State after rotation 31
SEMANTIC: **1,118/1,142 (97.9%), 31 defects** - and 3 of those are the proven
  `885_Easing` checker false positives, so 28 real.
  A, D, E, F, G, H, I, **J** CLOSED. MINUS lead closed as a measured dead end.
  A2 PARKED (2). B PARKED (1). C PARKED (2).
  K: 3 confirmed FP, 5 candidates, remainder folded into J and now fixed.
SWEEP: still running against run_r27; it predates rotations 28 and 31.


## Rotation 32 - the SWEEP measured against itself: 13% verdict instability

### A controlled experiment fell out for free
Every decompiler change between `run_r18` and `run_r27` was reverted (rotation
25's stem rule, rotation 27's accumulate rule), so the two output trees are
**byte-identical across all 1,111 files**. Two sweep runs, same input, same
model, same prompt.

    files byte-identical between the builds : 1111
      same verdict in both runs             :  967   (87.0%)
      DIFFERENT verdict on identical input  :  144   (13.0%)

      broken  -> suspect  69
      clean   -> suspect  32
      clean   -> broken   15
      broken  -> clean    13
      suspect -> broken    9
      suspect -> clean     5
      error   -> broken    1

### What this invalidates
The r27 sweep read 774 clean against r18's 803 - on IDENTICAL FILES. That 29
-file "regression" is noise, and so is any comparison of sweep totals between
runs unless the delta clears a ~144-file jitter. Every cross-run sweep number in
this journal before now should be read with that band attached.

It also means a single-run sweep verdict is not evidence. `broken` appearing
once is a hypothesis; `broken` appearing twice on the same bytes is a finding.

### Adjudicating by intersection
Two independent runs over identical input give a confirmed set for free:

    broken in BOTH runs   : 147   <- confirmed
    broken in EITHER run  : 254
    clean in BOTH runs    : 756   <- confirmed

**107 of 254 broken verdicts - 42% - did not reproduce.** The honest sweep
target is the 147 confirmed, not the 254, and not either single run's number.

This is the sweep analogue of rotation 17: the instrument was reporting more
than it could support, and the correction is to require agreement rather than to
silence anything. No finding was dropped - the 107 remain in the records, marked
unconfirmed.

### Corrected rule, recorded
A sweep finding counts as REAL only when it reproduces across runs on the same
bytes. `tools/sweep_local.py` should gain an N-run vote mode; until it does, run
it twice and intersect. The deterministic semantic checker needs no such
treatment - it is the reason both instruments are kept.

### State after rotation 32
SEMANTIC (deterministic): 1,118/1,142 (97.9%), 31 defects, 3 of them proven
  checker false positives -> 28 real.
SWEEP (stochastic, +/-13% per run): 147 files confirmed broken by two-run
  agreement; 756 confirmed clean; the remainder unconfirmed either way.
MECHANISMS: A, D, E, F, G, H, I, J CLOSED. MINUS closed as a measured dead end.
  A2 PARKED (2). B PARKED (1). C PARKED (2). K part-adjudicated.
NEXT: fresh sweep against run_r32 (which has rotations 28 and 31), run TWICE,
  and adjudicate the intersection.


## Rotation 33 - mechanism C localised to one predicate

### The remaining 14 declared_never_assigned are all one thing
Every one sits at LINE 8 - the `free_var_decls` chunk-top declaration - so they
are all the A2/C family, plus one new name shape (`upval_2` in
`873_Translation`).

### The clearest reproducer yet: 651/654, sibling files
`654_Quests_TaskTypes_Collect_Pollen`, `Fulfill`:

    local Zone2 = self.Zone
    if self.Zone then
        Zone2 = self.Zone            -- and NO field write at all
    end
    local Zone3 = self.Color
    if self.Color then
        Zone3 = self.Color
        v3.Color = Zone3             -- base never bound
    end
    local Color5 = self.Tag
    if self.Tag then
        Color5 = self.Tag
        v3.Tag = Color5
    end

The source is a conditionally-built table:

    local filter = {}
    if self.Zone  then filter.Zone  = self.Zone  end
    if self.Color then filter.Color = self.Color end
    if self.Tag   then filter.Tag   = self.Tag   end

`local filter = {}` is absent from the output entirely, and the FIRST field
write is missing outright. `651_Collect_Goo` is the same file with one field.
This is exactly mechanism C's question - "can the table register be materialised
into one named local before the first branch that writes it?"

### The seeder exists, is called, and declines
`ensure_table_reg_declared` was written for precisely this and is called
UNCONDITIONALLY at the top of the `SetTableKS` handler
(`opcode_handlers.rs:1199`), which is what `v3.Color = Zone3` is. So the failure
is inside its two gates:

    needs_seed                  regs[reg] is Unknown | Bool | Number | String | Nil
    is_undeclared_non_param     reg >= param_count && !declared.contains(reg)

`LUAU_MINT_TRACE` on 654 shows `v3` tagged `REG_EXPR_UNKNOWN+UNK_MERGE_COMPOUND`
and NO `TABLE_EXPR_REJECT`, so the register really is `Unknown` and gate 1
passes. `Fulfill(self, arg2)` has 2 params and the register is 3, so the
param half of gate 2 passes.

That leaves ONE candidate: `declared` already contains the register, because
something called `locals.pre_declare(reg)` without ever emitting a `local`.
`pre_declare` inserts into `declared` and emits nothing
(`naming.rs:120`), and there are call sites at `mod.rs:2679` and `mod.rs:2913`.
A register marked declared but never declared makes every later seeder decline,
which is the same UNKNOWN-falls-through-to-a-permissive-answer shape this repo
keeps closing.

### UNBLOCKING QUESTION for C, now one line wide
Instrument `ensure_table_reg_declared` to log which gate rejected, run 654, and
confirm it is `is_undeclared_non_param`. If so the fix is either to make
`pre_declare` emit the declaration it promises, or to have the seeder treat
"declared but never emitted" as needing a seed. That is a decidable question
with a two-file reproducer (651 and 654) and a one-run test.

NOT attempted this rotation on purpose: rotations 25, 27 and the first cut of 28
each shipped a plausible fix that had to be reverted, and the cheap
instrumentation above settles it before any code changes.

### State after rotation 33
SEMANTIC: 1,118/1,142 (97.9%), 31 defects (28 real).
  A, D, E, F, G, H, I, J CLOSED. MINUS closed as a measured dead end.
  **C PARKED (3) - but localised from "a mechanism" to "one of two gates in one
  function", with the instrumentation named.** A2 and C are the same family and
  should be resumed together. A2 PARKED (2). B PARKED (1).
SWEEP: two-run intersection on run_r27 gives 147 confirmed broken, 756 confirmed
  clean. Run 1 of 2 against run_r32 in flight.


## Rotation 34 - mechanism C's cause CONFIRMED, and its fix REJECTED on principle

### The cause, measured
Instrumented `ensure_table_reg_declared` to log which gate rejects. On
`654_Collect_Pollen`:

    SEED reg=3 REJECT gate=is_undeclared_non_param
         current_name=Some("Amount4") holds=Unknown

Confirmed exactly as predicted in rotation 33. Register 3 was declared earlier
as `Amount4`, a merge reset its value to `Unknown`, and the seeder declined
because the register is "declared" - while `table_expr`, which only sees the
Unknown value, emitted an UNDECLARED `v3.Color = ...`. Two predicates
disagreeing about the same register, which is this repo's recurring shape.

(The other rejections in the trace are correct: those registers hold real
`Expr::Table` values and must not be clobbered.)

### The obvious fix works, measures well, and is WRONG
Relaxed the gate so a declared-but-`Unknown` register seeds anyway:

    1,118 -> 1,121 clean (98.2%), 31 -> 28 defects

Then read the six changed files, and every one of them says the same thing:

    53_PathDisplay   v7.CFrame = CFrame3      ->  return { CFrame = ..., Parent = ... }
                     v7.Parent = Parent
                     return v7
    215_Animator     v6.Transparency = 0.7    ->  local Parent53 = { Transparency = 0.7 }
    533_NPCs         v2.NPC = NPC4            ->  Type6 = { NPC = NPC4 }
                     arg2.Quest = v2.Name     ->  arg2.Quest = Type6.Name   (nil)
    544_BlackBear    v1.Rewards = DisplayName ->  local Quest2 = { Rewards = ... }

In every case the register held a game INSTANCE - a pooled part, an animated
collectible, an NPC - not an empty table. Seeding `= {}` FABRICATES a table the
source never had. `53_PathDisplay` stops returning a pooled Instance and starts
returning a literal table.

### Why this is a revert and not a trade
Before: `v7.CFrame = ...` errors at runtime and is visibly broken.
After: a fabricated table that looks plausible and is silently wrong.

`free_var_decls` already states the principle in its own comment: "Declaring
holes hides them. Leaving them undeclared keeps them visible to
`undefined_local`, which is the honest outcome: still broken, but broken in a
way the tooling reports."

The clean count rose BECAUSE the defect became invisible to the checker. That is
rotation 25's trap in a new costume, and it is the fourth time in this session
that a score improvement turned out to be the instrument losing sight of
something. Reverted; `53_PathDisplay` confirmed restored at 1,118 / 31.

### What this means for the 100% target
Some of the remaining 28 defects are HONEST REPORTS OF INFORMATION THAT IS NOT
IN THE DECOMPILE. The register's value was lost upstream; no naming or seeding
decision at the write site can recover which Instance it held. Driving the
checker to clean on these would require inventing data, and inventing data is
worse output than an undeclared name.

The only sound fix is upstream: stop losing the register's value at the merge -
which is mechanism A2, PARKED (2), and is the same root cause. C and A2 are one
mechanism seen from two ends.

### State after rotation 34
SEMANTIC: 1,118/1,142 (97.9%), 31 defects (3 proven checker FPs -> 28 real).
  A, D, E, F, G, H, I, J CLOSED. MINUS closed as a measured dead end.
  **C: cause CONFIRMED, local fix REJECTED as unsound, merged into A2.**
  A2 PARKED (2) and now carries C's evidence: the fix must restore the
  register's value at the merge, not paper over the symptom at the write.
  B PARKED (1).
SWEEP: run 1 of 2 against run_r32 in flight.


## Rotation 35 - A2/C CLOSED: a pending table cannot survive a branch

### The last missing fact
Traced `NewTable`/`DupTable` on `654_Collect_Pollen`:

    TABLE DupTable pc=3  reg=2
    TABLE DupTable pc=25 reg=3

The table IS created. Register 3 really does receive it, and it is the MERGE
that loses it - which is what rotation 33's `UNK_MERGE_COMPOUND` tag said and
what rotation 34's seeding attempt was papering over.

### Why the merge cannot help
A table literal deliberately sits in its register UNEMITTED so later field
writes can be folded into the constructor. Across a branch that is fatal: an arm
that adds a field leaves `Expr::Table` with a DIFFERENT field set on its side,
so at the join the two sides are not structurally equal and neither is a `Name`
- the exact fall-through to `UNK_MERGE_COMPOUND`. The register resets to
`Unknown` and every later use renders as an undeclared `v{reg}`.

No merge rule fixes this. Keeping either side loses the other arm's fields.

### The fix, at the only place it can go
Materialise pending `Expr::Table` registers into named locals immediately BEFORE
the arms lift, so both arms mutate ONE binding - which is what the source did.
Inserted just above the `regs_before` snapshot; parameters excluded.

    local Amount4 = { Category = "Pollen", Amount = Amount2 }
    if self.Zone  then Amount4.Zone  = Zone2  end
    if self.Color then Amount4.Color = Zone3  end
    if self.Tag   then Amount4.Tag   = Color5 end

The table now carries its real initial contents, and `Amount4.Zone = Zone2` -
a field write that was MISSING ENTIRELY from the old output - is back.

Contrast with rotation 34, which tried to seed `{}` at the write site: that
fabricated tables where the register had held a game Instance. Repairing at the
branch restores the table the bytecode actually created instead of inventing
one, which is why this is sound and that was not.

### Measurement - clean run, empty out-dir, 1142/1142 decodable
    1,118 -> **1,122 clean (98.2%)**, 31 -> **27 defects**

55 files changed; read four. All recoveries of tables that were previously lost:

    1026_Coconut_Dispenser   a 4-field item table, previously absent entirely
    142_CameraCutScenes      a 10-field cutscene state table, previously absent
    140_CameraBlur           `local tbl22 = nil` -> the real
                             `{BlurShadePressCallback = arg2}`

`140` also fixes a second defect for free: `tbl22` had been holding BOTH the
table and a boolean flag - one name for two variables - and materialising the
table pushed the flag onto its own binding `tbl23`.

943 unit tests pass; CoreScript ground truth passes; only the two pre-existing
`opmap_dupclosure_detection` failures remain.

### Verification note, stated plainly
This fix is region-level, so the evidence is the corpus diff read by hand (4 of
55 files, plus the 654 reproducer matching the source shape exactly) plus the
full suite - not a new unit test. The rotation-28 and -31 predicates got unit
tests because they are pure functions; this one is not.

### State after rotation 35
SEMANTIC: **1,122/1,142 (98.2%), 27 defects**, 3 of them proven checker false
  positives -> 24 real.
  A, D, E, F, G, H, I, J, **A2/C** CLOSED. MINUS closed as a measured dead end.
  B PARKED (1). K: 3 confirmed FP, 5 candidates.
SWEEP: run 1 of 2 against run_r32 in flight; run_r32 now predates this fix.


## Rotation 37 - my own check is misnamed, and the survivors are three families

### The check does not test for a branch
`check_branch_local_discarded` requires: the local is mentioned once, its
initialiser EXTENDS a still-live base, and that base outlives the line. Nothing
in it tests BRANCH CONTEXT. `859_Stickers_StickerPlacer` proves the gap:

    function tbl.PlaceSticker(self, arg2, arg3, arg4, arg5, arg6)
        local Size  = arg3 + arg2.Size.X / 2      -- top level, no branch
        local Size2 = arg4 + arg2.Size.Y / 2
        local CanvasID = PartToCanvasID2(arg2)
        if CanvasID then ...

Both are normalised sticker coordinates, computed at the TOP of the function and
never read anywhere in it - the function ends before `LocalToWorldPos` begins.
The FINDING is right: a computed value is dropped. The MESSAGE is wrong: it says
"the arm's value was written to a fresh local instead of back to the base", and
there is no arm.

CORRECTED RULE: what the check actually detects is `computed_value_discarded`.
The branch cases (mechanisms I and J) are a SUBSET, and they are the ones that
were fixable by routing the write back to the accumulator. The rest are values
that never reach their consumer, which is a different mechanism.

Not renaming it this rotation - the name is load-bearing in the journal and in
two commits, and a rename with no behaviour change is churn better done in one
pass at the end. Recorded so the message is not mistaken for evidence of a
branch.

### The 17 survivors are three families, not one
    A. TOP-LEVEL DROPPED COMPUTATION (859 x2, and probably 297, 445, 646, 958)
       computed, never read, no branch involved. Real, and mechanism-less so far.
    B. CONFIRMED CHECKER FALSE POSITIVES (885_Easing x3)
       `local pow31 = arg4 / 2` duplicates the `d/2` already in the condition
       above it - nothing is lost. Established in rotation 30.
    C. PARAMETER-EXTENDING (21, 22, 381 - `v34 <- arg1`, `v33 <- arg1`)
       still unadjudicated; a dead local in the ORIGINAL source is
       indistinguishable from a dropped one in the emitted text.

### State after rotation 37
SEMANTIC: 1,122/1,142 (98.2%), 27 defects. Composition now known:
    17 computed_value_discarded  = 3 proven FP + ~6 top-level real + ~8 unadjudicated
    10 declared_never_assigned   = the register-value-lost family, of which
       53_PathDisplay is mechanism B (missing else arm, and `v6` carrying BOTH
       the pool counter and a Ray - two live variables on one name)
  A, D, E, F, G, H, I, J, A2/C CLOSED. MINUS closed as a measured dead end.
  B PARKED (1). K part-adjudicated.
SWEEP: run against run_r32 still in flight; r32 predates rotation 35.


## Rotation 38 - the argument count was capped at ten, and CFrame.new takes twelve

### Found while reading 859_StickerPlacer
`PlaceSticker` computes two normalised coordinates and never uses them, so I
looked at the call that should receive them - and found the same guessed
constant rotation 18 removed from the RESULT side, still present on the
ARGUMENT side, in both paths:

    // Cap non-vararg argument count - Luau functions rarely have >10 args.
    let capped_nargs = nargs.min(10);
    ...
    for i in 0..max_args.min(10) {          // and again, on top of a frame bound

### Ground truth settles it
`CFrame.new` with a full rotation matrix takes **exactly 12** arguments
(x, y, z, R00..R22). `893_Utils_Matrix3D` was emitting ten:

    before  CFrame.new(arg1[13], arg1[14], arg1[15], arg1[1], ..., arg1[9])
    after   CFrame.new(arg1[13], arg1[14], arg1[15], arg1[1], ..., arg1[9],
                       arg1[10], arg1[11])

Truncated at precisely the cap. `910_Utils_TriangleMaker` the same, twice.

Fixed the same way as rotation 18: the bytecode bounds it exactly, since
arguments occupy R(A+1)..R(A+nargs) and the proto declares its frame. A count
that does not fit is impossible; one that fits is a real call at any size. The
vararg path's `max_args` was ALREADY the frame bound, so its `.min(10)` was a
second guess stacked on an exact limit.

### Measurement - clean run, empty out-dir, 1142/1142 decodable
    1,122 clean (98.2%), 27 defects - UNCHANGED

5 files changed and all five are improvements:
    893_Matrix3D      CFrame.new 10 -> 12 args
    910_TriangleMaker CFrame.new 10 -> 12 args, twice
    177_Beamstorm     `if nil then` -> `if attachment then`,
                      `GetTags(nil)` -> `GetTags(attachment)`
    201_PollenBomb, 902_Reflect

943 tests pass; only the two pre-existing failures.

### The measure did not move, and that is the finding
The semantic checker has NO CHECK for argument count, so a call silently missing
two arguments scores clean. This is the fourth blind spot found this session
(the others: discarded branch values, value collapse, and argument counts now).
**The 98.2% is an upper bound on correctness, not a measurement of it.**

The honest reading of "semantic 1,142/1,142" as a target: it is reachable only
for the defect classes the checker can see, and this rotation fixed real,
externally-verifiable output damage while the number stood still.

### State after rotation 38
SEMANTIC: 1,122/1,142 (98.2%), 27 defects (24 real).
  A, D, E, F, G, H, I, J, A2/C CLOSED. **argument-cap CLOSED.** MINUS closed as
  a measured dead end. B PARKED (1). K part-adjudicated.
POSSIBLE CHECK, noted not built: a call whose argument count is impossible for a
  known Roblox constructor (`CFrame.new` accepts 1, 3, 7 or 12). That is
  externally-groundable and would have caught this without the corpus read.
SWEEP: run against run_r32 in flight.


## Rotation 39 - the 27 fully classified, and B's footprint is one file

### B does not generalise
Checked the other `declared_never_assigned` survivors for the missing-else
shape. None of them have it:

    79_Activatables_Toys   v7 in a concat AND returned  -> a lost STRING
    85_Badges              `if getTask5.Amount < v13`   -> a lost NUMBER
    368_HoneycombTools     `v19.Tastes.Treats`          -> a lost OBJECT

`53_PathDisplay` appears to be the only missing-else file in the corpus, so B is
a one-file mechanism. It stays PARKED (1) and is now correctly sized: fixing it
would move the measure by one file, and the journal's earlier note that a naive
fix cost 13 files means the trade is still bad.

### Final classification of the 10 declared_never_assigned
    UNK_CALL_CLEAR       5   533, 22, 53, 368, 85
    METHOD_RECV_REJECT   2   54, 21   (symptoms - the receiver really is Nil)
    UNK_MERGE_COMPOUND   1   79
    UPVAL_NAME           1   873_Translation  <- NEW mechanism

`873_Translation` emits `GetTranslator3:Translate(upval_2, arg1)` where
`upval_2` is a captured upvalue that nothing assigns. That is the capture family
`free_var_decls` was written for, and it runs into a KNOWN structural limit
recorded earlier in this project: CAPTURE is never detected on Roblox bytecode
because `child_protos` is always empty. So the upvalue's identity is not
recoverable from this input at all - the name is a placeholder for information
the decode does not contain.

### And the 17 computed_value_discarded
    proven checker false positives   3   885_Easing (rotation 30)
    top-level dropped computation    ~6  859 x2, 297, 445, 646, 958
    parameter-extending, unadjudicated ~8  21, 22, 381 ...

### What the remaining 24 actually are
Every one is a register whose VALUE was lost upstream - at a CALL boundary, at a
merge, or at a capture the format does not record. Rotation 35 solved the case
where the lost value is a TABLE, because a table literal is a pure allocation
and can be materialised before the branch. The rest cannot use that fix: moving
a `Call` or an arbitrary expression across a branch changes when its side
effects run.

That is not a reason to stop, but it is a reason to state the shape plainly: the
remaining work is upstream value tracking, not naming or emission, and several
cases (the capture one certainly) are information the input does not carry.

### State after rotation 39
SEMANTIC: 1,122/1,142 (98.2%), 27 defects (24 real after the 3 proven FPs).
  CLOSED: A, D, E, F, G, H, I, J, A2/C, argument-cap. MINUS: measured dead end.
  PARKED: B (1, one file, correctly sized). K part-adjudicated.
  NEW: UPVAL_NAME - blocked on a format limitation, not a lifter bug.
SWEEP: run against run_r32 at ~904/1111.


## Rotation 40 - the MULTRET clear re-measured on the right corpus, and kept

### The experiment the code invited
The CALL handler's comment records that skipping the register clear for MULTRET
was measured on CoreGui (1699/303 -> 1700/305) and rejected. That is a DIFFERENT
corpus, and `UNK_CALL_CLEAR` is the largest remaining tag here (5 of 10), so it
was worth re-running on full1143.

### MEASURED: identical score, 167 files changed, and the changes are harmful
    1,122 clean (98.2%), 27 defects - EXACTLY unchanged
    167 files differ

Reading them is what decides it. `1022_Bronze_Star_Amulet_Generator_ServerEffect`:

    before   {
                 Generate(Value, value4, true)      -- multret expands in tail position
             }
    after    local Generate2 = Generate(Value, value4, true)
             {
                 Generate2,
                 Value,
                 value4,
                 true                               -- the call's own ARGUMENTS
             }

The table now collects the call's ARGUMENT registers alongside its result. That
is precisely the stale-value leak the clear exists to prevent: SETLIST scans
upward and finds the argument registers still populated.

`533_NPCs` shows the seductive half of the same effect - `value7(Make3)` becomes
`value7(Make3, "Brown Bear")`, which READS like a correct registration call and
is indistinguishable, from the emitted text alone, from a leak of the adjacent
register. Given 1022 proves leakage is happening, the plausible-looking ones
cannot be trusted either.

REVERTED. The CoreGui verdict holds on this corpus, and now with a concrete
example of the mechanism rather than a score comparison.

### Why the score could not settle this
The count was IDENTICAL while 167 files changed, one of them provably for the
worse. A measure that cannot distinguish 167 changed files from zero is not
adjudicating this question, which is the same lesson as rotation 38 (a real
fix the measure could not see) arriving from the opposite direction.

### State after rotation 40
SEMANTIC: 1,122/1,142 (98.2%), 27 defects (24 real).
  CLOSED: A, D, E, F, G, H, I, J, A2/C, argument-cap.
  MEASURED DEAD ENDS: MINUS-as-operator; MULTRET-clear-skip (now on both
  corpora, with a mechanism).
  PARKED: B (1, one file). K part-adjudicated. UPVAL_NAME blocked on the
  format's missing capture information.
SWEEP: run against run_r32 nearly complete.


## Rotation 41 - the sweep on run_r32, and what it cannot prove

### The numbers
    SWEEP vs run_r32 (1,111 files)
      clean            779
      suspect          168
      broken           161
      error              3
      clean-or-suspect 947

    vs the run_r27 sweep: 1,009 of 1,111 files agree (90.8%)
    broken in BOTH the r27 and r32 sweeps: 137

### What this does NOT establish
`run_r32` carries rotations 28 and 31, which `run_r27` does not, so a real
improvement should show. It does not, and it cannot:

  * two sweeps of the SAME bytes agreed on only 87.0% of files (rotation 32),
    so the 90.8% here is INSIDE the instrument's own jitter;
  * confirmed-broken went 147 (r18 n r27, identical builds) -> 137 (r27 n r32,
    improved build) - a 10-file move against a band where 144 files flip on
    identical input.

So: the sweep cannot resolve the effect of this session's fixes. Not "the fixes
did not help" - the instrument is too noisy to say either way at this effect
size. Stating it as an improvement would be exactly the overclaim rotation 32
was written to prevent.

### What would settle it
N-run voting per build. Three runs per build over 1,111 files is ~3.5 hours at
the measured 16-33 files/min, and the confirmed set is the intersection. The
single-run numbers in this journal - including 793/1,142 in the original goal -
should all be read with the +/-13% band attached.

### The two measures now say different things, on purpose
    SEMANTIC (deterministic)  1,122/1,142 (98.2%), 27 defects, repeatable
                              exactly, and blind to at least four defect
                              classes found this session.
    SWEEP (stochastic)        947/1,111 clean-or-suspect, 13% per-run jitter,
                              sees classes the checker cannot, cannot resolve
                              small deltas.

Neither is the truth on its own. The semantic checker under-reports because its
checks are narrow; the sweep over-reports because it is unstable. Keeping both,
and never letting either alone adjudicate, is the working conclusion of this
session.

### State after rotation 41
SEMANTIC: 1,122/1,142 (98.2%), 27 defects, 24 real.
  CLOSED: A, D, E, F, G, H, I, J, A2/C, argument-cap.
  MEASURED DEAD ENDS: MINUS-as-operator, MULTRET-clear-skip.
  PARKED: B (1, one file). K part-adjudicated. UPVAL_NAME blocked on the
  format's missing capture data.
SWEEP: 137 files broken across two consecutive builds; adjudication of those
  is the next block of work, and it needs the N-run vote to be trustworthy.


## Rotation 42 - a latent defect I introduced in rotation 28, found by chasing another

### Attempt: preserve declared locals across the CALL clear. REVERTED.
The clear wipes `clear_start..regs.len()` unconditionally. A register already
bound to a DECLARED LOCAL is not call scratch, so skipping those looked
principled - and measured well: **1,122 -> 1,125 clean (98.5%), 27 -> 22**.

Reading the 232 changed files killed it. `861_StickerPrinterAnimator`:

    before   IsAnimating = StickerRoll.PrimaryPart.CFrame
             v2 = PrintModel.PrimaryPart.CFrame
    after    i = StickerRoll.PrimaryPart.CFrame     <- `i` is the FOR VARIABLE
             IsAnimating = i
             i = PrintModel.PrimaryPart
             i = i.CFrame

Keeping the binding alive makes later temporaries write THROUGH that local's
name, so scratch values are assigned over a live loop variable. Reverted; a note
left at the clear so this is not retried.

Fifth time this session that a score improvement turned out to be the instrument
losing sight of damage.

### But the chase found something real - in MY OWN rotation-28 rule
The same run introduced 11 new parameter assignments, including

    self = self.Name .. " is in a trade"          (356_TradeRequestGui)
    arg3 = (("... " .. arg3.Name) .. " earned the ") .. ...   (388_Sticker)

`self.Name .. "..."` IS a BinOp mentioning `self`, so rotation 28's guard fired -
but `self` appears only UNDER A FIELD ACCESS. The object is still live; the
expression merely reads a field of it. The guard's predicate,
`expr_references_name`, answers "mentioned anywhere", which is too generous.

FIXED with `expr_uses_name_as_operand`, which walks ONLY the BinOp/UnOp spine
and refuses to descend into a field, index or call. A real accumulate has the
name as an OPERAND - `Lvl3 + 2`, `Op .. " ("` - not as the base of a lookup.
Applied to both the rotation-28 guard and the rotation-31 alias arm.

### What that repaired, independent of the reverted experiment
21 files, all the same shape - an object or table being clobbered by a scalar
derived from it:

    144_CentralTime   IsDST3 = result9 + (IsDST3 and ...)      -> local IsDST4
                      a BOOLEAN was being overwritten by a number
    145_ChallengeTools CentralTime2 = (... CentralTime2[arg2]) .. ...
                      -> local CentralTime22; a TABLE was being overwritten by
                      a string
    276_CubMenu       GetCubInfo2 = GetCubInfo2 and 0 < #GetCubInfo2.Skins
                      -> local GetCubInfo4; an OBJECT overwritten by a boolean

These were REAL defects that rotations 28 and 31 introduced and that no measure
reported. The semantic score is IDENTICAL before and after - 1,122/27 - so the
checker is blind to object-clobbering too. That is the fifth blind spot found.

### Measurement - clean run, empty out-dir, 1142/1142 decodable
    1,122/1,142 (98.2%), 27 defects - unchanged, 21 files repaired
    943 tests pass; the two pre-existing opmap failures remain

### State after rotation 42
SEMANTIC: 1,122/1,142 (98.2%), 27 defects (24 real).
  CLOSED: A, D, E, F, G, H, I (now with a corrected predicate), J (same),
  A2/C, argument-cap.
  MEASURED DEAD ENDS: MINUS-as-operator, MULTRET-clear-skip,
  preserve-declared-locals-across-the-clear.
  PARKED: B (1, one file). K part-adjudicated. UPVAL_NAME format-blocked.
SWEEP: run 1 of 2 against run_r37 in flight - NOTE it predates this rotation's
  21-file repair, so a third run against the final build will be needed for the
  intersection to describe the shipped state.


## Rotation 43 - K adjudicated in full: 8 of 17 were the checker's fault

### The remaining candidates, read by hand
All three unresolved cases turned out to be one shape:

    21_ClickToMove   local v34 = arg1 + 0.1
                     if arg2 < arg1 + 0.1 then          -- recomputed inline
    381_Eggs         local v7 = arg1 + arg2
                     for i = arg1, arg1 + arg2 - 1 do
    919_pprint       local found8 = found7 + len4
                     if make_option2.level_width < found7 + len4 then

The emitter materialises a dead copy and the use site recomputes the expression.
NOTHING IS LOST. Together with `885_Easing` x3 from rotation 30 that is SIX
hand-confirmed false positives, which is the evidence rotation 30 lacked when it
refused to add this rule on a bare substring test.

### Condition 4 added: not simply recomputed
`check_branch_local_discarded` claims "a value the bytecode carried was
dropped". If the identical expression is evaluated again elsewhere, that premise
is false. Deliberately EXACT-text after whitespace normalisation: a looser match
would suppress real losses, and a dropped value that happens to share its
spelling with a live computation is a case worth MISSING rather than one worth
INVENTING.

### Measurement - clean run, empty out-dir, 1142/1142 decodable
    1,122 -> **1,126 clean (98.6%)**, 27 -> **19 defects**

MUTATION TEST, and it is exact:
  * decompiler output changed on **0 files** - this is a checker-only change,
    verified by diffing the trees rather than assumed;
  * all six confirmed false positives disappeared;
  * `859_StickerPlacer`'s `Size` and `Size2` SURVIVE - their expression
    (`arg3 + arg2.Size.X / 2`) appears nowhere else, so the computation really
    is discarded. That is the case condition 4 must not swallow, and it did not.
  * two unit tests pin both directions; 945 passing.

### K is now fully adjudicated
    17 computed_value_discarded
      8 FALSE POSITIVE  - recomputed elsewhere, checker corrected, rule recorded
      9 REAL            - 859 x2, 297, 445, 646, 958, 263, 317, 21(new11)

No finding was silenced: the eight were each shown to have their value present
elsewhere in the same file before the rule was written.

### State after rotation 43
SEMANTIC: **1,126/1,142 (98.6%), 19 defects** - and unlike earlier figures this
  one has no known false positives left in it. 19 real.
    9 computed_value_discarded (real)
    10 declared_never_assigned (5 UNK_CALL_CLEAR, 2 METHOD_RECV_REJECT,
       1 UNK_MERGE_COMPOUND, 1 UPVAL_NAME format-blocked, 1 other)
  CLOSED: A, D, E, F, G, H, I, J, A2/C, argument-cap, **K**.
  MEASURED DEAD ENDS: MINUS-as-operator, MULTRET-clear-skip,
  preserve-declared-locals.
  PARKED: B (1, one file). UPVAL_NAME format-blocked.
SWEEP: run against run_r37 in flight.


## Rotation 44 - the CALL clear resists a THIRD preservation strategy

### The reasoning that looked sound
Rotation 42 showed the VALUE preservation was right (85_Badges' `v13` correctly
became `LastTier + 1`) while the NAME staying bound caused the harm (861 wrote
temporaries through its own `for` variable). Those separate cleanly, so:
preserve only PURE, NON-`Name` expressions - arithmetic, literals, field chains
- and never a bare `Name` or `LoopVar`.

Measured **1,126 -> 1,127 clean (98.7%), 19 -> 18**, and `85_Badges` gained a
real value: `local GetStat4` became `local GetStat4 = v42 + 1`.

### Reverted: 321 files changed and 861 broke a different way
    before   local Animate2
             Animate2 = tbl.Animate
             Animate2(arg1, arg2, arg3)
    after    local tbl2 = task.wait      <- stale value survived the call
             tbl2 = tbl
             tbl2 = tbl2.Animate         <- one name, three different things
             tbl2(arg1, arg2, arg3)

`task.wait` is `Field { object: Name("task") }`, which my purity predicate
accepted, so it survived the clear and polluted the register downstream. The
predicate allowed `Name` INSIDE a compound while forbidding a bare one, and a
field chain rooted at a name is exactly the case that distinction fails to
exclude.

### Three strategies, three different failures - the pattern is the point
    rotation 40  skip the clear for MULTRET
                 -> argument registers leak into a following SETLIST
                    (1022_Amulet_Generator)
    rotation 42  keep registers holding a DECLARED LOCAL
                 -> temporaries write through a live loop variable (861)
    rotation 44  keep registers holding a PURE NON-NAME value
                 -> stale field chains survive and are threaded through
                    later scratch (861 again, differently)

Every one raised the semantic score. Every one damaged output the score cannot
see. The CALL clear is load-bearing for the correctness of emitted NAMES, not
just values, and any relaxation lets a stale register participate in later
naming decisions.

### UNK_CALL_CLEAR: PARKED (3) - and the attack must change
Per the escalation rule this is now parked three times, so the next attempt must
not be another preservation variant. What is ruled out is above, each with the
file that killed it. The unblocking question has moved:

  NOT "which registers can survive the clear" - three answers tried, all wrong.
  BUT "why does a register the source still needs live ABOVE a call's base at
  all?" A Luau call frame is allocated above every live local, so a live value
  sitting above `A` means either the call's `A` is decoded too low, or the value
  was placed there by an earlier instruction the lifter mis-assigned. That is
  checkable on ONE file (85_Badges, register 13, call at A=4) by dumping the
  instruction that last wrote register 13 before the call - and it decides
  whether this is a clear problem at all, or an opcode-decode problem wearing a
  clear problem's clothes.

### State after rotation 44
SEMANTIC: **1,126/1,142 (98.6%), 19 defects**, no known false positives among
  them.
  CLOSED: A, D, E, F, G, H, I, J, A2/C, argument-cap, K.
  MEASURED DEAD ENDS: MINUS-as-operator; MULTRET-clear-skip;
    preserve-declared-locals; preserve-pure-values.
  PARKED: UNK_CALL_CLEAR (3, attack must change, question restated above);
    B (1, one file); UPVAL_NAME (format-blocked - Roblox bytecode carries no
    CAPTURE, so the identity is not in the input).
SWEEP: run against run_r37 in flight.


## Rotation 45 - the attack changed, and it dissolved the framing

### The check the escalation rule called for
Parked three times, so no fourth preservation variant. Instead: instrument the
clear to report whether any DECLARED register sits above the call base, since a
Luau frame is allocated above every live local and such a register should be
impossible.

`85_Badges`:

    FRAME pc=436 A=12 clear_start=13 maxstack=19 highest_declared=13
          DECLARED_ABOVE=[13]

A call based at 12 with a declared local at 13. In valid Luau the compiler would
have placed that call at 14.

### But it is not rare, and that is the answer
    85_Badges         66 such calls
    368_HoneycombTools 95
    533_NPCs          118
    22_ClickToMove    213

A systematically low `A` would wreck the entire corpus, not 19 files. So the
contradiction is not a decode fault - it is my premise.

`locals.declared` MIXES real source locals with EMITTER-CREATED TEMPORARIES.
`store_complex` materialises scratch values as named locals by design, so a
register can be genuine call scratch in the bytecode while being a live Lua
local in the emitted text. Frame reasoning applies to the bytecode's locals and
says nothing about the emitter's.

### This retroactively explains rotation 42
"Preserve registers holding a declared local" sounded principled and failed
because `declared` is not the set I thought it was: it includes temporaries
whose registers ARE legitimately scratch, so preserving them kept dead bindings
alive and 861 wrote through its own loop variable. The failure was not a tuning
problem - the predicate was measuring the wrong thing.

### Where UNK_CALL_CLEAR actually stands
The clear is CORRECT. Three preservation strategies failed because none of them
was addressing the defect. A read of register 13 after a call that legitimately
scrubbed it means the WRITE the source performed between the call and the read
was lost - and the surviving candidates for losing it are the merge
(`UNK_MERGE_COMPOUND`, the A2/C family, whose TABLE case rotation 35 fixed) and
the branch structuring.

So `UNK_CALL_CLEAR` is a TAG, not a mechanism - consistent with rotation 20's
finding that `note_unknown` is register-keyed and overwriting, and it should
never have been treated as a cause. That is the second time this session the
tags misdirected me; the corrected rule from rotation 20 ("use the tags to GROUP
candidates, never to name a cause") applies and I did not follow it here.

### UNBLOCKING QUESTION, restated for the last time
For `85_Badges` register 13: which instruction between the call at pc=436 and
the read writes it, and what does the lifter do with that write? The
`LUAU_FRAME_TRACE` instrumentation is left in place (env-gated, zero cost when
unset) because it is the tool the next attempt needs.

### State after rotation 45
SEMANTIC: 1,126/1,142 (98.6%), 19 defects, no known false positives.
  CLOSED: A, D, E, F, G, H, I, J, A2/C, argument-cap, K.
  MEASURED DEAD ENDS: MINUS-as-operator; MULTRET-clear-skip;
    preserve-declared-locals; preserve-pure-values - all four with the file that
    killed them.
  PARKED: UNK_CALL_CLEAR - reclassified from mechanism to TAG, with the real
    question named; B (1, one file); UPVAL_NAME (format-blocked).
SWEEP: run against run_r37 in flight.


## Rotation 46 - the parked question, answered as far as one trace can

### Tooling: LUAU_REG_WATCH
Added a register watch at the DISPATCH BOUNDARY of `lift_instruction_range`, so
it observes writes from every handler rather than one site. `LUAU_REG_WATCH=13`
prints each change to `regs[13]` with the pc that produced it. Env-gated and
verified behaviour-neutral: clean run still 1,126/19, 945 tests pass.

### What it shows at 85_Badges' call
    pc=436  reg13 = Expr(Name("Value2"))   -- a MATERIALISED LOCAL
    pc=437  reg13 = Unknown                -- the call at A=12 scrubbed it
    pc=442  reg13 = Expr(String("+1%"))    -- and it IS written again after

Two things follow, and the second corrects an assumption I was carrying.

FIRST: at the call the register held `Name("Value2")` - a local the emitter had
already declared. So the clear does destroy a NAME BINDING that is still in
scope as a Lua variable, even though the bytecode register is legitimately
scratch. That is the asymmetry rotation 45 identified, now seen directly.

SECOND, and the correction: register 13 IS rewritten immediately afterwards
(`"+1%"` at pc=442) and repeatedly through the proto - the full trace shows it
cycling through tables, strings, numbers and names dozens of times. So the
emitted `v13` is NOT a read of a scrubbed-and-never-rewritten register. Some
LATER read resolves to Unknown, and its own preceding write is the thing that
was lost.

### Why that matters for how this gets attacked next
I had been treating "the call scrubbed it" as the whole story for four
rotations. It is not: the scrub is real, immediately repaired by the next write,
and the defect is a DIFFERENT read further on. Pairing the write trace with a
READ-side trace - which pc resolves `regs[13]` to Unknown, and what wrote it
last before that - is the remaining step, and it is one more instrument away
rather than one more hypothesis away.

### State after rotation 46
SEMANTIC: 1,126/1,142 (98.6%), 19 defects, no known false positives.
  CLOSED: A, D, E, F, G, H, I, J, A2/C, argument-cap, K.
  MEASURED DEAD ENDS: MINUS-as-operator; MULTRET-clear-skip;
    preserve-declared-locals; preserve-pure-values.
  PARKED: UNK_CALL_CLEAR (tag, not mechanism; question narrowed to a read-side
    trace, tooling half-built); B (1); UPVAL_NAME (format-blocked).
TOOLING now in the tree, all env-gated and zero-cost when unset:
    LUAU_MINT_TRACE   which names were minted and at which sites
    LUAU_SEED_TRACE   which gate rejected a table seed
    LUAU_FRAME_TRACE  declared registers above a call base
    LUAU_REG_WATCH    every change to one register, with pc
SWEEP: run against run_r37 in flight.


## Rotation 47 - the read-side trace, and UNK_CALL_CLEAR was never the site

### Tooling completed
Added the READ half to `LUAU_REG_WATCH`, in `reg_expr`'s Unknown arm: it prints
the moment a register is rendered as a minted `vN`. Paired with the write-side
watch, that identifies WHICH read lost its value instead of assuming the nearest
preceding call did - the assumption that cost rotations 40, 42 and 44.

### The result, and it is unambiguous
`85_Badges`, watching register 13:

    WATCH reg=13 before_pc=145 value=Expr(Table { ... })
    WATCH reg=13 before_pc=152 value=Unknown
    READ  reg=13 MINTED v13 (register held Some(Unknown))

    total READs of reg 13 that mint: **1**

ONE read, immediately after pc=152. The call at pc=436 - which
`UNK_CALL_CLEAR` pointed at for four rotations - is 280 instructions LATER and
has nothing to do with it. The tag was attached only because `note_unknown` is
register-keyed and overwriting, so the last call to touch register 13 claimed
the label.

### What it actually is
The register held an `Expr::Table` at pc=145 and was `Unknown` by pc=152. That
is the A2/C shape - a pending table literal destroyed at a control-flow join -
which rotation 35 fixed for the if/else case by materialising before the arms.
This instance survives that fix, so the loss is at a boundary rotation 35 does
not cover (a loop head or a nested region are the obvious candidates).

### The cost of not following my own rule
Rotation 20 recorded: "use the tags to GROUP candidates, never to name a cause."
Rotations 40, 42 and 44 each attacked the CALL CLEAR on the strength of that
tag, and each produced a plausible fix that raised the score and damaged output.
Every one of those was avoidable. The rule is now demonstrated, not just
asserted: the tag was wrong by 280 instructions.

### State after rotation 47
SEMANTIC: 1,126/1,142 (98.6%), 19 defects, no known false positives.
  CLOSED: A, D, E, F, G, H, I, J, A2/C, argument-cap, K.
  MEASURED DEAD ENDS: MINUS-as-operator; MULTRET-clear-skip;
    preserve-declared-locals; preserve-pure-values - all four were chasing a
    mislabelled site.
  **UNK_CALL_CLEAR: DISSOLVED.** It is not a mechanism and not a call problem.
    The 5 files under it fold into A2/C - a value destroyed at a control-flow
    join - and the open question is which join, answerable per-file now that
    both trace halves exist.
  PARKED: B (1, one file); UPVAL_NAME (format-blocked).
Verified behaviour-neutral: clean run 1,126/19, 945 tests pass.
SWEEP: run B against run_r37 in flight; run A gave 949/1,112 clean-or-suspect.


## Rotation 48 - loop joins are NOT where the remaining tables die

### The hypothesis, straight from rotation 47
The read-side trace showed `85_Badges` losing an `Expr::Table` between pc=145
and pc=152 with no branch involved. Rotation 35 materialises pending tables
before if/else arms; loops are the obvious uncovered join. So: extract that
block into `materialise_pending_tables()` and call it at all four loop regions
as well.

### Build note worth recording
Three of the four loop sites had no `pc` in scope, then no `branch_pc`, then
`loop_start` turned out to be declared inside a nested block. During that the
build was RED while a corpus run still reported 1,126/19 - from the STALE
BINARY. Caught by comparing binary mtime against source mtime, which is the
check CLAUDE.md prescribes and which I nearly skipped. A measurement taken while
the build is broken is not a measurement.

### MEASURED after a verified-fresh build: no benefit, some noise
    1,126/1,142 (98.6%), 19 defects - UNCHANGED
    2 files differ

    250_GlitchEffect        + local AnchorPoint22 = {}   (unused, pure noise)
    852_StickerSeekerReward + a 9-field table materialised before the `if`,
                              while the `else` arm REASSIGNS the same table
                              wholesale two lines later - a DUPLICATION, not a
                              recovery

REVERTED. Loop joins do not hold the remaining table losses, and the extraction
would have cost duplicated output for nothing.

### What this rules out, which is the value
A2/C's fix covers if/else. Loops are now excluded by measurement. The `85_Badges`
loss between pc=145 and pc=152 therefore happens at neither - the remaining
candidates are a nested region boundary or the block-level structuring itself,
and the read/write trace pair can localise it per file.

### State after rotation 48
SEMANTIC: 1,126/1,142 (98.6%), 19 defects, no known false positives.
  CLOSED: A, D, E, F, G, H, I, J, A2/C (if/else), argument-cap, K.
  MEASURED DEAD ENDS: MINUS-as-operator; MULTRET-clear-skip;
    preserve-declared-locals; preserve-pure-values; loop-join materialisation.
  PARKED: A2/C-residual (5 files, the ex-UNK_CALL_CLEAR set) - join type
    narrowed to "neither if/else nor loop"; B (1 file); UPVAL_NAME
    (format-blocked).
SWEEP: run B against run_r37 in flight.


## Rotation 49 - CORRECTION to rotation 47, and the real site at last

### Rotation 47's conclusion was wrong
It claimed `85_Badges` lost a table between pc=145 and pc=152. Dumping the
opcode stream there shows nothing of the kind:

    PC 143 SetList     a=13 b=14 c=6     -- fills the table in reg13
    PC 145 SetTableKS  a=13 b=12 c=225   -- STORES reg13's table into reg12
    PC 150 NewTable    a=13 b=0  c=0     -- reg13 = a fresh empty table
    WATCH before_pc=152 value=Table { fields: [] }

The register was never lost: its table was stored into a parent and the next
literal began. At pc=152 the value is `Table {}`, not `Unknown`.

**PC NUMBERING RESTARTS PER PROTO.** The `Unknown` at pc=152 I matched against
this stream came from a DIFFERENT proto. I correlated across protos and drew a
conclusion from it - the same class of error as trusting the register-keyed
tag, and the second time in three rotations that a correlation, not a
measurement, misled me.

Fixed at the source: `LUAU_REG_WATCH` and `LUAU_PC_WINDOW` now print
`proto=<debug_name>@<line_defined>` on every line, so the two cannot be matched
across protos again.

### The real site, in proto `setData@1163`
    WATCH before_pc=121 value=BinOp { left: Name("LastTier"), op: Add,
                                      right: Number(1.0) }
    PC    118 GetUpval a=11 b=5     -- callee into R11
    PC    119 Move     a=12 b=0     -- argument 1
    PC    120 Move     a=13 b=7     -- argument 2  <- `LastTier + 1`
    PC    121 Call     a=11 b=3 c=2 -- f(R12, R13)
    WATCH before_pc=122 value=Unknown

Register 13 holds `LastTier + 1`, is MOVED into an ARGUMENT SLOT, and the call
at pc=121 consumes it. The clear wipes from 12 upward, which is exactly right -
those are argument registers and the VM considers them dead.

The emitted defect is `if getTask5.Amount < v13`, where the source plainly reads
`LastTier + 1`. Rotation 42's experiment produced exactly that string, which is
why it looked like a fix.

### So what the defect actually is
Not a clear bug. Not a merge bug. Something LATER reads register 13 and expects
the value that was consumed as a call argument - a register the VM itself treats
as dead. The lifter is reading a stale register when reconstructing a condition.

That reframes it a third time and finally makes it specific:
`extract_branch_condition` (or whatever reconstructs the `<` comparison here)
resolves an operand from `regs` at a pc where that register no longer holds the
value the ORIGINAL comparison used. The source's `LastTier + 1` was passed to
the call AND used in the comparison; the compiler kept it in R7 (`Move a=13 b=7`
copies FROM R7), so the comparison should read **R7**, not R13.

### UNBLOCKING QUESTION, now concrete enough to act on
Which register does the comparison instruction actually name, and does the
lifter resolve it through the argument copy at R13 instead of the source at R7?
`Move a=13 b=7` is the copy; if the condition extraction follows the
destination rather than the source, that is the bug and it is a one-line
locality.

### State after rotation 49
SEMANTIC: 1,126/1,142 (98.6%), 19 defects. Behaviour-neutral this rotation
  (traces are env-gated); 945 tests pass.
  CLOSED: A, D, E, F, G, H, I, J, A2/C (if/else), argument-cap, K.
  MEASURED DEAD ENDS: MINUS-as-operator; MULTRET-clear-skip;
    preserve-declared-locals; preserve-pure-values; loop-join materialisation.
  PARKED: A2/C-residual - RE-DIAGNOSED as a stale-register read in condition
    reconstruction, with the question above; B (1 file); UPVAL_NAME
    (format-blocked).
TOOLING: LUAU_MINT_TRACE, LUAU_SEED_TRACE, LUAU_FRAME_TRACE, LUAU_REG_WATCH
  (write+read halves), LUAU_PC_WINDOW - all env-gated, all proto-qualified.


## Rotation 50 - the losing instruction identified, and it is a region boundary

### Interleaving the read trace with the opcode stream
Dumping `setData@1163` around the single minting read:

    PC 150 Call  a=16 b=3 c=2      -- 1 result into R16
    PC 151 Move  a=13 b=16         -- R13 = R16
    PC 152 Move  a=8  b=13         -- R8 = R13
    READ reg=13 MINTED v13

The CALL at 150 has base 16 and one result, so its clear starts at 17 and never
touches R13. Rotation 49's hypothesis - that a condition read the argument copy
instead of its source - is WRONG: there is no comparison here at all, just two
MOVEs.

### What the two watches show together
Watching R16:

    WATCH reg=16 before_pc=151 value=Expr(Name("GetStat8"))   -- result bound
    WATCH reg=16 before_pc=152 value=Unknown                  -- SOURCE wiped

Watching R13 (separate deterministic run): the read at 152 finds `Unknown`.

So after pc=151, BOTH the source and the destination read `Unknown`. The `Move`
handler cannot produce that - for a `Name` source it does
`regs[a] = RegVal::Expr(src_expr)` and never touches `regs[b]`. Neither
register's state is consistent with that handler having run.

### The conclusion, and it is a fourth reframing
`lift_instruction_range` runs PER REGION. A state where both registers are
`Unknown` at 152 is what a REGION BOUNDARY produces: 151 and 152 are lifted in
different regions, and the state at 152 arrives through `merge_regs` rather than
from 151's execution. The call result is bound correctly, the MOVE would
propagate it correctly, and the boundary between them discards both.

This is the A2/C family after all - a value destroyed at a control-flow join -
but at a join that is neither an if/else (rotation 35 covers those) nor a loop
(rotation 48 excluded those by measurement). It is a boundary INSIDE a linear
run of instructions, which is the case none of the materialisation work touches.

### UNBLOCKING QUESTION - and it is now a structural one
Why is there a region boundary between pc=151 and pc=152, two consecutive MOVEs
in straight-line code? Either the CFG is splitting a basic block that has no
branch in it, or the region walker is lifting these two instructions under
different parents. Dumping the block/region assignment for
`setData@1163` pcs 145-155 answers it, and it decides whether the fix is in
block construction or in what `merge_regs` does at a boundary with no
divergence.

### State after rotation 50
SEMANTIC: 1,126/1,142 (98.6%), 19 defects. Behaviour-neutral rotation; 945 tests
  pass.
  CLOSED: A, D, E, F, G, H, I, J, A2/C (if/else), argument-cap, K.
  MEASURED DEAD ENDS: MINUS-as-operator; MULTRET-clear-skip;
    preserve-declared-locals; preserve-pure-values; loop-join materialisation.
  PARKED: A2/C-residual - now localised to ONE instruction pair
    (`setData@1163` pc 151/152) with a structural question; B (1 file);
    UPVAL_NAME (format-blocked).

### Honest note on this thread
Four reframings: call clear -> merge -> stale-register read in a condition ->
region boundary in straight-line code. Each was corrected by an instrument, not
by argument, and three of the four corrections came from admitting a
correlation had been mistaken for a measurement. The tooling built along the way
(proto-qualified register watch, read side, pc window) is what makes the current
localisation exact rather than plausible.


## Rotation 51 - A2/C-residual DIAGNOSED: a phi that is confined to empty arms

### The structural question, answered
Added `LUAU_LEADER_TRACE` to `ControlFlowGraph::build`, reporting every block
leader with its proto. For `setData@1163`:

    LEADERS ... 145, **152**, 159, ...

pc=152 IS a block leader. pc=151 is a `Move`, not a branch, so 152 can only be a
leader because something JUMPS to it. It is a merge point, and rotation 50's
inference from the register states was right.

### The complete mechanism, at last
    PC 150 Call a=16 b=3 c=2    -- result bound to Name("GetStat8") in R16
    PC 151 Move a=13 b=16       -- R13 = R16      (fall-through path only)
    -- pc 152 is a JOIN: another edge arrives here without ever executing 151
    PC 152 Move a=8  b=13       -- reads R13 -> merge_regs already reset it

Source shape: `if cond then x = f(...) end` and then a use of `x`. One arm
binds the register; the other does not; `merge_regs` finds the two sides
unequal with neither a `Name` and resets to `Unknown`; the later read mints
`v13`.

### Why the existing machinery misses it
The lifter ALREADY reconstructs a phi for exactly this - "Materialize a phi for
registers that the two arms disagree on" - but its own comment says it is
"deliberately confined to arms that emit no statements: with no statements there
is no side effect and no ordering consequence". Here the arm emits a CALL, so
the phi is skipped and the blunt reset wins.

Rotation 35 solved the same join for TABLES by materialising before the arms,
because a table literal is a pure allocation that can be moved. A call result
cannot be moved. So the fix is not materialisation - it is the B0.57 shape:
declare the binding BEFORE the branch and turn the arm's write into an
assignment, which is exactly what `hoist_escaping_locals` does for
`Stat::Local` nodes whose initialiser is safe to hoist.

### UNBLOCKING QUESTION, now a design choice rather than a mystery
Can the phi reconstruction be extended from "arms with no statements" to "arms
whose disagreement is a single register bound by a statement that STAYS IN THE
ARM"? That needs `local X` emitted before the `if` and the arm's
`local X = f(...)` rewritten to `X = f(...)` - no statement moves, no side
effect is reordered, and the join then has a `Name` on both sides so
`merge_regs` keeps it.

The risk to measure is scope: `hoist_escaping_locals` already does this for safe
initialisers and `docs/MERGE_REGS_SCOPE_RISK.md` records where that goes wrong.
This extension is the unsafe-initialiser case it currently declines.

### State after rotation 51
SEMANTIC: 1,126/1,142 (98.6%), 19 defects. Behaviour-neutral rotation; 945 tests
  pass.
  CLOSED: A, D, E, F, G, H, I, J, A2/C (if/else, tables), argument-cap, K.
  MEASURED DEAD ENDS: MINUS-as-operator; MULTRET-clear-skip;
    preserve-declared-locals; preserve-pure-values; loop-join materialisation.
  PARKED: A2/C-residual - FULLY DIAGNOSED (single-arm binding lost at a join
    because phi reconstruction requires statement-free arms), with a concrete
    fix shape and the named risk; B (1 file); UPVAL_NAME (format-blocked).
TOOLING: LUAU_MINT_TRACE, LUAU_SEED_TRACE, LUAU_FRAME_TRACE, LUAU_REG_WATCH,
  LUAU_PC_WINDOW, LUAU_LEADER_TRACE - all env-gated, all proto-qualified.


## Rotation 52 - the diagnosed fix was already rejected, and now on both corpora

### The code had the answer before I wrote it
Rotation 51's proposed fix - declare `local X` before the branch, leave the
arm's `X = init` in place - is documented verbatim inside
`hoist_escaping_locals`, marked TESTED AND REJECTED:

    "Hoisting the DECLARATION alone ... evaluates once, needs no safety test,
     and would make the binding lexically live at the join -- exactly what
     merge_regs looks for before resetting a register to Unknown. Sound in
     principle; measured on CoreGui it went 1699/303 -> 1674/335 ...
     Declaring more names does not make more of them assigned on every path."

I derived that proposal from first principles over two rotations. It was
already written down, with its measurement, in the function I was about to
change. Reading the target function completely BEFORE designing the fix would
have saved both rotations.

### Re-measured anyway, because the number came from CoreGui
This session has already re-tested two CoreGui verdicts here (MULTRET-clear-skip,
and MINUS via the DB). Implemented behind `LUAU_DECL_HOIST` and run on full1143:

    1,126 clean / 19 defects  ->  **1,120 clean / 25 defects**

Both corpora reject it, and for the stated reason: the extra `local x` converts
an `undefined_register` into a `declared_never_assigned` - a different label on
the same missing value. My own `branch_local_discarded` and
`declared_never_assigned` checks now make that trade visible as a REGRESSION,
where a marker-counting checker might have scored it as an improvement.

Left gated rather than deleted, with the full1143 numbers written into the
comment beside the CoreGui ones, so the next reader reproduces it in one run.
Default behaviour verified unchanged: 1,126/19, 945 tests pass.

### What this settles about A2/C-residual
The single-arm binding lost at a join is now FULLY DIAGNOSED and its two
obvious repairs are both measured and rejected:

    materialise before the arms   works for TABLES (rotation 35, kept),
                                  impossible for a call result - it cannot move
    declare before the arms       rejected on CoreGui AND full1143 - it relabels
                                  the defect instead of fixing it

What remains is the harder version: emit the declaration ONLY when the register
is genuinely assigned on every path reaching the join, which requires knowing
the paths rather than the arms. That is a dataflow question, not a peephole, and
it is the honest boundary of this mechanism.

### State after rotation 52
SEMANTIC: 1,126/1,142 (98.6%), 19 defects, no known false positives.
  CLOSED: A, D, E, F, G, H, I, J, A2/C (if/else, tables), argument-cap, K.
  MEASURED DEAD ENDS (each with the corpus and the file): MINUS-as-operator;
    MULTRET-clear-skip; preserve-declared-locals; preserve-pure-values;
    loop-join materialisation; declaration-only hoisting.
  PARKED: A2/C-residual - diagnosed, both peephole repairs rejected, remaining
    fix identified as requiring path-sensitive dataflow; B (1 file);
    UPVAL_NAME (format-blocked).


## Rotation 53 - the SWEEP measure, answered properly at last

### Two runs over IDENTICAL bytes at the current build (run_r37)
    files                    1,112
    same verdict both runs   1,026   (92.3%)
    CONFIRMED broken         **134**   (broken in BOTH runs)
    broken in EITHER run       197
    never broken               915   (82.3%)

    unreproducible broken verdicts: 63 of 197 - **32%**

### Kinds confirmed by agreement, which is the adjudicated defect map
    undefined_register     61
    truncated_body         42
    name_collision         27
    dropped_call           14
    dead_store             11
    duplicate_call          3

`name_collision` collapses from 352 raw findings (rotation 41) to 27 that
reproduce - the single largest correction the two-run method makes, and it
matches rotation 22's finding that the kind is cosmetic by the prompt's own
definition.

### What the number IS and what it is NOT
It IS: 134 files that two independent passes of the same grader, over the same
bytes, both call broken. That is the honest sweep target, and it is the first
figure in this journal that survives the instrument's own jitter.

It is NOT comparable to the goal's original "793/1,142". That figure was a
single run, and a single run has been measured here at 13% verdict instability
(rotation 32) with a third of its broken verdicts unreproducible. Every
single-run sweep number in this project - including the starting one - carries
that band.

### The three intersections measured this session
    r18 n r27  (identical builds)  147 confirmed broken
    r27 n r32  (r32 improved)      137
    r37 n r37  (same run, 2 passes) 134

The trend is downward but every step is inside the band, so the sweep still
cannot attribute the improvement to specific fixes. What it CAN now do is name
which 134 files to work on, which is what it is for.

### State after rotation 53
SEMANTIC (deterministic): 1,126/1,142 (98.6%), 19 defects, no known false
  positives, exactly repeatable.
SWEEP (stochastic, 2-run vote): 134 confirmed broken of 1,112; 915 never broken.
  Adjudication complete in the sense the goal requires - every finding is
  either REAL (in the 134, with its kind), a PROVEN false positive (K's 8,
  885_Easing's 3, the verdict-rule's 99), or unreproducible and therefore not a
  finding at all.
MECHANISMS: A, D, E, F, G, H, I, J, A2/C(if-else/tables), argument-cap, K CLOSED.
  Six measured dead ends, each with corpus and file.
  PARKED with questions: A2/C-residual (needs path-sensitive dataflow);
  B (1 file); UPVAL_NAME (format-blocked).


## Rotation 54 - all 42 confirmed truncated_body findings are FALSE POSITIVES

### CORRECTS ROTATION 30
Rotation 30 adjudicated this family by the FILE's bytecode size - "all 36 are
>= 300 bytes, so none is the trivial-script case, therefore REAL". That is the
wrong granularity. A 1,263-byte file can contain a one-instruction proto.

### The in-file contrast that exposed it
`889_Utils_Invisibility` ends with a pair of sibling wrappers:

    local function MakeInvisible(arg1)
        makeInvisible(arg1)      -- correct
    end
    local function MakeVisible(arg1)
    end                          -- empty, flagged as truncated

Same construct, same file, one works. The bytecode settles it:

    MakeInvisible@50   GetUpval / Move / Call / Return   (4 instructions)
    MakeVisible@54     Return                            (1 instruction)

There is NO CALL in `MakeVisible`'s proto. The shipped game function is empty -
someone wrote the wrapper and never filled it in. The decompiler is right and
the finding is wrong.

### The sound adjudicator, and it is now a tool
`LUAU_BODY_REPORT` prints, per proto, INSTRUCTIONS against emitted STATEMENTS.
A real truncation is a proto with substantial code and no statements; a
correctly-empty function is a proto with one `Return`.

Applied to all 42 confirmed files:

    protos with >2 instructions emitting 0 statements : **0**
    files where every empty body has <=2 instructions : **42**

Not one real truncation among them. `truncated_body` is a FALSE POSITIVE class
in its entirety on this corpus, and the corrected rule is recorded: adjudicate
an empty body against its PROTO's instruction count, never the file's size.

### What this does to the sweep number
The two-run confirmed set was 134 files. Forty-two of them are confirmed
false positives by mechanical test, so the honest confirmed-broken figure is
**at most 92**, and the remaining kinds still need the same treatment:

    undefined_register  61   the semantic checker's deterministic version of
                             this reports only 10, so most are likely the
                             emitter-noise shape rotation 43 closed
    name_collision      27   cosmetic by the grader's own prompt (rotation 22)
    dropped_call        14   not yet adjudicated
    dead_store          11   real by rotation 23's evidence
    duplicate_call       3   not yet adjudicated

### The pattern across three adjudications now
K: 8 of 17 false positive. The verdict rule: 99 files misgraded. truncated_body:
42 of 42. Every time a defect class has been adjudicated MECHANICALLY against
the bytecode or the emitter's own state, a large fraction turned out not to be
defects. The sweep's raw counts have consistently overstated, and the two-run
vote removes only the UNSTABLE findings - not the STABLE-but-wrong ones. Both
corrections are needed.

### State after rotation 54
SEMANTIC: 1,126/1,142 (98.6%), 19 defects. Behaviour-neutral rotation; 945 tests
  pass.
SWEEP: 134 two-run-confirmed, of which 42 now PROVEN false positive ->
  <= 92 real, with three kinds still to adjudicate the same way.
TOOLING: + LUAU_BODY_REPORT (per-proto instructions vs statements).
MECHANISMS unchanged: A, D, E, F, G, H, I, J, A2/C(if-else/tables),
  argument-cap, K CLOSED; six measured dead ends; A2/C-residual, B and
  UPVAL_NAME parked with questions.


## Rotation 55 - undefined_register is 94% false positive, and the sweep collapses

### The adjudication
Same mechanical method as rotation 54: test the CLAIM against the artefact
rather than reading the grader's word for it. For `undefined_register` the claim
is "a bare `vN` is referenced but never assigned", which is decidable from the
emitted file - with rotation 33's correction that a bare `local x` declaration
line is not a read.

    two-run confirmed undefined_register files : 72
      REAL  (a bare vN read, never assigned)   :  **4**
      FALSE POSITIVE                           : **68**  (94%)

The four are `22_ClickToMove`, `53_PathDisplay`, `54_Controls`,
`79_Activatables_Toys` - every one of them already inside the semantic checker's
independent list of 10. Two instruments built on different principles, agreeing
on the same four files, is the strongest corroboration available here.

### The sweep after two mechanical adjudications
    two-run confirmed broken                      134
    after removing truncated_body (42/42 FP),
    undefined_register (68/72 FP) and the
    prompt-cosmetic name_collision                 **41**

    residual kinds   dropped_call        26
                     dead_store          12
                     duplicate_call       4
                     undefined_register   3

So the sweep's real target is **41 files**, not 134, and not the 349 the goal's
original 793/1,142 implies. Nothing was silenced to get there: every removed
finding was tested against the bytecode or the emitted artefact and shown false,
with the corrected rule written down.

### The pattern, now four for four
    the verdict rule    99 files misgraded          (rotation 22)
    K                    8 of 17 false positive     (rotation 43)
    truncated_body      42 of 42 false positive     (rotation 54)
    undefined_register  68 of 72 false positive     (rotation 55)

Every defect class adjudicated MECHANICALLY has collapsed. The two-run vote
removes UNSTABLE findings; it does nothing about STABLE-BUT-WRONG ones, and
those turn out to be the larger population. A finding that reproduces is not
thereby true - it is merely consistently claimed.

### Still unadjudicated, and they are the honest remainder
`dropped_call` (26) and `duplicate_call` (4) have not been tested. Both are
mechanically decidable the same way: a dropped call means a result is referenced
with no call emitted, which `LUAU_BODY_REPORT` and the register traces can
settle per file. `dead_store` (12) is REAL by rotation 23's evidence - it is the
lost-reassignment shape that mechanism I fixed, so these are the residue that
fix did not reach.

### State after rotation 55
SEMANTIC: 1,126/1,142 (98.6%), 19 defects, no known false positives.
SWEEP: 41 files carrying an unadjudicated or real finding, from 1,112.
  dropped_call and duplicate_call are the next mechanical adjudications.
MECHANISMS: A, D, E, F, G, H, I, J, A2/C(if-else/tables), argument-cap, K
  CLOSED. Six measured dead ends. A2/C-residual, B, UPVAL_NAME parked with
  questions.


## Rotation 56 - dropped_call adjudicated: 7 real, 7 false, and the bias is stated

### The test
`LUAU_BODY_REPORT` now also counts CALL and NAMECALL opcodes per proto, so the
`dropped_call` claim - "a call whose result is referenced but the call itself is
missing" - can be tested by comparing the calls the BYTECODE contains against
the calls the OUTPUT emits.

    CONFIRMED dropped_call files                 14
      bytecode calls > emitted  (likely REAL)     7   deficits 3 to 21
      emitted >= bytecode       (FALSE POSITIVE)  7

    359_WindShrineGui   108 -> 87   deficit 21
    240_Events          103 -> 83   deficit 20
    254_BQInspector      72 -> 56   deficit 16
    354_TradableGrid    109 -> 98   deficit 11
    218_Collectors       44 -> 36   deficit 8
    356_TradeRequestGui 172 -> 167  deficit 5
    413_BeamShot         25 -> 22   deficit 3

### The bias, stated rather than hidden
The emitted-call count is a REGEX over the output text and it demonstrably
OVER-counts: `698_Use_Toy` reads bytecode=3, emitted=9, and
`850_Statistics` 29 against 48. Function definitions, table constructors and
parenthesised expressions all match it.

That bias runs in ONE direction, and it makes the result conservative: a file
where the bytecode still exceeds the output DESPITE over-counting the output has
a real deficit. So the 7 are strong; the 7 "false positives" are weaker and
should be read as "not demonstrated", not "disproven".

This is the first adjudication this session where the instrument's error
direction matters to the conclusion, so it is recorded with the conclusion
rather than left implicit.

### Sweep standing after four adjudicated kinds
    two-run confirmed broken                     134
    truncated_body       42 of 42 FALSE          (rotation 54)
    undefined_register   68 of 72 FALSE          (rotation 55)
    name_collision       cosmetic by the prompt  (rotation 22)
    dropped_call          7 of 14 likely REAL    (this rotation)
    dead_store           REAL by rotation 23's evidence (12 files)
    duplicate_call       4 files, STILL UNTESTED

Honest residual: **~19-23 files** carrying a demonstrated or undisputed defect,
against 1,112 swept. Every removal was tested against the bytecode or the
emitted artefact, and every corrected rule is written down.

### State after rotation 56
SEMANTIC: 1,126/1,142 (98.6%), 19 defects, no known false positives.
  Behaviour-neutral rotation; 945 tests pass.
SWEEP: ~19-23 files with a real or undisputed finding; `duplicate_call` (4) is
  the last untested kind.
MECHANISMS: A, D, E, F, G, H, I, J, A2/C(if-else/tables), argument-cap, K
  CLOSED. Six measured dead ends. A2/C-residual, B, UPVAL_NAME parked with
  questions.
TOOLING: LUAU_MINT_TRACE, LUAU_SEED_TRACE, LUAU_FRAME_TRACE, LUAU_REG_WATCH,
  LUAU_PC_WINDOW, LUAU_LEADER_TRACE, LUAU_BODY_REPORT (insns/stmts/calls).


## Rotation 57 - duplicate_call CANNOT be adjudicated with the instruments here

### The test I built, and why I am not banking its answer
"The same call emitted twice into different locals" looks directly decidable
from the output, so: find identical call text bound to two different locals.

    CONFIRMED duplicate_call files : 5
      matched the pattern          : 2
      did not                      : 3

    267_BeequipReqsBox  L129 Position5 / L139 Position8
                        = UDim2.new(0, result5, 0, result4)
    277_DragManager     L295 Size8 / L344 Size11 / L361 Size12
                        = UDim2.new(0, self.PinAbsSize.X, 0, self.PinAbsSize.Y)

I am NOT recording those 2 as real. The occurrences are 10, 49 and 66 lines
apart - in different branches and different functions - and a GUI script
legitimately builds the same `UDim2` in several places. **The test detects
source repetition and decompiler duplication identically**, so a match is not
evidence for either.

### The alternative test is blocked by a known bias
The sound version is "the bytecode had ONE call and the output emitted TWO",
which needs the call counter from rotation 56 - and that counter's emitted side
is a regex that over-counts (`698_Use_Toy`: bytecode 3, emitted 9). Over-counting
manufactures exactly the signal this test would look for, so it cannot
distinguish a duplicated call from a miscounted one.

### Recorded as UNADJUDICATED, with the blocker named
This is the first kind this session I could not resolve, and saying so is the
honest outcome. It is NOT "no defects found" and NOT "5 false positives" - it is
5 findings the available instruments cannot decide.

UNBLOCKING QUESTION: count emitted calls from the AST rather than the text. The
lifter already returns `Vec<Stat>` per proto in `lift_proto_inner`, so walking
those statements for `Expr::Call`/`Expr::MethodCall` gives an exact count with no
regex bias, and the bytecode-vs-emitted comparison then decides both
`duplicate_call` AND the 7 weak halves of `dropped_call` from rotation 56.
That is a bounded change to `LUAU_BODY_REPORT`.

### Final sweep ledger, all six kinds
    truncated_body      42 of 42   FALSE POSITIVE   (per-proto instruction count)
    undefined_register  68 of 72   FALSE POSITIVE   (bare-vN test, agrees with
                                                     the semantic checker on 4)
    name_collision      cosmetic by the grader's own prompt wording
    dropped_call         7 of 14   REAL (conservative - survives an over-counting
                                   emitted-side)
    dead_store          12 files   REAL by rotation 23's hand evidence
    duplicate_call       5 files   UNADJUDICATED, blocker named above

    honest residual: ~19 demonstrated + 5 undecided, of 1,112 swept

### State after rotation 57
SEMANTIC: 1,126/1,142 (98.6%), 19 defects, no known false positives.
SWEEP: ~19 files with a demonstrated defect, 5 undecided; every other finding
  tested and shown false with its corrected rule recorded.
MECHANISMS: A, D, E, F, G, H, I, J, A2/C(if-else/tables), argument-cap, K
  CLOSED. Six measured dead ends. PARKED with questions: A2/C-residual
  (path-sensitive dataflow), B (1 file), UPVAL_NAME (format-blocked),
  duplicate_call (AST-based call counting).


## Rotation 58 - the exact call counter, and the sweep ledger closes

### The instrument
`count_calls_stats` walks the lifted `Vec<Stat>` and counts `Expr::Call` and
`Expr::MethodCall` nodes. The match is EXHAUSTIVE - no `_` arm - so adding an
AST variant breaks the build rather than silently under-counting, which would
invent `dropped_call` deficits. Closure bodies are not descended into, because
`Expr::Function` is a separate proto reported on its own line.

The compiler immediately earned that choice: `Expr::Ternary`'s field is `cond`,
not `condition`, and an exhaustive match refused to compile until it was right.

Sanity check on the file that exposed the old bias - `698_Use_Toy`:
regex said 9 emitted against 3 in the bytecode; the AST counter says
**bc_calls=3, ast_calls=3**. Exact.

### duplicate_call: 0 REAL, 5 FALSE POSITIVE - closed
No confirmed file emits MORE calls than its bytecode contains. And the two
files rotation 57's TEXT test flagged emit dramatically FEWER:

    267_BeequipReqsBox   bc 82 -> ast 49
    277_DragManager      bc 140 -> ast 90

They are dropped-call files misfiled as duplicates. Rotation 57 declined to bank
that text result on the grounds that it could not tell source repetition from
decompiler duplication; that caution was correct, and the exact counter shows
the finding was not merely undecided but inverted.

### dropped_call: 10 REAL, 4 FALSE POSITIVE - corrects rotation 56
Rotation 56's regex called 7 real. The exact counter finds **10**, adding
`150_ClickManager`, `273_BoostMarketGui` and one more that over-counting had
wrongly cleared. Per-proto deficits are unambiguous:

    254_BQInspector    SetBeequipFile@79   22 calls -> 16
    359_WindShrineGui  SetItem@51          12 -> 8
    356_TradeRequestGui globalTradeEnabled@37 4 -> 2
    218_Collectors     collectStart@48      6 -> 4

### The sweep ledger, complete and evidenced
    truncated_body      42 of 42  FALSE   per-proto instruction count
    undefined_register  68 of 72  FALSE   bare-vN test; the 4 real agree with
                                          the semantic checker exactly
    name_collision      cosmetic by the grader's own prompt wording
    duplicate_call       5 of 5   FALSE   exact AST vs bytecode call counts
    dropped_call        10 of 14  REAL    exact counts, per-proto deficits
    dead_store          12 files  REAL    rotation 23 hand evidence

    **~22 files carry a demonstrated defect, of 1,112 swept.**

Every one of the six kinds is now adjudicated with a mechanical test against the
bytecode or the lifter's own output. No finding was suppressed; each false
positive was shown false and its corrected rule written down.

### What the whole sweep exercise established
The grader's raw output was 246 broken files. Two-run agreement cut that to 134
by removing UNSTABLE findings. Mechanical adjudication cut it to ~22 by removing
STABLE-BUT-WRONG ones. The second correction is five times larger than the
first, and only the second required looking at the artefact instead of the
verdict.

### State after rotation 58
SEMANTIC: 1,126/1,142 (98.6%), 19 defects, no known false positives.
SWEEP: ~22 files with a demonstrated defect; all six kinds adjudicated.
  The 10 `dropped_call` files are now a bounded, evidenced fix target with
  exact per-proto deficits naming where to look.
MECHANISMS: A, D, E, F, G, H, I, J, A2/C(if-else/tables), argument-cap, K
  CLOSED. Six measured dead ends. PARKED with questions: A2/C-residual
  (path-sensitive dataflow), B (1 file), UPVAL_NAME (format-blocked).
Behaviour-neutral rotation; 945 tests pass.


## Rotation 59 - CORRECTION: my call counter double-counted every method call

### The bug, found by picking the smallest deficit
`218_Collectors::getSwingSpeedMultiplier@37` reported bc_calls=2 against
ast_calls=1. Its entire body is

    local function getSwingSpeedMultiplier()
        return value3:Get().Transient.CollectorSpeed or 1
    end

One method call, correctly emitted. In Luau `obj:Method()` compiles to NAMECALL
FOLLOWED BY CALL - two instructions for ONE call expression - and rotation 58's
counter counted both. Every method call inflated the bytecode side by one and
MANUFACTURED a deficit.

Fixed: count only `LuauOpcode::Call`. Every call, plain or method, terminates in
exactly one CALL, so that is the right unit to compare against AST call nodes.
After the fix all six of 218's protos match exactly: 1/1, 4/4, 1/1, 3/3, 10/10.

### Rotation 58's dropped_call result was WRONG
    before (double-counted)   10 REAL of 14
    after  (corrected)         **1 REAL of 14**

The files I listed last rotation with "unambiguous per-proto deficits" -
`254_BQInspector` 22->16, `359_WindShrineGui` 12->8, `356_TradeRequestGui`
4->2 - now match EXACTLY. Those deficits were entirely my own miscount. Only
`240_Events` survives, losing one call in `safeInvokeClient@17` (72 vs 71).

`duplicate_call` is unchanged at 0 of 5 real - the corrected count did not
resurrect it - but two of those files show genuine DEFICITS
(`267_BeequipReqsBox` 62->49, `277_DragManager` 105->90), so they are
dropped-call cases the grader misfiled.

### The lesson, and it is the third instance this session
Rotation 56 flagged a regex that OVER-counted the emitted side. Rotation 58
replaced it with an exact AST walker and I treated the result as settled - while
the BYTECODE side still carried a systematic bias I had not tested. I checked
one side of the comparison and not the other.

The check that caught it cost nothing: take the SMALLEST reported deficit and
read the function. A three-line body that "loses" one of two calls is not a
decompiler defect, it is a counting error, and the smallest case makes that
obvious where the large ones look convincing.

Corrected rule recorded: **before believing a comparison, verify BOTH sides
against a case whose answer is known by inspection.**

### The sweep ledger, corrected
    truncated_body      42 of 42  FALSE
    undefined_register  68 of 72  FALSE   (4 real, agreeing with the checker)
    name_collision      cosmetic by the grader's own prompt
    duplicate_call       5 of 5   FALSE
    dropped_call         1 of 14  REAL    (240_Events)
    misfiled deficits    2 files  REAL    (267, 277 - flagged duplicate_call)
    dead_store          12 files  REAL    (rotation 23 hand evidence)

    **~19 files carry a demonstrated defect, of 1,112 swept.**

### State after rotation 59
SEMANTIC: 1,126/1,142 (98.6%), 19 defects, no known false positives.
SWEEP: ~19 demonstrated of 1,112; all six kinds adjudicated with both sides of
  every comparison now verified.
MECHANISMS: A, D, E, F, G, H, I, J, A2/C(if-else/tables), argument-cap, K
  CLOSED. Six measured dead ends. PARKED: A2/C-residual (path-sensitive
  dataflow), B (1 file), UPVAL_NAME (format-blocked).
Behaviour-neutral rotation; 945 tests pass.


## Rotation 60 - mechanism B is 29 files, not one, and no checker sees it

### 240_Events: a dropped block, not a dropped call
`safeInvokeClient@17` loses one call, and the emitted loop shows why:

    local now = os.clock()
    local now2                          -- declared, never used
    while true do
        if 10 >= os.clock() - now then
            continue                    -- under 10s -> SKIP the wait: busy-spin
        end
        task.wait()
    end
    return nil                          -- unreachable

An infinite busy-spin with an unreachable return. The source had a completion
check - `if v3 then return v2 end`, using the flags the spawned closure sets -
and `now2` is the result local it would have written. A whole conditional block
was dropped and the missing call was inside it.

### 267 and 277: whole bodies, not scattered calls
    267_BeequipReqsBox  CreateTypesRow@111   14 bytecode calls ->  1 emitted
    277_DragManager     OnInputBegin@184     15               ->  0

`OnInputBegin` emits its early-return guard and NOTHING ELSE:

    function tbl:OnInputBegin(arg2)
        local Size2 = {}
        if not self.Pulling then
            ...
            return
        end
    end

Everything after the guard's `return` is gone. That is mechanism B - "RETURN
abandoning later blocks" - which rotation 39 sized as ONE FILE on the strength
of `53_PathDisplay` being the only missing-ELSE case. Wrong sub-shape, wrong
size.

### Sized properly, corpus-wide
Using the corrected call counter across all 1,143 files:

    protos emitting ZERO calls where bytecode has >=3 :  **8**
      99_BeequipTypes    _CheckReqs@5246          20 calls -> 0
      277_DragManager    OnInputBegin@184         15 -> 0
      256_BadgeMenu      privateInstanceUpdate@366 12 -> 0
      430/453/469        GrowStar@...              5 -> 0  (three sibling files)
      438_LocalFlames    DestroyFlame@525          3 -> 0
      238_EggTypes       OnGive@3476               3 -> 0

    protos emitting under HALF their calls (bc>=4)    : **27**
      314_RoboBearGui    InitInfoBox@1411         20 -> 3
      267_BeequipReqsBox CreateTypesRow@111       14 -> 1
      25_RootCamera      ApplyVRTransform@554      9 -> 2

    **files affected: 29 of 1,143**

### The sixth blind spot, and the biggest
The semantic checker reports NONE of these. It sits at 19 defects and not one of
those 29 files is among them, because a dropped body leaves no undefined name,
no bad call and no discarded value - nothing any existing check looks for. The
sweep only caught 2 of the 29, and misfiled one of them as `duplicate_call`.

Blind spots found this session: discarded branch values, value collapse,
argument counts, the branch check's naming, object-clobbering, and now DROPPED
FUNCTION BODIES - which is the largest of them by damage per file. A function
that silently does nothing is worse output than one with an undeclared name in
it, and it scores clean on both instruments.

### The instrument that found it is the contribution
`bc_calls` vs `ast_calls` per proto needs no ground truth, no model and no
judgement: the bytecode says how many calls a function makes, the AST says how
many were emitted, and a function that makes twenty calls and emits none is
broken by inspection. That check is worth having permanently, and it is the
natural next semantic check - it would have found this class without the sweep,
the two-run vote, or six rotations of adjudication.

### State after rotation 60
SEMANTIC: 1,126/1,142 (98.6%), 19 defects by the CURRENT checks - which are now
  known to miss a 29-file class. The honest reading is 1,126 minus up to 29.
SWEEP: ~19 demonstrated of 1,112, all six kinds adjudicated.
MECHANISMS: A, D, E, F, G, H, I, J, A2/C, argument-cap, K CLOSED.
  **B RE-OPENED and RESIZED: 29 files, 8 total body drops, reproducer
  `277_DragManager::OnInputBegin@184` (15 calls -> 0) and three sibling
  `GrowStar` files that should decompile identically.**
  Six measured dead ends. PARKED: A2/C-residual, UPVAL_NAME.
Behaviour-neutral rotation; 945 tests pass.


## Rotation 61 - `calls_not_emitted`: the measure can finally see a dropped body

### The seam
The checker works on source TEXT and has no bytecode, but the CLI already reads
`Protos: N total` out of the emitted header - so bytecode facts reach it through
a comment. `call_recovery` accumulates per-chunk counts during lifting (the one
place holding both the bytecode and the AST), `lib.rs` writes

    -- call recovery: 1 proto(s) emitted 15 fewer call(s) than the bytecode contains

and `check_call_recovery` reads it back. Reset per chunk, or a batch run has
every file after the first reporting the corpus.

### THRESHOLDED to what was verified, and the gap recorded
Any-deficit reporting flagged **95 files**. The shape actually demonstrated by
inspection is 29. The difference is small shortfalls in large functions -
`23_Invisicam::Update@395` emits 20 of 21 - which is far likelier a counting
edge (dead code, a fused builtin, an unreachable path) than a dropped body, and
I have NOT shown otherwise.

Reporting 95 would assert five times the evidence I hold. The threshold is
`emitted == 0 && bytecode >= 3` or `bytecode >= 4 && emitted*2 < bytecode`.
The excluded cases are UNADJUDICATED, not dismissed - the comment says so and
raising the threshold brings them back.

### Measurement - clean run, empty out-dir, 1142/1142 decodable
    1,126 -> **1,100 clean (96.3%)**, 19 -> **48 defects**

**The number went DOWN by 26 files and that is the point** - the same shape as
rotation 24, where adding `branch_local_discarded` cost 82 files and was the
instrument becoming honest. These 26 files were always broken; nothing looked.

DISCRIMINATION VERIFIED, not assumed:
    277_DragManager     flagged, 15 calls short   (OnInputBegin emits 0 of 15)
    99_BeequipTypes     flagged, 20 short         (_CheckReqs)
    256_BadgeMenu       flagged, 12 short
    23_Invisicam        NOT flagged               (20 of 21 - unverified shape)

Four unit tests pin the reading, including the two silent cases; 949 pass. The
two `opmap_dupclosure_detection` failures are the same pre-existing pair.

### Why this check is worth more than the count it costs
It needs no ground truth, no model and no judgement. The bytecode states how
many calls a function makes; the AST states how many were emitted. A function
that makes twenty and emits none is broken by inspection, and it had been
scoring clean on BOTH instruments for the whole project.

It would have found this class without the sweep, the two-run vote, or six
rotations of adjudication - which is the argument for preferring checks that
compare the artefact against its own input over checks that read the output and
form an opinion.

### State after rotation 61
SEMANTIC: **1,100/1,142 (96.3%), 48 defects** - a wider and more honest
  instrument than the 1,126 it replaces.
    19  the previously-known families
    ~26 newly visible dropped bodies (mechanism B)
SWEEP: ~19 demonstrated of 1,112, all six kinds adjudicated.
MECHANISMS: A, D, E, F, G, H, I, J, A2/C, argument-cap, K CLOSED.
  **B: now MEASURED and VISIBLE** (was invisible to every instrument), with
  reproducer `277_DragManager::OnInputBegin@184` and three sibling `GrowStar`
  files that should decompile identically. NOT yet fixed.
  Six measured dead ends. PARKED: A2/C-residual, UPVAL_NAME.


## Rotation 62 - two causes of B eliminated, and a third found by accident

### Cause 1 ELIMINATED: unreachable blocks
`structure_control_flow` walks `cfg.reverse_postorder()`, so a block absent from
the RPO is unreachable from the entry, gets no region, and its code vanishes.
That is exactly what a dropped body looks like from the structurer, so it was
the first hypothesis.

Added `LUAU_RPO_TRACE`, reporting blocks the structurer will never visit.
`OnInputBegin@184` does NOT appear: all 25 of its blocks are reachable. The
structurer visits them and the code is still not emitted, so the loss is
downstream of region selection.

### Cause 2 ELIMINATED: the statement budget
`MAX_STMTS_PER_PROTO` is 50,000. `OnInputBegin` is a 117-instruction function.
It cannot trip.

Both eliminations cost one build each and remove the two explanations that
would have been most satisfying to find. What remains is the shape the journal
named for B originally - a RETURN abandoning the blocks after it - now with the
two competing hypotheses ruled out rather than merely unlisted.

### Cause 3, FOUND BY THE INSTRUMENT, and separate
Running the RPO trace over the whole corpus: **50 protos contain at least one
block unreachable from their entry.** Those blocks get no region and their code
IS silently dropped - a real defect source, independent of B, that nothing has
ever reported.

`?@323` in `277_DragManager` is the first case: 16 blocks, 15 reachable, block
65 orphaned.

An unreachable block is either genuine dead code the compiler emitted (possible,
and harmless to drop) or a MISSING CFG EDGE (a real defect that silently deletes
live code). Distinguishing them needs the edge builder, not the structurer, and
50 protos is worth that check.

### State after rotation 62
SEMANTIC: 1,100/1,142 (96.3%), 48 defects. Behaviour-neutral rotation; 949 tests
  pass.
MECHANISMS: A, D, E, F, G, H, I, J, A2/C, argument-cap, K CLOSED.
  **B: measured, visible, two causes eliminated.** Reproducer
  `277_DragManager::OnInputBegin@184` (0 of 15 calls, all blocks reachable,
  budget intact). The remaining hypothesis is the RETURN-abandons-later-blocks
  shape, and the next step is to trace which regions the lifter actually
  consumes for that proto against the 25 the structurer produced.
  **NEW: 50 protos with unreachable blocks** - park with the question "is each
  orphan genuine dead code or a missing CFG edge?", answerable in the edge
  builder.
  Six measured dead ends. PARKED: A2/C-residual, UPVAL_NAME.
TOOLING: + LUAU_RPO_TRACE (blocks the structurer will never visit).


## Rotation 63 - B localised to the structurer, and a number I am NOT reporting

### The datum
`LUAU_REGION_TRACE` reports what `structure_control_flow` produced:

    REGIONS proto=OnInputBegin@184 blocks=25 regions=1

Twenty-five blocks, ONE region. Combined with rotation 62 (every block
REACHABLE, budget intact) and rotation 61 (0 of 15 calls emitted), the loss is
inside that single region's construction and lifting - not in reachability, not
in the budget, and not in the lifter's consumption of a region list that never
contained the rest.

That is mechanism B's shape as the journal first named it: the guard's `if`
claims blocks to the end of the proto as `handled`, so the code AFTER the `if`
never gets a region of its own, and the region that swallowed it emits only the
arm.

### The number I am NOT reporting
Corpus-wide, 797 protos have >= 8 blocks and <= 2 regions, with
`GenerateModifiers@403` at 98 blocks in 1 region.

**That is not a defect count and I am not presenting it as one.** Regions NEST:
a single `IfThenElse` legitimately covers dozens of blocks through its arms, so
a low region count is an ordinary structural fact. Reporting 797 would be the
same error as rotation 58's 10 "dropped calls" - a plausible ratio mistaken for
evidence.

The signal that survives scrutiny is the CALL DEFICIT (29 files, verified by
inspection). For `OnInputBegin` the two coincide, and it is the coincidence that
localises the fault, not the region count alone.

### State after rotation 63
SEMANTIC: 1,100/1,142 (96.3%), 48 defects. Behaviour-neutral; 949 tests pass.
MECHANISMS: A, D, E, F, G, H, I, J, A2/C, argument-cap, K CLOSED.
  **B: localised to region construction.** Ruled out across three rotations:
  unreachable blocks (62), statement budget (62), missing regions in the
  lifter's input (63). Reproducer `277_DragManager::OnInputBegin@184` -
  25 blocks, 1 region, 0 of 15 calls.
  UNBLOCKING QUESTION: in `structure_control_flow`'s RPO loop, which blocks does
  the `handled` set absorb when the first `IfThenElse` is built, and does the
  arm ending in RETURN cause the join blocks to be marked handled without ever
  being lifted? That is one trace on the `handled` insertions for this proto.
  **50 protos with unreachable blocks** - separate, parked.
  Six measured dead ends. PARKED: A2/C-residual, UPVAL_NAME.
TOOLING: + LUAU_REGION_TRACE (blocks vs regions produced).


## Rotation 64 - CORRECTS rotation 63: the region HAS the blocks, the lifter drops them

### The trace
`LUAU_IF_TRACE` reports what each `IfThenElse` absorbs:

    IF proto=OnInputBegin@184 cond_block=0 then=1 else=23 merge=None has_else=true

One block in the then-arm, **23 in the else-arm**, and **no merge point**.

### What that overturns
Rotation 63 concluded "the loss is inside region construction - the code after
the `if` never gets a region of its own". That is WRONG in an important way:
`has_else=true` and `else_region` carries all 23 blocks. Structuring is
arguably CORRECT here - with the then-arm returning, the rest of the function
genuinely IS the else arm.

The region holds the blocks and the emitted output has NO `else` BRANCH AT ALL.
So the fault is in the LIFTER's handling of an `IfThenElse` whose `merge_pc` is
`None`, one layer below where I put it last rotation.

Three rotations, three localisations, each corrected by the next instrument:
    62  not unreachability, not the budget
    63  "region construction"          <- too high
    64  the lifter's else-arm handling when merge is None

### A second number I am NOT reporting
Corpus-wide, 935 if/else regions have `merge=None` with >= 5 else-blocks.

Not a defect count, for the same reason as rotation 63's 797: a function whose
arms both RETURN legitimately has no merge point, so the shape is common and
mostly correct. Two rotations running I have produced a large, alarming ratio
and had to decline to report it - the discipline that matters is that the
verified 29-file call deficit stays the measure, and structural ratios are
context for it rather than evidence in their own right.

### UNBLOCKING QUESTION, one layer lower
In the lifter's `Region::IfThenElse` arm, what happens to `else_region` when
`merge_pc` is `None`? The reproducer emits the then-arm and nothing else, so
either the else blocks are never lifted, or they are lifted and discarded. One
trace on that arm distinguishes them, and it is the last question before a fix.

### State after rotation 64
SEMANTIC: 1,100/1,142 (96.3%), 48 defects. Behaviour-neutral; 949 tests pass.
MECHANISMS: A, D, E, F, G, H, I, J, A2/C, argument-cap, K CLOSED.
  **B: localised to the lifter's else-arm handling with merge_pc = None.**
  Ruled out: unreachable blocks, statement budget, missing regions, region
  construction. Reproducer `277_DragManager::OnInputBegin@184` - 25 blocks,
  1 region, else arm holds 23 blocks, 0 of 15 calls emitted.
  50 protos with unreachable blocks - separate, parked.
  Six measured dead ends. PARKED: A2/C-residual, UPVAL_NAME.
TOOLING: + LUAU_IF_TRACE (what each if/else absorbs).


## Rotation 65 - the else arm IS lifted, and the post-passes are exonerated

### Two decisive traces
    IFLIFT proto=OnInputBegin@184 else_blocks=23 else_stmts=Some(37) then_blocks=1

The else arm is passed to `lift_branch_region` and comes back with **37
statements**. The code is recovered. It is not a lifting failure and not an
empty arm.

    PASS proto=OnInputBegin@184 before_post_passes calls=0
    PASS proto=OnInputBegin@184 after_post_passes  calls=0

Calls are ALREADY zero before any post-pass runs, so the whole post-pass stage
is exonerated. I had suspected `collapse_if_assign_bool` after finding it
rewrites `if` into `return`; it only fires when both arms are a single boolean
return, and bracketing the stage settled it in one build rather than by reading
1,900 lines.

### The window, and the two possibilities left inside it
The loss is between `lift_branch_region` returning 37 statements and the end of
the region loop. Everything in that window runs on the arms: phi
reconstruction, `merge_regs`, the B0.116 import-guard propagation,
`hoist_escaping_locals`, and finally the `Stat::If` push.

Exactly two possibilities remain, and they are distinguishable:
  1. the 37 statements contain NO CALLS - the calls were dropped INSIDE
     `lift_branch_region`, and 37 statements of something else survived;
  2. the `Stat::If` never reaches `stmts` - the else body is built and the
     statement discarded.

`count_calls_stats(&else_stmts)` at the IFLIFT site separates them in one build.
That is the next step and it is the last question in this chain.

### Also confirmed: only ONE if/else region for this proto
Checked, because rotation 47's error was attributing a trace across protos and
rotation 49's was reading pc numbers that restart per proto. One `IF` line, one
`IFLIFT` line - the attribution is sound this time.

### State after rotation 65
SEMANTIC: 1,100/1,142 (96.3%), 48 defects. Behaviour-neutral; 949 tests pass.
MECHANISMS: A, D, E, F, G, H, I, J, A2/C, argument-cap, K CLOSED.
  **B: window narrowed to the region loop's arm handling.** Ruled out by
  measurement across four rotations: unreachable blocks, statement budget,
  missing regions, region construction, empty else arm, and the entire
  post-pass stage. Reproducer `277_DragManager::OnInputBegin@184`.
  50 protos with unreachable blocks - separate, parked.
  Six measured dead ends. PARKED: A2/C-residual, UPVAL_NAME.
TOOLING: + LUAU_PASS_TRACE (calls before/after the post-pass stage).


## Rotation 66 - the statement is BUILT COMPLETE and never arrives

### The three traces, in order
    IF     ... cond_block=0 then=1 else=23 merge=None has_else=true
    IFLIFT ... else_stmts=Some(37) else_calls=Some(14) then_stmts=0
    IFPUSH ... then_stmts=0 else_stmts=Some(37) else_calls=Some(14)

The else arm lifts to 37 statements carrying **14 of the function's 15 calls**,
and the `Stat::If` is pushed into `stmts` with that body INTACT. Possibility 1
from rotation 65 (calls dropped inside `lift_branch_region`) is dead: nothing is
lost during lifting at all.

### Where it goes
    PASS proto=OnInputBegin@184 before_post_passes calls=0

`IFPUSH` is in `lift_region` (mod.rs:2156). The `PASS` bracket is in
`lift_proto_inner_impl` (mod.rs:956). They are DIFFERENT FUNCTIONS, and by the
time control reaches the second one the body has zero calls.

So the complete statement that `lift_region` builds does not reach the proto
body. Nothing damages it; it simply never arrives.

`is_empty_if` was the obvious suspect and is INNOCENT - it requires the else
body to be empty as well (`else_body.map_or(true, |b| b.is_empty())`), so a
37-statement else is safe from it. Checked before accusing it.

### What this closes off
Eliminated by measurement across five rotations, each in one build:
    unreachable blocks (62)      statement budget (62)
    missing regions (63)         region construction (64)
    empty else arm (65)          the whole post-pass stage (65)
    loss inside lift_branch_region (66)   is_empty_if (66)

### UNBLOCKING QUESTION - the narrowest yet
`lift_proto_inner_impl` calls `lift_region` for each region. For
`OnInputBegin@184` that call produces an `If` holding 14 calls and the caller
ends up with none. So: which `stmts` vector does `lift_region` push into for
this proto, and why is it discarded rather than appended? One trace on the
CALLER side - counting calls in the caller's accumulator immediately after each
`lift_region` return - answers it, and it is the last question in a chain that
has narrowed for five rotations.

Note the shape: this is the same "an answer is computed correctly and then
discarded at the call site" as mechanism G (rotation 17), where
`classify_write` returned the right `WriteKind` and the caller threw it away.
That precedent is the reason to look at the caller rather than re-examine the
producer.

### State after rotation 66
SEMANTIC: 1,100/1,142 (96.3%), 48 defects. Behaviour-neutral; 949 tests pass.
MECHANISMS: A, D, E, F, G, H, I, J, A2/C, argument-cap, K CLOSED.
  **B: the statement is built complete and lost between `lift_region` and its
  caller.** Reproducer `277_DragManager::OnInputBegin@184`; eight candidates
  eliminated by measurement.
  50 protos with unreachable blocks - separate, parked.
  Six measured dead ends. PARKED: A2/C-residual, UPVAL_NAME.
TOOLING: LUAU_IF_TRACE now reports IF / IFLIFT / IFPUSH with per-arm statement
  and call counts.


## Rotation 67 - MECHANISM B ROOT CAUSE FOUND: eliminate_dead_code

### CORRECTS ROTATION 65
Rotation 65 declared "the post-passes are exonerated" on the strength of a
bracket that turned out to sit AFTER eight of them. Between the region loop and
where I put that trace run:

    simplify_stmts · eliminate_dead_stores · eliminate_dead_code
    eliminate_dead_key_eq_value_locals · eliminate_dead_code
    convert_while_true_break_to_repeat · convert_single_pass_loops
    collapse_elseif_chains

I bracketed the tail of the sequence and reported the whole stage clear. The
lesson is the same one rotation 59 taught about the call counter: verify WHERE
an instrument is placed, not just what it prints.

### The bisect, one trace per pass
    after_region_loop                     calls=14
    after_simplify_stmts                  calls=14
    after_eliminate_dead_stores           calls=14
    after_eliminate_dead_code             calls=**0**   <-- HERE
    ...everything downstream               calls=0

`eliminate_dead_code` deletes the statement. Its own docstring describes the
shape: "Empty if-blocks: `if v9 then end` ... If only some branches are empty,
prune those branches."

The `If` at this point is `if cond then <EMPTY> else <37 statements, 14 calls>
end`. An empty THEN with a populated ELSE is read as an empty if-block and the
whole statement goes, taking the else body with it.

### Why the guard survives in the output
`OnInputBegin` still emits `if not self.Pulling then ... return end`. That is a
NESTED if inside what remains, not the deleted outer one - which is why the
function looks like a plausible early-return guard rather than obviously
truncated, and why every checker scored it clean for the whole project.

### The full elimination record for B - nine candidates, one build each
    unreachable blocks (62)          statement budget (62, and the sentinel
    missing regions (63)              appears in ZERO files corpus-wide)
    region construction (64)         empty else arm (65)
    loss inside lift_branch_region (66)   is_empty_if (66)
    note_stmts_pushed truncation (67)
    -> eliminate_dead_code (67) CONFIRMED by before/after call counts

### THE FIX, stated precisely for the next step
An `If` whose THEN body is empty but whose ELSE body is not is NOT an empty
if-block. The correct rewrite is to INVERT it -
`if not cond then <else body> end` - not to delete it. Deleting is only sound
when EVERY branch is empty, which is what `is_empty_if` already checks correctly
one function away.

Expected effect: the 8 protos emitting zero calls and the 27 emitting under
half should recover; `277_DragManager::OnInputBegin` should regain 15 calls and
37 statements.

### State after rotation 67
SEMANTIC: 1,100/1,142 (96.3%), 48 defects. Behaviour-neutral; 949 tests pass.
MECHANISMS: A, D, E, F, G, H, I, J, A2/C, argument-cap, K CLOSED.
  **B: ROOT CAUSE FOUND** - `eliminate_dead_code` deletes an if with an empty
  then-arm and a populated else. Fix identified (invert rather than delete),
  not yet applied.
  50 protos with unreachable blocks - separate, parked.
  Six measured dead ends. PARKED: A2/C-residual, UPVAL_NAME.


## Rotation 68 - CORRECTS ROTATION 67's diagnosis, and the fix was already there

### Rotation 67 over-claimed twice
It said `eliminate_dead_code` "deletes an if with an empty then-arm and a
populated else", and proposed inverting instead of deleting. Reading the pass
properly:

  1. `is_empty_if` (Phase 3) requires the ELSE to be empty as well, so it cannot
     remove our statement.
  2. **Phase 4 ALREADY performs exactly the inversion I proposed** -
     `then_body.is_empty() && !elseif && else non-empty` negates the condition
     and swaps the body in.

So the fix I "identified" was in the tree already, and the mechanism I named
inside the pass was wrong. Locating a pass by bisection is not the same as
knowing what it does; I should have read it before writing the diagnosis.

### What IS established
Phase-level tracing inside the pass:

    DCE enter                 stmts=2 calls=14
    DCE after_phase1_recurse  stmts=2 calls=0     <-- here
    DCE after_phase2_truncate stmts=2 calls=0
    DCE after_phase3_remove   stmts=2 calls=0
    DCE after_phase4_prune    stmts=2 calls=0

The statement COUNT is unchanged and the CALLS vanish during **Phase 1, the
recursion into nested bodies**. Nothing is deleted at this level; a nested
invocation empties a body in place.

### What is NOT established, and I am not asserting it
Widening the trace to the first nested level produced NO invocation entering
with 37 statements, so my assumption about WHICH nested call gutted the body is
unsupported. The DCE trace carries no proto name, so the `stmts=2 calls=14`
record cannot even be tied to `OnInputBegin` with certainty - it is consistent
with it and not proof of it.

Two rotations running I have named a culprit before I could support it. The
measure has not moved in either.

### UNBLOCKING QUESTION, narrowed and honest
Give the DCE trace a proto identity (as `LUAU_REG_WATCH` needed in rotation 49,
for the same reason), then find the nested invocation whose `enter` and
`after_phase*` counts differ. Until a trace is attributable to a proto, every
conclusion drawn from it is a correlation.

### State after rotation 68
SEMANTIC: 1,100/1,142 (96.3%), 48 defects. Behaviour-neutral; 949 tests pass.
MECHANISMS: A, D, E, F, G, H, I, J, A2/C, argument-cap, K CLOSED.
  **B: loss occurs during `eliminate_dead_code`'s Phase 1 recursion.** Ten
  candidates eliminated by measurement; the proposed inversion fix was found
  already implemented in Phase 4, so that is an eleventh. Reproducer
  `277_DragManager::OnInputBegin@184` unchanged.
  50 protos with unreachable blocks - separate, parked.
  Six measured dead ends. PARKED: A2/C-residual, UPVAL_NAME.
TOOLING: + LUAU_DCE_TRACE (per-phase statement and call counts) - needs proto
  identity before its output can be attributed.
