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
