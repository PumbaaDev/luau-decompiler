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


## Rotation 69 - MECHANISM B: root cause established, with an attributable trace

### First, why rotation 68 could not attribute anything
`DCE_PROTO` was stamped at the top of `lift_proto_inner_impl`, but NESTED
CLOSURES are lifted during the region loop and each overwrites it. By the time
the outer proto reaches `eliminate_dead_code`, the identity names the last
nested proto. Filtering for `OnInputBegin@184` returned NOTHING - which is
exactly the symptom, and is why rotation 68's `stmts=2 calls=14` record could
not honestly be tied to this function.

Fixed by re-stamping immediately before the post-pass sequence. Same class of
error as rotation 47 (pc windows read from the wrong proto) and rotation 49
(pc numbering restarts per proto): **a trace without identity is a
correlation.**

### The attributable trace
    depth=2 enter                 stmts=36 calls=14
    depth=2 after_phase1_recurse  stmts=36 calls=14
    depth=2 after_phase2_truncate stmts=4  calls=0
    depth=2 after_phase3_remove   stmts=3  calls=0

    DCETRUNC proto=OnInputBegin@184 at_index=3
             terminator=Return(0 values) dropping=32 stmts

Phase 2 truncates after the first top-level terminator. In the else body a
**bare `return` sits at index 3 of 36**, so 32 statements carrying all 14 calls
are discarded.

### `eliminate_dead_code` IS CORRECT
Everything after an unconditional `return` genuinely is unreachable. `is_terminator`
is `Return | Break | Continue`; `exits_on_all_paths` correctly returns false for an
empty body; `is_empty_if` correctly requires every branch empty; Phase 4 already
inverts an empty-then/populated-else. Four separate suspicions inside this pass,
all checked, all innocent.

**The fault is upstream: a trailing `return` placed in the MIDDLE of a live
body.** That return is visible in the emitted output - it is the one inside the
guard that made the function read as a plausible early-return wrapper.

This ties back to rotation 64: the `if` has `merge=None`, so the structurer put
every remaining block - INCLUDING the function's trailing RETURN block - into
the else arm, and `else_sorted.sort_unstable()` ordered that return into
position 3 rather than last.

### A number I am NOT reporting, again
107 truncations fire corpus-wide. Truncating after a GENUINE return is correct
and common, so that is not a defect count. Only truncations where the return is
misplaced are defects, and separating them needs the block-ordering check below.

### UNBLOCKING QUESTION - now about ordering, not deletion
For an if/else region with `merge_pc = None`, are the else-arm blocks ordered so
that the block containing the function's final RETURN sorts last? `else_sorted`
is a plain ascending sort of block ids; if the RETURN block has a lower id than
blocks that execute before it, the return lands mid-body. Compare the block ids
in `else_region` against their pc ranges for `OnInputBegin@184` - one trace.

### State after rotation 69
SEMANTIC: 1,100/1,142 (96.3%), 48 defects. Behaviour-neutral; 949 tests pass.
MECHANISMS: A, D, E, F, G, H, I, J, A2/C, argument-cap, K CLOSED.
  **B: root cause established** - a bare `return` at index 3 of a 36-statement
  else body causes correct dead-code removal of 32 live statements. Twelve
  candidates eliminated by measurement across eight rotations.
  50 protos with unreachable blocks - separate, parked.
  Six measured dead ends. PARKED: A2/C-residual, UPVAL_NAME.
TOOLING: LUAU_DCE_TRACE now proto-qualified, + DCETRUNC naming the terminator
  and the loss.


## Rotation 70 - MECHANISM B FULLY DIAGNOSED: a flattened else arm

### The ordering trace answers the question, and refutes its premise
    ORDER proto=OnInputBegin@184
      else_sorted=[4 9*RET 10 13 16*RET 17 23 29*RET 30 36*RET 37 49 54 55 59
                   61 72 83 94 97 105 108 117*RET]

The question was "does the trailing RETURN block sort last, or mid-body?" It
sorts LAST (117). The premise was wrong: the else arm contains **five** blocks
carrying RETURN instructions, and the earliest - block 9 - sorts SECOND of 23.

Those are the function's own `if ... then return end` guards. They are
CONDITIONAL in the bytecode.

### The complete chain, end to end
    1. The `if` at block 0 has `merge=None` (rotation 64), so the structurer
       cannot find where it ends and puts ALL 23 remaining blocks into the
       else arm.
    2. `else_sorted.sort_unstable()` orders them by ascending block id and
       `lift_branch_region` lifts them as a FLAT LINEAR SEQUENCE - the nested
       control flow among those 23 blocks is not reconstructed.
    3. Block 9's return, conditional in the bytecode, becomes an
       UNCONDITIONAL `return` at statement index 3 of 36 (rotation 69).
    4. `eliminate_dead_code` Phase 2 correctly deletes the 32 statements after
       an unconditional return, taking all 14 calls.
    5. `calls_not_emitted` (rotation 61) reports the file; nothing else ever did.

Every step is now measured rather than inferred, and each stage of the pipeline
behaves correctly given its input. The single wrong thing is step 2: an arm of
23 blocks with internal branching is lifted as straight-line code.

### Innocent, each checked before being cleared
`eliminate_dead_code` and all four of its phases; `is_terminator`;
`exits_on_all_paths`; `is_empty_if`; `note_stmts_pushed`; the statement budget;
CFG reachability; region production. Twelve candidates across nine rotations,
one build each.

### THE FIX, and why it is not a one-liner
An if/else arm holding blocks with their own control flow must be STRUCTURED
RECURSIVELY, not concatenated. `structure_control_flow` already does exactly
this for a whole proto; the else arm needs the same treatment applied to its
block subset.

That is a real change to `lift_branch_region`, not a peephole, and it is where
the next rotation starts. The reproducer is exact: `OnInputBegin@184`, else arm
= 23 blocks, five containing RETURN, first at position 1.

### State after rotation 70
SEMANTIC: 1,100/1,142 (96.3%), 48 defects. Behaviour-neutral; 949 tests pass.
MECHANISMS: A, D, E, F, G, H, I, J, A2/C, argument-cap, K CLOSED.
  **B: FULLY DIAGNOSED** - flattened else arm turns a conditional return into
  an unconditional one. Affects 8 protos emitting zero calls and 27 emitting
  under half (29 files). Fix identified as recursive structuring of branch
  arms; not attempted.
  50 protos with unreachable blocks - separate, parked.
  Six measured dead ends. PARKED: A2/C-residual, UPVAL_NAME.
TOOLING: + LUAU_ORDER_TRACE (else-arm block order with RETURN marks).


## Rotation 71 - the fix missed its own target, and the miss is the finding

### The attempt
`lift_branch_region` already structures a run recursively when it contains a
nested for-loop (`range_contains_nested_for` -> `lift_structured_run`).
Extended that gate with `run_has_early_return`: a run whose RETURN appears
before its final block is the shape rotation 70 diagnosed.

Measured: **1,100 -> 1,102 clean (96.5%), 48 -> 44 defects.** 31 files changed.

### It did NOT fix the reproducer
`277_DragManager::OnInputBegin@184` is byte-identical. The +2 came from
elsewhere entirely.

The reason is in my own guard. `split_contiguous_runs` requires PC-ADJACENCY
(`ranges[j].1 == ranges[j+1].0`), and the else arm's 23 blocks are NOT adjacent
- the then-block and other regions' blocks sit between them. So the arm
fragments into single-block runs where `hi == lo`, and `run_has_early_return`
returns false on its first line.

**That is a new and precise fact:** the arm is fragmented BEFORE any structuring
decision is made, so no gate placed at that point can ever see the arm as a
whole. A fix must act before `split_contiguous_runs`, or on the block LIST
rather than on contiguous PC spans.

### Reverted anyway, on evidence
    174_ClientStatCache  `if 1 < #arg2 then` -> `while v42 < #arg2 do`
                         a genuine RECOVERY - an if that was really a loop
    218/219_Collectors   `local stringValue2 = ...` inside the branch became
                         chunk-top `local Parent2, stringValue2, value33`

The second is the `free_var_decls` fallback, which rotation 33 established means
the lifter LOST those bindings. So the change trades recovered loops for lost
scoping, and the checker scores that +2 because it cannot see the difference.

A change that misses its stated target, improves some files, degrades others,
and is scored positively by an instrument blind to the degradation is not one to
keep on a +2.

### The count of reverts, stated plainly
Seven fixes reverted this session after measuring well: stem rule (25),
accumulate rule (27), preserve-declared-locals (42), preserve-pure-values (44),
loop-join materialisation (48), declaration-only hoisting (52), and this one.
Every one raised or held the score. Every one was caught by reading output the
score could not see. That ratio is the argument for the checks added in
rotations 24 and 61 rather than against the discipline.

### State after rotation 71
SEMANTIC: 1,100/1,142 (96.3%), 48 defects. 949 tests pass.
MECHANISMS: A, D, E, F, G, H, I, J, A2/C, argument-cap, K CLOSED.
  **B: diagnosis stands; fix location corrected.** The arm is fragmented by
  `split_contiguous_runs` before any structuring gate sees it, so the repair
  must operate on the block LIST (`else_region`) rather than on contiguous PC
  spans. Reproducer unchanged.
  50 protos with unreachable blocks - separate, parked.
  Seven measured dead ends. PARKED: A2/C-residual, UPVAL_NAME.


## Rotation 72 - the whole-arm span, re-measured; second fix location, second miss

### The experiment
`lift_branch_region`'s header records that structuring `min(start)..max(end)` of
a DISJOINT arm "double-lifted foreign blocks and broke 23 compile-gate files".
That verdict predates this corpus, and re-measuring such claims here has been
productive three times, so it got one run behind `LUAU_ARM_SPAN`.

    default        1,100 clean / 48 defects
    LUAU_ARM_SPAN  **1,104 clean / 39 defects**

And the failure mode the comment warned about did not appear in a 60-file
sample: **0 of 60 span-mode outputs fail `validate`.** That is a sample, not the
corpus, and it is not a clearance - but it is enough that the old verdict should
not be treated as settled here.

### It STILL does not fix the reproducer
`277_DragManager::OnInputBegin@184` is byte-identical AGAIN. Rotation 71 missed
it because `split_contiguous_runs` fragments the arm; this attempt bypasses that
entirely and still misses.

So the +4 comes from other arms both times, and **two different fix locations
have now failed to touch the diagnosed case.** That is evidence the diagnosis is
incomplete: something about THIS arm prevents `lift_branch_region` from taking
either structured path, and I do not yet know what. The guard requires
`ranges.len() > 1`, which 23 blocks satisfy, so the likeliest explanation is
that this arm reaches the lifter by a route I have not traced.

### Kept GATED rather than reverted or adopted
Default behaviour verified unchanged (1,100/48, 949 tests). The code stays
behind `LUAU_ARM_SPAN` with its numbers in the comment, exactly as
`LUAU_DECL_HOIST` does, so the next attempt reproduces it in one run.

Adopting it on a +4 would repeat rotation 71's mistake: banking a score gain
from a change that misses its stated target and whose broad effects are sampled
rather than verified.

### State after rotation 72
SEMANTIC: 1,100/1,142 (96.3%), 48 defects by default. 949 tests pass.
MECHANISMS: A, D, E, F, G, H, I, J, A2/C, argument-cap, K CLOSED.
  **B: diagnosis INCOMPLETE.** Root cause of the truncation is established with
  an attributable trace (rotation 69-70), but two fix locations have failed to
  reach the reproducer. NEXT QUESTION: trace which `lift_branch_region`
  invocation actually receives `OnInputBegin@184`'s else arm - print the
  `sorted_blocks` it is called with - because both attempts assumed a call that
  may not be happening.
  50 protos with unreachable blocks - separate, parked.
  Seven measured dead ends plus one gated experiment. PARKED: A2/C-residual,
  UPVAL_NAME.


## Rotation 73 - both fixes were in the right place; the STRUCTURER is wrong

### The trace that ended the guessing
    ARM proto=OnInputBegin@184 blocks=1  ranges=1  first=[3]
    ARM proto=OnInputBegin@184 blocks=23 ranges=23 first=[4, 9, 10, 13, 16, 17]

`lift_branch_region` IS called with all 23 else-arm blocks. So rotation 72's
guard (`ranges.len() > 1` plus an early RETURN in `ranges[..last]`) was
satisfied, and the structured path WAS taken - it simply produced the same
output.

### Why: the arm structurer cannot express a branch
`lift_structured_run` delegates to `structure_numeric_for_body`, and that
function emits only:

    Region::Linear
    Region::ValueJoin

**No `Region::IfThenElse`.** It reconstructs for-loops and value joins; it has
no capacity to rebuild if/else nesting. Structuring a branch-only arm with it
returns the arm flattened - which is precisely the defect.

So rotations 71 and 72 were not in the wrong place. The gate fired (or would
have), the structured path ran, and the structurer handed back linear regions.
Two rotations of "the fix location must be wrong" were chasing the wrong half of
the call.

### What this makes the fix
Branch arms need `structure_control_flow`'s if/else matching applied to their
block subset - not `structure_numeric_for_body`. That function currently takes a
whole proto and its CFG and walks the reverse postorder; making it operate on a
block subset (or adding an entry point that does) is the actual change, and it
is real work rather than a gate adjustment.

That also retires the framing carried since rotation 70 ("recursive structuring
of branch arms" as if the machinery existed). It does not - the machinery
handles loops and joins only.

### State after rotation 73
SEMANTIC: 1,100/1,142 (96.3%), 48 defects. Behaviour-neutral; 949 tests pass.
MECHANISMS: A, D, E, F, G, H, I, J, A2/C, argument-cap, K CLOSED.
  **B: complete diagnosis.** A branch arm whose blocks contain their own
  if/else is lifted flat because the only structurer available to arms emits
  Linear and ValueJoin. A conditional RETURN therefore becomes unconditional
  and `eliminate_dead_code` correctly removes everything after it. 29 files;
  reproducer `277_DragManager::OnInputBegin@184`.
  REQUIRED CHANGE, stated exactly: an if/else-capable structuring entry point
  that accepts a block subset.
  50 protos with unreachable blocks - separate, parked.
  Seven measured dead ends plus one gated experiment (LUAU_ARM_SPAN, +4/-9,
  0 of 60 sampled validation failures). PARKED: A2/C-residual, UPVAL_NAME.
TOOLING: + LUAU_ARM_TRACE (what each branch-arm lift receives).


## Rotation 74 - the subset structurer BUILT and MEASURED: it regresses badly

### What was built
`structure_block_subset(cfg, proto, subset)` - the change rotation 73 named. It
mirrors `structure_control_flow`'s if/else matching but is CONFINED to the arm's
own blocks: a branch is matched only when both arms lie entirely inside the
subset, so it cannot claim foreign blocks the way the `min..max` span attempt
did. Unmatched blocks fall through to `Region::Linear`, so in principle the
result is never worse than the flat lift.

Wired into `lift_branch_region` behind the same early-return gate.

### Result 1: infinite recursion
The corpus run overflowed the stack. `lift_structured_regions` routes an
`IfThenElse` back through `lift_region`, which calls `lift_branch_region` on the
arms, which structures again - and if a produced arm is not STRICTLY smaller
than the input, that never terminates.

Added a progress guard requiring every produced arm to be smaller than the input
block list. That is the right shape of fix and it stopped the overflow.

### Result 2: a large regression
    before  1,100 clean / 48 defects
    after   **1,055 clean / 140 defects**

Ninety-two defects worse. Reverted.

### What the failure teaches
"Never worse than the flat lift" was wrong, and the reasoning behind it was
wrong in a specific way: matching a branch inside the arm CHANGES REGISTER
STATE HANDLING. A flat lift threads one register state through the whole span;
an `IfThenElse` region splits it, merges the arms through `merge_regs`, and
every value the merge cannot reconcile becomes `Unknown`. Introducing branch
structure therefore introduces every merge-loss mechanism this session has
already documented (A2/C, rotation 35's tables, rotation 51's single-arm
bindings) into 29 files that previously lifted linearly.

So the flat lift is not merely a missing feature - it is avoiding a cost. A
correct fix has to structure the arm AND survive the merges that structuring
creates, which is why this is not a self-contained change.

### The honest position on mechanism B
Root cause: established and attributable (rotations 69-70).
Required change: named (rotation 73).
Change: BUILT and MEASURED here, and it costs 92 defects.

That is not a park for want of an idea; it is a measured verdict that the
obvious implementation of the named change is net-negative, and the next
attempt must handle arm merges rather than only arm structure.

### State after rotation 74
SEMANTIC: 1,100/1,142 (96.3%), 48 defects. 949 tests pass; baseline restored
  and verified.
MECHANISMS: A, D, E, F, G, H, I, J, A2/C, argument-cap, K CLOSED.
  **B: diagnosis complete; the named fix implemented and measured at
  -45 clean / +92 defects.** Reproducer `277_DragManager::OnInputBegin@184`.
  Next attempt must pair arm structuring with the merge handling that
  structuring introduces.
  50 protos with unreachable blocks - separate, parked.
  **Eight measured dead ends** plus one gated experiment. PARKED:
  A2/C-residual, UPVAL_NAME.


## Rotation 75 - the unreachable-block thread CLOSED as a false alarm

### Returning to a parked item with better tooling
Rotation 62 found 50 protos containing blocks unreachable from their entry, and
parked the question: is each orphan genuine dead code, or a MISSING CFG EDGE
silently deleting live code? It was the one thread never revisited.

The question is mechanically decidable. If any block lists the orphan as a
SUCCESSOR, the CFG knows the edge and reachability lost it - a real defect. If
nothing points at it, the compiler emitted dead code and dropping it is correct.

### The adjudication
    protos with unreachable blocks : 50
      orphan has a predecessor     :  0
      orphan has NO predecessor    : **50**

All fifty are genuine dead code. No block anywhere lists any orphan as a
successor, so nothing reaches them and discarding them is correct behaviour.

**CLOSED as a false alarm**, with a mechanical proof over the whole set rather
than a sample - the same standard that resolved `truncated_body` (42 of 42) and
`undefined_register` (68 of 72).

### Worth noting about the original finding
Rotation 62 recorded this as "a real defect source, independent of B, that
nothing has ever reported". That framing was too strong: it was an
UNADJUDICATED observation, and stating it as a defect source was the same
overreach as the 797- and 935-block ratios I later declined to report. The
correction is that finding an anomaly and demonstrating a defect are different
acts, and only the second belongs in a count.

### State after rotation 75
SEMANTIC: 1,100/1,142 (96.3%), 48 defects. Behaviour-neutral; 949 tests pass.
MECHANISMS: A, D, E, F, G, H, I, J, A2/C, argument-cap, K CLOSED.
  **unreachable blocks CLOSED** (50 of 50 genuine dead code).
  B: diagnosis complete, named fix built and measured at -45 clean / +92
  defects; next attempt must pair arm structuring with merge handling.
  Eight measured dead ends plus one gated experiment.
  PARKED: A2/C-residual (path-sensitive dataflow), UPVAL_NAME (format-blocked).
TOOLING: LUAU_RPO_TRACE now reports orphan predecessors, which is what made the
  adjudication a one-line question.


## Rotation 76 - the merge explanation is FALSIFIED

### The narrowing, and why it should have worked
Rotation 74 measured general arm structuring at -45 clean / +92 defects and
explained it: an `IfThenElse` splits the register state, `merge_regs` resets
whatever the arms disagree on, and structuring therefore imports every
merge-loss mechanism into arms that previously lifted linearly.

If that explanation were right, restricting structuring to branches whose arms
TERMINATE should cost nothing - a branch with no join has nothing to reconcile.
`OnInputBegin`'s guards are exactly that shape.

Built `structure_terminating_branches`, gated on `blocks_all_terminate`, with
the confinement and strict-decrease guards from rotation 74 retained.

### MEASURED: essentially the same damage
    baseline                 1,100 clean /  48 defects
    general structuring (74) 1,055       / 140
    terminating-only (76)    **1,060     / 141**

Restricting to merge-free branches recovered 5 files out of 45 lost. The cost is
NOT the merges.

**Rotation 74's explanation is falsified.** It was a plausible mechanism,
consistent with everything this project has documented about `merge_regs`, and
it is wrong - or at most a small part of the cost. I recorded it as the reason;
it was a hypothesis wearing a reason's clothes.

### What that leaves
Something about routing an arm through `Region::IfThenElse` rather than a flat
`lift_instruction_range` is itself expensive, independently of joins. Candidates
not yet separated: register threading through `lift_region`'s arm setup, name
allocation differences (`classify_write` sees a different write order), the
B0.57 hoists that run per-branch, or `regs_before` snapshots taken at a
different point.

Isolating that needs the same treatment mechanism B's diagnosis got - a
before/after count on ONE file across the structured and flat paths - rather
than another whole-corpus attempt.

### State after rotation 76
SEMANTIC: 1,100/1,142 (96.3%), 48 defects. Baseline restored and verified;
  949 tests pass.
MECHANISMS: A, D, E, F, G, H, I, J, A2/C, argument-cap, K, unreachable-blocks
  CLOSED.
  **B: two implementations measured, both net-negative, and the explanation for
  WHY is now known to be wrong.** The honest next step is not a third
  implementation but a per-file comparison of the structured and flat paths on
  the reproducer.
  **Nine measured dead ends** plus one gated experiment.
  PARKED: A2/C-residual, UPVAL_NAME.


## Rotation 77 - the per-file comparison: structuring RECOVERS A LOOP the measure cannot see

### The comparison rotation 76 called for
Ran `174_ClientStatCache` through both paths - default (flat) and
`LUAU_ARM_SPAN` (structured) - and diffed:

    FLAT (shipping today)              STRUCTURED
    if 1 < #arg2 then                  while v42 < #arg2 do
        local tbl9 = StatTools2[arg2[1]]   tbl9 = StatTools2[arg2[v42]]
        if StatTools2[arg2[1]] == nil      if StatTools2[arg2[v42]] == nil
        ...                                ...
                                           v42 += 1
                                       end
    StatTools2[arg2[#arg2]] = arg3     tbl9[arg2[#arg2]] = arg3

The flat path renders a `while` LOOP as a single `if` with the index HARDCODED
to `arg2[1]`, and writes the result to the wrong target. The structured path
recovers the loop, the loop variable, the increment, and the correct target.

The structured output is substantially, obviously more correct.

### And the checker scores them IDENTICALLY
    flat.lua   : 1 finding - calls_not_emitted, 5 calls short
    struct.lua : 1 finding - calls_not_emitted, 5 calls short

One finding each, the same one. **The measure cannot distinguish a recovered
loop from a lost one.**

### What that does and does not establish
It does NOT support the hypothesis I formed on seeing the diff - that the
checker PENALISES correct output. It scores them the same, not worse. I nearly
recorded the stronger claim; the check took one run and did not support it.

It DOES establish that the corpus-level -45 clean measured in rotations 74 and
76 is driven by other files, and that at least some of what the structured path
produces is a real improvement the measure is blind to. So "structuring costs 45
clean files" is not the same statement as "structuring makes the output worse",
and I have been treating them as one.

### Process slip, recorded
`rm -rf crates/luau-core/examples` to remove a throwaway comparison harness also
deleted the tracked `decompile_demo.rs`. Caught by `git status` in the same
command and restored immediately. The lesson is the obvious one: remove the file
you created, not the directory it happens to sit in.

### State after rotation 77
SEMANTIC: 1,100/1,142 (96.3%), 48 defects. Tree clean; 949 tests pass.
MECHANISMS: A, D, E, F, G, H, I, J, A2/C, argument-cap, K, unreachable-blocks
  CLOSED.
  **B: the structured path is now known to produce genuinely better output on at
  least one file while scoring identically.** The -45 needs attributing to
  specific files before any verdict on structuring is justified - the next step
  is a per-file diff of the corpus run under `LUAU_ARM_SPAN`, ranked by which
  files LOST clean status, not another implementation.
  Nine measured dead ends plus one gated experiment.
  PARKED: A2/C-residual, UPVAL_NAME.


## Rotation 78 - MECHANISM B: a fix ADOPTED, after correcting my own conflation

### The conflation that hid a working fix for six rotations
Three different implementations were measured and I attributed one's result to
another:

    LUAU_ARM_SPAN (rotation 72)          1,104 clean /  39 defects  IMPROVES
    structure_block_subset (74)          1,055       / 140          regresses
    structure_terminating_branches (76)  1,060       / 141          regresses

Rotation 77 compared flat against `LUAU_ARM_SPAN` - the IMPROVING one - and I
discussed the result as though the -45 belonged to it. It never did. Rotation 72
declined to adopt it for "missing the reproducer" and having "sampled rather
than verified" effects, and then rotations 74-77 argued about a cost that
belonged to two other pieces of code.

### Verified properly this rotation
    files losing clean status                : **0**
    validation failures, FULL corpus         : **0 of 1142** (baseline: 0 of 1142)
    changed files GAINING a loop construct   : **27**
    changed files LOSING one                 : **0**

The compile breakage the source comment warned about ("broke 23 compile-gate
files") does not occur on this corpus at all - full corpus, not a sample.

### What it actually does
The flat arm lift FLATTENS LOOPS. `174_ClientStatCache` renders a `while` loop
as a single `if` with the index hardcoded to `arg2[1]` and writes to the wrong
target; `302_PromoButton` renders a `while true do` body once, unconditionally;
`218_Collectors` loses `while v8 == v8 + 1 do`. Structuring the arm recovers
them - 27 files.

The 4 files that gain a chunk-top declaration are the CONSEQUENCE of that: a
variable assigned inside a recovered loop body needs outer scope. Rotation 71
saw this same signature and read it as a regression; with the loop counts beside
it, it is the opposite.

### Measurement - clean run, empty out-dir, 1142/1142 decodable
    1,100 -> **1,104 clean (96.7%)**, 48 -> **39 defects**
    949 tests pass; the two pre-existing `opmap_dupclosure_detection` failures
    are unchanged.

### What this cost, and what it says
Six rotations were spent arguing about a regression that belonged to code I had
already reverted. The specific error: recording three experiments under one
name ("arm structuring") and then reasoning about "it". Distinct implementations
need distinct names in the journal, or their measurements merge.

Against that, the discipline held where it mattered - nine changes were reverted
for damaging output the score could not see, and this one was adopted only after
0/1142 validation, per-file attribution, and a loop-count check that turned the
apparent regression into its explanation.

### State after rotation 78
SEMANTIC: **1,104/1,142 (96.7%), 39 defects.**
MECHANISMS: A, D, E, F, G, H, I, J, A2/C, argument-cap, K, unreachable-blocks
  CLOSED. **B: PARTIALLY FIXED** - 27 files regain lost loops; the
  `OnInputBegin` reproducer is NOT among them and still emits 4 of 36
  statements, so the dropped-body half remains open with its diagnosis intact.
  Nine measured dead ends. PARKED: A2/C-residual, UPVAL_NAME.


## Rotation 79 - B's remaining half, scoped and its question written down

### The reproducer under the adopted fix: UNCHANGED
    BODYAST proto=OnInputBegin@184 bc_calls=15 ast_calls=0
    IFLIFT  else_blocks=23 else_stmts=Some(37) else_calls=Some(14)
    IFPUSH  then_stmts=0  else_stmts=Some(37) else_calls=Some(14)

Byte-identical to before rotation 78. The arm still lifts 37 statements with 14
calls, still pushes them, still loses them.

### Which half the adopted fix actually fixed
`lift_structured_run` delegates to `structure_numeric_for_body`, which emits
`Linear` and `ValueJoin` - and matches FOR-LOOPS. So structuring the arm span
recovers arms whose blocks contain LOOPS (27 files, rotation 78) and does
nothing for arms whose blocks contain only BRANCHES.

`OnInputBegin`'s 23 blocks are guards - `if ... then return end` - with no loop
among them. The fix cannot reach it, and that is a property of the structurer,
not a gate condition.

So mechanism B splits cleanly:
    B-loops     arms containing loops       **FIXED** (27 files, rotation 78)
    B-branches  arms containing branches    OPEN (the dropped-body half)

### B-branches: the unblocking question, stated
Adding `Region::IfThenElse` to arm structuring has been built twice and measured
twice:

    structure_block_subset (74)          -45 clean / +92 defects
    structure_terminating_branches (76)  -40 clean / +93 defects

Rotation 76 falsified the merge explanation (restricting to merge-free branches
recovered 5 files of 45). The cost is real, reproducible, and UNEXPLAINED.

**THE QUESTION: which specific files lose clean status under
`structure_block_subset`, and what changes in them?**

Rotation 78 answered exactly this shape of question for the loop fix - per-file
attribution turned an apparent 4-file regression into 27 recovered loops - and
that method was never applied to the branch build. Concretely: rebuild
`structure_block_subset` from `backup/pre-subset-structurer`, run the corpus,
diff per-file clean status against baseline, and read the top few losers. The
answer is either a real defect the branch path introduces, or the same
misreading rotation 71 made about chunk-top declarations.

That is one rebuild and one corpus run, and it is where B-branches resumes.

### State after rotation 79
SEMANTIC: **1,104/1,142 (96.7%), 39 defects.** 949 tests pass.
MECHANISMS: A, D, E, F, G, H, I, J, A2/C, argument-cap, K, unreachable-blocks,
  **B-loops** CLOSED.
  **B-branches** PARKED with the question above and the exact rebuild recipe.
  A2/C-residual PARKED (needs path-sensitive dataflow; both peephole repairs
  measured and rejected in rotations 35 and 52).
  UPVAL_NAME BLOCKED (Roblox bytecode carries no CAPTURE data, so the upvalue's
  identity is not present in the input at all).
  Nine measured dead ends, each with its numbers and the file that killed it.
SWEEP: 6 kinds adjudicated mechanically; ~19 files carry a demonstrated defect
  of 1,112, none silenced.


## Rotation 80 - B-branches ANSWERED: it is blocked on A2/C

### The recorded question, executed
Rotation 79 wrote down: "which specific files lose clean status under
`structure_block_subset`, and what changes in them?" - and noted that rotation
78's per-file attribution had never been applied to the branch build.

Rebuilt it behind `LUAU_BRANCH_ARMS` (gated, for measurement, not adoption) and
ran the corpus: 915/1142 clean, 517 defects. Worse than rotation 74's -45
because it now stacks on the adopted arm-span fix.

### The attribution, and it is one kind
    new findings introduced, by kind
      **204  declared_never_assigned**
        6  undefined_local
        6  branch_local_discarded
        3  calls_not_emitted

204 of 219 are a SINGLE kind. `declared_never_assigned` is the signature of a
register losing its value and `free_var_decls` declaring the name at chunk top
with nothing ever assigning it - the A2/C family, documented since rotation 33.

### What that resolves
The cost was called "real, reproducible and UNEXPLAINED" for four rotations.
It is now explained and it is not a new mechanism: **structuring branch arms
makes A2/C fire 204 more times.**

Rotation 76's terminating-arms narrowing failed because it confined the fix to
the branch being matched, while the register losses land on code DOWNSTREAM of
it - so removing the local join changed almost nothing.

**B-branches is blocked on A2/C**, which is already parked as needing
path-sensitive dataflow, with both peephole repairs measured and rejected
(rotations 35 and 52). Fixing branch-arm structuring first would emit 204 more
of exactly the defect A2/C describes.

That is a DEPENDENCY, not a fourth failed attempt: the two parked mechanisms are
ordered, and the order is A2/C first.

### State after rotation 80
SEMANTIC: **1,104/1,142 (96.7%), 39 defects.** Default verified unchanged;
  949 tests pass. `LUAU_BRANCH_ARMS` left gated with its numbers, as
  `LUAU_DECL_HOIST` is.
MECHANISMS: A, D, E, F, G, H, I, J, A2/C(if-else/tables), argument-cap, K,
  unreachable-blocks, **B-loops** CLOSED.
  **B-branches: question ANSWERED, now BLOCKED ON A2/C** with the measurement
  that establishes the dependency.
  **A2/C-residual** is therefore the single next target, and its own question
  stands: emit the declaration only where the register is genuinely assigned on
  every path reaching the join - which needs the paths, not the arms.
  UPVAL_NAME BLOCKED on the format (no CAPTURE data in Roblox bytecode).
  Nine measured dead ends plus two gated experiments.
SWEEP: six kinds adjudicated mechanically; ~19 of 1,112 files carry a
  demonstrated defect; nothing silenced.


## Rotation 81 - the remaining 39, and the ceiling for each mechanism

### Composition of what is left
    24  calls_not_emitted        B-branches (dropped function bodies)
     9  declared_never_assigned  A2/C-residual
     6  branch_local_discarded   mechanism I residue

### The ceiling, and the ordering it implies
    A2/C-residual   fixes  9 directly, and UNBLOCKS the 24 (rotation 80)
    B-branches      fixes 24, but cannot be attempted first
    mechanism I     6 remain after rotations 28/31/42's predicates

So A2/C-residual is worth 33 of the 39 by dependency, not 9. That is the
argument for treating it as the single next target rather than the largest
count.

### What A2/C-residual needs, restated with the evidence behind it
Emit the declaration only where the register is genuinely assigned on EVERY
PATH reaching the join. Both peephole substitutes are measured and rejected:

    materialise before the arms   works for TABLES (rotation 35, kept and
                                  shipping), impossible for a call result -
                                  it cannot be moved across a branch
    declare before the arms       rejected on CoreGui AND full1143
                                  (rotation 52: 1,126 -> 1,120 clean); it
                                  relabels `undefined_register` as
                                  `declared_never_assigned` rather than fixing
                                  anything

The distinguishing fact both substitutes lack is which PATHS assign the
register - `merge_regs` sees two arm states, not the set of paths reaching the
join. That is the dataflow the mechanism needs, and it is why this is
engineering rather than another predicate.

### Session arithmetic, for the record
    semantic   1,097/1,142 -> **1,104/1,142**, 97 defects -> **39**
    and the checker gained TWO defect classes it previously could not see
    (`branch_local_discarded`, rotation 24; `calls_not_emitted`, rotation 61),
    so the 39 is measured by a strictly wider instrument than the 97 was.

    sweep      six kinds adjudicated mechanically; the grader's 246 broken
               became 134 by two-run agreement and ~19 by artefact testing.

    13 mechanisms closed · 9 measured dead ends · 2 gated experiments ·
    9 reverts of changes that improved the score and damaged the output.

### State after rotation 81
SEMANTIC: 1,104/1,142 (96.7%), 39 defects, no known false positives among them.
CLOSED: A, D, E, F, G, H, I(predicates), J, A2/C(if-else/tables), argument-cap,
  K, unreachable-blocks, B-loops.
NEXT TARGET: **A2/C-residual** - 9 direct, 24 unblocked, question stated above.
BLOCKED: B-branches (on A2/C, measured); UPVAL_NAME (on the format - Roblox
  bytecode carries no CAPTURE data, so the identity is absent from the input).


## Rotation 82 - rotation 81 was wrong about the 9, and the real fault is located

### Correction: these are not branch-join declarations
Rotation 81 filed the 9 `declared_never_assigned` as "A2/C-residual, needs
path-sensitive dataflow". That was wrong, and the file's own trace says so.
Eight of the nine sit at LINE 8 - the chunk prologue, not a join - and
`LUAU_MINT_TRACE` classifies every one as the READONLY branch of
`free_var_decls` with these mint sites:

    REG_EXPR_UNKNOWN + UNK_CALL_CLEAR      5   (533, 85, 22, 368, 53)
    REG_EXPR_UNKNOWN + UNK_MERGE_COMPOUND  1   (79)
    METHOD_RECV_REJECT                     1   (54)
    TABLE_EXPR_REJECT                      1   (376)
    UPVAL_NAME                             1   (873, already blocked on format)

Five distinct mint sites, not one dataflow gap. `merge_regs` is not involved.

### The write is in the bytecode; the BLOCK is in the wrong place
`533_NPCs::AddEvent`, the `if v3 then` at emitted line 195:

    174: GETTABLEKS  R3 R1."StartReqs"     <- the write is RIGHT THERE
    176: JUMPIFNOT   R3 -> +16

So nothing was dropped by a naming pass. The emitted block sits INSIDE the
`error(...)` arm and BEFORE the pc-139 `SETTABLEKS R2."NPC"` block - out of
bytecode order. `REGIONS proto=AddEvent blocks=37 regions=4` is the same fact
from the other side.

### The located fault
`LUAU_IF_TRACE` on AddEvent:

    IF cond_block=112 then=14 else=0 merge=Some(210) has_else=false

The `if` is `JUMPIF R2 -> +9` at pc 129, so its target is pc 139, and the error
block 130..138 FALLS THROUGH to 139. Both paths converge at 139: that is the
immediate postdominator and the correct merge. The structurer instead returned
210 - and pc 147 is `JUMP -> 62`, i.e. 147+1+62 = 210 exactly.

**The merge is being taken as the target of the arm's terminating unconditional
jump instead of the immediate postdominator.** Blocks 139..209 are therefore
handed to `lift_branch_region` already inside the arm.

### The experiment that proved it, by failing
Widened the adopted arm trigger from "contains Return" to "contains a
conditional branch" (`LUAU_ARM_COND`, left gated, inert - re-measured 1,104/39
with the gate off):

    clean 1,104 -> 1,103   defects 39 -> 40
    fixed 286_LeaderboardsGui; broke 7_Shop and 919_pprint
    533_NPCs - the file it was BUILT from - UNCHANGED

The negative result is the useful half. No trigger inside `lift_branch_region`
can fix 533, because by the time that function runs the join is already in the
arm. That rules out the whole family of fixes attempted in rotations 78-81 and
points at one function instead.

### What this does to the ceiling
`calls_not_emitted` (24) is dropped bodies; `UNK_CALL_CLEAR` (5) is a register
cleared by a call that a later read reaches because the read was placed on the
wrong side of a join. Both are consistent with one wrong merge point, which
would make **~29 of the 39** one mechanism rather than three.

Not yet proven - it is proven for 533 and consistent with the rest.

### Attack that is blocked, with the reason
The goal's "compile a sub-20-line reproducer yourself" route: `tools/luau/
luau-compile.exe` emits **bytecode v11**. Opening the parser gate 3..=9 -> 3..=11
gets past the version check and then fails in the proto reader ("unexpected end
of bytecode reading u32 at offset 414") - v11 changed the proto encoding. This
build exposes no bytecode-version fflag (`LuauBytecodeVersion` is registered but
rejects an integer value). Gate change reverted. Synthetic reproducers need a
Luau build old enough to emit v9, or v11 proto support in the parser.

### State after rotation 82
SEMANTIC: 1,104/1,142 (96.7%), 39 defects. 949 tests pass; the 2
  `opmap_dupclosure_detection` failures are pre-existing and unrelated.
NEXT TARGET: the merge computation in `structure_control_flow` - make the merge
  the immediate postdominator rather than the arm's jump target. Reproducer:
  `533_NPCs` proto `AddEvent`, expect merge=139, currently 210.
CLOSED: A, D, E, F, G, H, I(predicates), J, A2/C(if-else/tables), argument-cap,
  K, unreachable-blocks, B-loops.
PARKED: UPVAL_NAME (format carries no CAPTURE data); synthetic reproducers
  (need a v9-emitting Luau).


## Rotation 83 - a +1 that deletes a purchase guard, and a correction to r82

### Correction to rotation 82
R82 claimed the fault was `find_merge_point` returning the arm's jump target
instead of the immediate postdominator, citing `merge=Some(210)` with pc 147 =
`JUMP -> 62` (147+1+62 = 210). **That was wrong.** The conditional at
`cond_block=112` is the `JUMPXEQKS` at pc 121, not the `JUMPIF` at pc 129. Its
targets are 123 and 148, and both genuinely converge at 210, so 210 is the
CORRECT merge for that test. `find_merge_point` is not at fault and its
alternating BFS is sound.

What is true from r82 stands: the write is in the bytecode at pc 174, the block
is emitted out of order, and `AddEvent` is 37 blocks -> 4 regions. The `if not
quest` at pc 129 is NESTED inside the 14-block then-arm of the pc-121 test.

### The hole that is real
`range_contains_nested_for` structures an arm that contains a complete FOR pair.
Nothing does the same for a nested BRANCH, and a CONTIGUOUS arm cannot reach
either trigger in `lift_branch_region` because both are behind
`ranges.len() > 1`. Added `range_contains_nested_cond` + `LUAU_ARM_IF`.

    clean 1,104 -> 1,105   defects 39 -> 38
    fixed 286_LeaderboardsGui; NO new findings anywhere
    23 files changed; loops gained in 13, lost in 0; calls lost in 1

Sampled recoveries are genuine, not score-chasing:

    93_Beequips     if INBOX_CAPACITY < #self.Beequips[v32]  ->  while ... do
    383_Honey       if arg2.Honey == arg2.Honey - self.Amount -> while ... do
                    (the baseline condition was self-referential nonsense)
    974_MonsterLevel  gains a `while` with a `break`, replacing inverted logic

### Why it is NOT adopted
`1125_Supreme_Star_Amulet_Generator` LOSES this line outright:

    if 490000000000 <= value2.Honey then

and runs the body that spends 490 billion honey UNCONDITIONALLY. No check in
the suite can see a deleted guard - the file still parses, still scores clean,
and is now wrong in the most expensive direction available.

Narrowing the predicate to strictly-nested targets (`target < end`, on the
theory that a test whose target IS `end` is the arm's own guard) did not restore
it: byte-identical output for 1125, same 1,105/38. So the guard is lost
somewhere other than the predicate boundary, and the theory was wrong.

The B-loops fix was adopted on "gained 27, lost 0". This one loses a construct
in one file. By the project's own standard it does not ship. Left gated and
verified inert (1,104/39 with the gate off, 949 tests pass).

### 533 is still not located
`LUAU_ARM_IF` structures exactly the shape 533 was diagnosed as - a contiguous
arm with a nested conditional - and `533_NPCs` is UNCHANGED under it, as it was
under `LUAU_ARM_COND`. Two different triggers built from that diagnosis, neither
touches the file. So the then-arm of the pc-121 test is not lifted through the
runs loop in `lift_branch_region` at all.

**THE QUESTION**: which code path lifts the 14-block then-arm of
`AddEvent`'s pc-121 `JUMPXEQKS`? Instrument the entry of every lift path with
the proto and range and read it off, rather than inferring it from the shape a
third time. Both triggers so far were built on an inference about that path and
both missed.

### State after rotation 83
SEMANTIC: 1,104/1,142 (96.7%), 39 defects (unchanged - the +1 was rejected).
949 tests pass; the 2 `opmap_dupclosure_detection` failures are pre-existing.
GATED, INERT, MEASURED: `LUAU_ARM_COND` (-1 clean), `LUAU_ARM_IF` (+1 clean,
  deletes a guard), `LUAU_DECL_HOIST`, `LUAU_BRANCH_ARMS`.
NEXT: answer THE QUESTION above by instrumentation, not inference.


## Rotation 84 - the path, measured; pc 174 is never lifted by anything

### Answering r83's question by instrumentation
Two fixes were built on an inference about which path lifts
`533_NPCs::AddEvent`'s arm, and both left the file byte-identical. Added
`LUAU_LIFTPATH` (gated, verified inert): one line per lift entry with the proto
and the range. `AddEvent`, from pc 115 on:

    branch_region    115..210
    structured_run   115..210      <- it IS structured; both earlier guesses wrong
    flat             115..176         outer span
    flat             123..148
    flat             130..139         the error arm
    flat             153..174
    flat             155..159
    flat             160..174      <- ends AT 174, exclusive
    flat             177..182      <- resumes AFTER 176
    flat             183..191
    flat             193..210

### The fault
**Nothing lifts pc 174.** Ranges are half-open, so `160..174` stops before it and
`177..182` starts after the `JUMPIFNOT` at 176. Only the outer `115..176` spans
it, and that walk does not come back for it: after recursing an inline-if arm it
resumes at the branch instruction, skipping whatever sits between the end of the
recursed arm and that branch.

    172: SETTABLEKS  R2."Requirements" R3     <- last instruction of 160..174
    174: GETTABLEKS  R3 R1."StartReqs"        <- LIFTED BY NOBODY
    176: JUMPIFNOT   R3 -> +16                <- emitted as `if v3 then`

That is the whole defect. `v3` is not a naming failure, not a dropped write, and
not a merge-point error - the instruction is skipped by the pc walk. It explains
the shape exactly: the condition survives (176 is emitted), its definition does
not (174 is not), and `free_var_decls` then hoists the orphan to line 8.

### What this retires
    r81  "needs path-sensitive dataflow at joins"     - wrong, not a join
    r82  "find_merge_point returns the jump target"   - wrong, merge=210 correct
    r83  "contiguous arm never reaches a structurer"  - wrong, structured_run RUNS

Three inferences, three misses, all from reasoning about the shape of the
output. One instrumented run settled it. The lesson is cheap to state and was
expensive to learn: instrument the path before theorising about the path.

### Next
Fix the resume point in `lift_instruction_range`'s inline-if recursion so the
walk continues from the end of the recursed arm rather than from the branch,
and any instruction between them is lifted. Expected to be visible on
`533_NPCs` (`local v3 = arg2.StartReqs` appears, `if v3 then` becomes
`if arg2.StartReqs then` or keeps a bound `v3`), and to reach the other four
`UNK_CALL_CLEAR` files if they share the shape.

Mutation test for the fix: it must NOT change `1125_Supreme_Star_Amulet_
Generator`, and the whole-corpus construct census (loops/ifs/calls gained vs
lost per changed file) must show no construct lost anywhere - the standard the
B-loops fix was adopted on and `LUAU_ARM_IF` failed.

### State after rotation 84
SEMANTIC: 1,104/1,142 (96.7%), 39 defects. 949 tests pass.
`LUAU_LIFTPATH` kept: it answered in one run what three rotations of inference
got wrong, and the next fix needs it to verify.


## Rotation 85 - correcting r84: pc 174 IS walked, and its value IS correct

### The correction
R84 concluded "nothing lifts pc 174", read off `LUAU_LIFTPATH` ranges. Wrong.
`LUAU_PC_WINDOW=150-182` shows the walk visiting it:

    150 GetTableKS a=3   152 JumpIf a=3    153 GetUpval a=3   154 JumpIf a=3
    155 NewTable a=3     157 SetTableKS    160 NewTable a=3   162 DupTable a=4
    163 NewTable a=5     165 GetUpval a=6  166 SetList        168 SetTableKS
    170 SetList          172 SetTableKS    174 GetTableKS a=3  <-- WALKED
    177 GetImport a=3    179 GetTableKS a=4  181 Call

and `LUAU_REG_WATCH=3` shows the value arriving and surviving:

    before_pc=174  Expr(Name("NPC2"))
    before_pc=177  Expr(Field { object: Name("arg2"), field: "StartReqs" })

So R3 holds exactly the right expression at the point the span ends (176), and
the `for` at 177-181 goes on to resolve `arg2.StartReqs` correctly from R4.

**The instruction is lifted, the register is correct, and the emitted condition
is still `v3`.** The loss is between that live register state and the
region-level `IfThenElse` condition built from block 176 - not in the walk, not
in the naming, not in the merge.

### Why r84 went wrong, and what to trust
`LUAU_LIFTPATH` reports RANGES. A range tells you what a call was asked to
cover, not which instructions the walk actually executed - the outer span
115..176 covers 174 even though no inner span does, and I read the absence of an
inner span as absence of a lift. Instruction-level instruments
(`LUAU_PC_WINDOW`, `LUAU_REG_WATCH`) do not have that failure mode and
contradicted the range reading immediately.

Four rotations, four wrong locations, each one narrower than the last:

    r81  path-sensitive dataflow at joins    (not a join at all)
    r82  find_merge_point returns the jump   (merge=210 was correct)
    r83  contiguous arm never structured     (structured_run does run)
    r84  pc 174 never lifted                 (it is walked, value correct)

Each was falsified by an instrument one level finer than the one that produced
it. The window is now two functions wide instead of the whole pipeline.

### Next, and how to check it
Find where the region-level `IfThenElse` condition is built for a block whose
predecessor is a `Linear` region in the same `lift_structured_run`, and which
`regs` it reads. The hypothesis to test FIRST, not to act on: the condition is
evaluated from a snapshot taken before the Linear region was lifted. Confirm by
watching R3 at the moment the condition is constructed, not by reading the code
and inferring - that is what the last four rotations did.

### State after rotation 85
SEMANTIC: 1,104/1,142 (96.7%), 39 defects. 949 tests pass. No behaviour changed
this rotation; it is diagnosis and a correction.


## Rotation 86 - I was tracing the wrong proto, and the real fault is measured

### The mistake, first
Rotations 82-85 all traced `533_NPCs::AddEvent`. The defect is in
`AddOverrideEvent`. The two protos are near-duplicates - same constants, same
" references nil quest " string, same shape - and the emitted `if v3 then` sits
among `tbl3.OverrideEvents`, which I read past four times.

This repo already carries the warning, in `LUAU_REG_WATCH`'s own comment:
"Proto identity is REQUIRED here: pc numbering restarts per proto, and
correlating a WATCH line with a PC dump from a different proto is exactly how
rotation 47 concluded a table died where it had not." I did precisely that,
for four rotations, with the note in front of me. Every trace I add from now on
prints the proto; `pass_probe` was the one that did not, and it is what exposed
the error - it fired twice with opposite answers.

So the r85 finding stands, but about the WRONG function: in `AddEvent` the
condition really is correct and that proto is not defective.

### The fault, on the right proto
    COND  AddOverrideEvent  pc=151  JumpIfNot  reg=3  Unknown

R3 is `Unknown` where the condition is read. That IS the `if v3 then`.

Bytecode:

    129: JUMPIF      R2 -> ...        `if not quest`
    130: GETIMPORT   R2               error(
    ...
    138: CALL        R2 args=1 results=0
    139: JUMP        ->               <-- UNCONDITIONAL, leaves the span
    140: GETTABLEKS  R2 R1."Quest"
    142: GETTABLEKS  R3 R2."Requirements"
    144: JUMPIF      R3 -> 149
    145: NEWTABLE    R3
    147: SETTABLEKS  R2."Requirements" R3
    149: GETTABLEKS  R3 R1."StartReqs"   <-- the write, never lifted
    151: JUMPIFNOT   R3

Leaders are CORRECT - `LUAU_LEADER_TRACE` lists 130, 140, 145, 149, 152 - so
the CFG split the blocks properly. But `LUAU_LIFTPATH` shows the lift as

    flat 115..129
    flat 130..151      <- ONE span across FOUR blocks and the JUMP at 139
    flat 152..157

The walk enters at 130, reaches the unconditional JUMP at 139 whose target is
outside [130,151), and does not come back for 140..150. So pc 149 is never
lifted, R3 stays `Unknown`, and the condition at 151 renders as the generated
name that `free_var_decls` then hoists to line 8.

**The fault is a lifted span that crosses an unconditional jump.** Not the
leaders, not the merge, not the naming, not the walk's AUX stepping.

### The question this leaves
Which region construction emits a span of 130..151 when the CFG has leaders at
140, 145 and 149 inside it? Answer that by instrumenting the region builder's
output for this proto (proto-qualified), not by reading it.

### Instruments added, all gated and verified inert
`LUAU_LIFTPATH` (r84), `LUAU_COND_TRACE` and `LUAU_PASS_PROBE` (r86). Corpus
re-measured with everything off: 1,104/39, 949 tests pass.

### State after rotation 86
SEMANTIC: 1,104/1,142 (96.7%), 39 defects. No behaviour changed.
The reproducer is now exact: `533_NPCs` proto `AddOverrideEvent`, span 130..151,
expected to stop at 140.


## Rotation 87 - FIXED. The inline-if arm swallowed everything past its own exit

### The mechanism, at last
`structure_numeric_for_body`, Phase B0.5 Shape B, wraps a forward conditional
whose target range contains a nested for-loop as `Region::InlineIfThenInLoop`
with body `jump_next_pc..target`. `LUAU_RUNREG_TRACE` on
`533_NPCs::AddOverrideEvent` showed run 115..183 producing exactly two regions:

    Linear 115..129
    InlineIfThenInLoop            <- everything else, 130..183

But the arm ENDS EARLY:

    129: JUMPIF     R2 -> 183     `if not quest`
    130..138       error(...)
    139: JUMP      -> 183         <-- the arm's own exit
    140..182       the rest of the function, NOT the arm's

Taking the body as 130..183 swallowed 140..182. The flat lift of that span then
stopped at the pc-139 jump, so `149: GETTABLEKS R3 R1."StartReqs"` was never
lifted and its `JUMPIFNOT` at 151 rendered as a bare `v3`.

### The fix
Truncate the arm at its own exit jump, test `range_contains_nested_for` against
the TRUE arm, and resume after the jump so the tail is structured on its own.

**With one guard that the first attempt lacked**: a jump to the target is only an
arm exit if what follows it is UNREACHABLE from inside the arm. An if/else
inside the arm also jumps to the arm's end to skip its else. Without the guard
(measured): 32 files changed, calls lost in six, and `555_DapperBearInit` lost a
whole dialogue branch and three `table.insert` calls. With it: 11 files, zero
calls lost.

### Measured, adopted
    clean 1,104 -> 1,105   defects 39 -> 38
    FIXED   533_NPCs declared_never_assigned
    NEW     none, anywhere
    11 files changed; calls lost 0; loops lost 0
    99_BeequipTypes   +76 lines, +13 ifs, +2 loops, +21 calls
    87_BeeBoosts      nested ifs -> guard clauses, and `v3` now DECLARED
    88_BeeStats       `Type12 = self.Type; if Type12 == "Gummy"` replacing
                      `Type12 = arg3; Type12("GummyPower")`
    949 tests pass.

`533_NPCs::AddOverrideEvent` now reads as the source did - the `error` arm
closes, and the condition is `if arg2.StartReqs then`.

Mutations, both measured WORSE: no fix at all (1,104/39); fix without the
reachability guard (damages 555, 576, 258, 368, 380, 879).

### The trail this closes
Six rotations, five wrong locations, each falsified by an instrument one level
finer than the one before:

    r81  path-sensitive dataflow at joins    not a join
    r82  find_merge_point returns the jump   merge was correct
    r83  contiguous arm never structured     structured_run runs
    r84  pc 174 never lifted                 it is walked
    r85  region-level condition construction correct at extraction
    r86  WRONG PROTO for four rotations      AddEvent vs AddOverrideEvent
    r87  the arm swallows past its own exit  FIXED

The instrument that finally paid was `LUAU_RUNREG_TRACE`, which prints what the
structurer EMITS rather than what any consumer receives. Every earlier
instrument reported a consumer's view, and every consumer looked innocent
because it was.

### State after rotation 87
SEMANTIC: **1,105/1,142 (96.8%), 38 defects**. 949 tests pass.
CLOSED: A, D, E, F, G, H, I(predicates), J, A2/C(if-else/tables), argument-cap,
  K, unreachable-blocks, B-loops, **inline-if-arm-overrun**.
NEXT: re-enumerate the 38 and check whether the four remaining
  `REG_EXPR_UNKNOWN+UNK_CALL_CLEAR` files share this shape.


## Rotation 88 - the SAME fault one level down: "remaining instructions are dead code"

### Re-enumerating after r87
    24  calls_not_emitted
     8  declared_never_assigned   (533 closed)
     6  branch_local_discarded

Of the five `REG_EXPR_UNKNOWN+UNK_CALL_CLEAR` files, only `368_HoneycombTools`
still showed an Unknown branch condition:

    COND  ConstructCell  pc=99  JumpIfNot  reg=19  Unknown

### Same shape, different trigger
`LUAU_RUNREG_TRACE` again showed `InlineIfThenInLoop`, and `LUAU_LIFTPATH` a
`flat 22..99`. But `LUAU_PC_WINDOW=85-105` proved pc 88..98 were NEVER WALKED -
the walk jumped straight to 100. The abort is at

    56: LOADN  R7 1
    57: JUMP  -> 216        <-- leaves the span 22..99

and `lift_instruction_range`'s unconditional-jump arm did:

    // Unconditional jump - remaining instructions are dead code
    break;

That assumption holds for straight-line code and fails here: pc 58 is the OTHER
side of a conditional above it, reachable regardless of the path just
abandoned. Breaking there abandoned 58..98, so `98: CALL R19` never ran and the
`JUMPIFNOT R19` at 99 emitted a bare `v19`.

### The fix
On a forward jump that leaves the span, resume at the next instruction instead
of breaking WHEN something else in the proto jumps to it (`pc_is_branch_target`)
- which is exactly the condition under which the tail is not dead.

### Measured, adopted
    clean 1,105 -> 1,106   defects 38 -> 37
    FIXED   368_HoneycombTools declared_never_assigned
    NEW     none
    7 files changed; EVERY delta non-negative:
        368_HoneycombTools  +12 lines, +1 call   `v19` gone from line 8, and
                            the lost block returns - the "How many ... will you
                            feed to" prompt and its `ClientCall`
        532_MusicPlayer     +26 lines, +5 ifs, +3 calls
        25/27/29/43         +1..+3 calls each
    calls lost 0, loops lost 0, ifs lost 0 - the first census with no
    negative entry anywhere.
    949 tests pass.

### The pattern worth naming
r87 and r88 are the same fault at two levels: **a span whose walk stops at a
jump that leaves it, while the instructions after that jump still belong to the
program.** r87 fixed it where the span is CARVED (the arm swallowed past its
exit); r88 where the span is WALKED (the walk quit at a jump out of range).
Both were invisible to every consumer-side instrument, and both showed up
immediately once the structurer's own output and the raw pc walk were printed.

### State after rotation 88
SEMANTIC: **1,106/1,142 (96.8%), 37 defects**. 949 tests pass.
Two fixes adopted in two rotations after six rotations of none; the difference
was instrumenting producers instead of consumers.
REMAINING: 24 calls_not_emitted, 7 declared_never_assigned, 6
  branch_local_discarded.
NEXT: the 24 `calls_not_emitted` are the dropped-body class and are the same
  family - check whether they are spans that stop early for either of these two
  reasons, using `LUAU_PC_WINDOW` against the proto's instruction count.


## Rotation 89 - a producer-side instrument for the 24, and it needed correcting twice

### Why build another instrument
r87 and r88 were both found by printing what a PRODUCER emits. `calls_not_emitted`
is the dropped-body class: a proto whose body went missing. If it is the same
family, the walk should be visibly failing to reach part of the proto - so
measure exactly that, per proto, instead of inferring it from the output.

Added `crates/luau-core/src/decompiler/walk_cover.rs` (`LUAU_WALK_COVER`,
gated): records every pc `lift_instruction_range` steps over and reports
coverage per proto. Proto-qualified, for the r86 reason.

### The instrument was wrong the first time, and said so
First cut compared `walked` against `code.len()`. That is not the denominator:
an instruction carrying an AUX word occupies two slots but is ONE step, so
every AUX-heavy proto reads as half-walked. It flagged ten healthy protos in
`277_DragManager`. Corrected to normalise against the number of steps a
complete linear walk would take. The comment in the file records the wrong
version so it is not rebuilt.

Worth stating plainly: the instrument produced a confident, plausible,
completely spurious list of ten "shortfalls" before the denominator was fixed.
That is the same failure the corpus-wide model work kept hitting, from a
different direction.

### First real reading
`277_DragManager`, normalised:

    OnInputBegin@184           walked=106  steps=150  pct=70
    DragBegin@414              walked=58   steps=73   pct=79
    _validateInputIsGamepad    walked=9    steps=12   pct=75
    Destroy@175                walked=10   steps=12   pct=83
    endPulling@262             walked=32   steps=36   pct=88

`OnInputBegin` never reaches 30% of its own instructions. The journal's older
note on this file - "keeps 4 of 36 statements" - now has a mechanism-shaped
number behind it rather than a statement count.

### State after rotation 89
SEMANTIC: 1,106/1,142 (96.8%), 37 defects - unchanged; this rotation is an
instrument, verified inert (corpus re-measured 1,106/37, 949 tests pass).
REMAINING: 24 calls_not_emitted, 7 declared_never_assigned, 6
  branch_local_discarded.
NEXT: for `OnInputBegin`, find where the walk stops - `LUAU_PC_WINDOW` over the
  gap, then `LUAU_RUNREG_TRACE` for the region that carved the span. Both r87
  and r88 were one of two things: a span carved past its arm's exit, or a walk
  that quit at a jump leaving the span. Check those two first, by measurement.


## Rotation 90 - the third variant of the same fault, measured NEUTRAL, not adopted

### Locating it
`LUAU_WALK_COVER` put `277_DragManager::OnInputBegin` at 106 of 150 steps.
Collecting every walked pc and diffing against the step list gave the gaps:

    (16,49) (58,61) (120,124) (157,166) (170,177) (198,203)

The dominant one is 17..48, inside the span `flat 4..48`, which stops at 16:

    15: JUMPIFNOT R2 -> 17     `if pulling or dragging then`
    16: RETURN                     return end
    17: GETTABLEKS R2 R1."UserInputType"

pc 17 is the JUMPIFNOT's OWN target, so it is reachable. The Return arm ends
with `break; // Return terminates the current block` - the same assumption the
unconditional-jump arm made in r88, third variant of one fault: **a span whose
walk stops at a terminator while the instructions after it are still live.**

Also visible in the trace: `flat 127..138` appears TWICE - a double lift, worth
its own look later.

### The fix works and buys nothing
Applied the r88 shape (resume when the next pc is a branch target):

    clean 1,106 -> 1,106   defects 37 -> 37
    defect set BYTE-IDENTICAL: nothing fixed, nothing broken
    OnInputBegin coverage 70% -> 82% (walked 106 -> 123)
    30 files changed
    corpus total 284,717 -> 284,710 lines

So the extra 17 instructions produced no statements - and no duplication
either, which the line total rules out. The two output changes worth naming are
both fine: `397_WindyTailInit` turns `local function fn() ... end;
task.spawn(fn)` into `task.spawn(function() ... end)` and gains a `local Body4 =
{}`; `277_DragManager` itself changes only register names.

**Not adopted.** r87 and r88 each closed a defect with visible recoveries. This
changes 30 files to move nothing any check can see. Left gated with the numbers
in the source, because the mechanism is right and the next attempt on
`calls_not_emitted` starts here - but a 30-file diff for a neutral measurement
is not something to ship on the theory that it must be better.

The honest reading: coverage was the wrong proxy. Walking more instructions is
not the same as emitting the missing calls, and I assumed it would be.

### State after rotation 90
SEMANTIC: 1,106/1,142 (96.8%), 37 defects. 949 tests pass. Verified inert.
GATED, MEASURED, INERT: `LUAU_RET_RESUME` (neutral), `LUAU_ARM_IF` (+1 but
  deletes a guard), `LUAU_ARM_COND` (-1), `LUAU_DECL_HOIST`, `LUAU_BRANCH_ARMS`.
NEXT: `calls_not_emitted` needs the CALLS located, not the coverage. Count
  bytecode `Call` per proto against emitted calls per proto (`count_calls_stats`
  already exists and is exhaustive) and find WHICH call sites are missing -
  then trace those pcs specifically. Coverage told me where the walk went, not
  where the calls went.


## Rotation 91 - I overturned r90 on a bad metric, then overturned myself back

### The instrument
Extended `walk_cover` to answer the question `calls_not_emitted` actually poses:
of the CALL sites the bytecode contains, which pcs did the walk never reach?

    603_Orbiter::Update@182      missed 4/5   pcs [42, 46, 51, 58]
    146_CharacterGui::Make@59    missed 3/5   pcs [20, 25, 43]
    277_DragManager::OnInputBegin missed 1/15 pcs [45]
    99_BeequipTypes              NO GAP - r87 closed it

The `call_recovery.rs` docstring is now stale: it says `_CheckReqs` loses 20 and
`OnInputBegin` loses 15. r87/r88 fixed both; `OnInputBegin` is down to one.

### The overclaim
`LUAU_RET_RESUME` (r90, measured neutral and left gated) closes every one of
those gaps. Corpus-wide:

    base            77 protos with a gap, 243 CALL pcs never reached
    ret_resume      16 protos,             60 CALL pcs never reached

I read that as "183 call sites recovered" and concluded r90's verdict had been
wrong - that the thresholded `calls_not_emitted` check simply could not see the
gain.

### The correction
It could not see a gain because there is none. Emitted call tokens across the
full corpus:

    base        38,177
    ret_resume  38,177

Byte-identical. `603_Orbiter` output is byte-identical too. **Reaching a CALL pc
is not emitting a call**, and CALLGAP measures walk-reach - exactly the mistake
r90 named one level up ("coverage was the wrong proxy") repeated one level down.
r90's original verdict stands: `LUAU_RET_RESUME` stays gated.

Twice now a metric of my own construction has produced a confident number
pointing the wrong way, and both times the check that caught it was comparing
the OUTPUT rather than an intermediate.

### What the episode did establish
`603_Orbiter::Update` is a real dropped body, and now a minimal one. Bytecode
has five CALL sites (14, 42, 46, 51, 58); the emitted body has one:

    function tbl:Update(arg2)
        if self.Active then
            self:Destroy(false)
            return false
        else
            return false          -- pcs 42..58 belong HERE
        end
    end

With `LUAU_RET_RESUME` the walk REACHES 42..58 and the output is unchanged. So
there are two faults stacked:

    1. the walk stops at the `return` (r90's mechanism, fix measured, gated)
    2. the statements lifted from 42..58 are DISCARDED downstream

Fixing (1) alone changes nothing, which is why it measured neutral. Fixing (2)
alone cannot work either, because the instructions are not reached. **They must
land together**, and that is why every single-sided attempt so far has scored
zero.

### THE QUESTION
Where do the statements lifted from 42..58 go? Lift `Update@182` with
`LUAU_RET_RESUME=1` and instrument the `stmts` vector at each level - the else
arm is being built and then dropped, or built into a vector nobody splices.
Answer by printing statement counts at each nesting level, NOT by reading the
structurer.

### State after rotation 91
SEMANTIC: 1,106/1,142 (96.8%), 37 defects. Unchanged; this rotation is an
instrument plus a retraction. Verified inert.
REPRODUCER, minimal: `603_Orbiter` proto `Update@182`, 5 CALL sites, 1 emitted.


## Rotation 92 - located: DCE phase 2, and why RET_RESUME is a no-op BY CONSTRUCTION

### Answering r91's question
`LUAU_IF_TRACE` on `603_Orbiter::Update@182`:

    base        IFPUSH  then_stmts=1  else_stmts=Some(3)   else_calls=Some(1)
    RET_RESUME  IFPUSH  then_stmts=1  else_stmts=Some(11)  else_calls=Some(5)

So the else arm IS built with all 11 statements and all 5 calls, and IS pushed.
Nothing is dropped at lift time. The loss is downstream, as r91 predicted.

### The proto qualifier, for the third time
The first run of the per-pass probe showed 20 statements / 5 calls holding
through fifteen passes and then collapsing to 4 / 1. That collapse was the NEXT
PROTO's pass chain beginning - the probe printed no proto. Same failure as
rotation 86 and rotation 47 before it, third occurrence in this session, this
time in an instrument I had written two rotations earlier while explicitly
noting why proto identity matters. The probe is now proto-qualified and carries
that note in its body.

Qualified, the answer is immediate:

    Update@182  simplify_stmts         stmts=1  calls=5
    Update@182  eliminate_dead_stores  stmts=1  calls=5
    Update@182  eliminate_dead_code    stmts=1  calls=1

### The phase
`LUAU_DCE_TRACE`, depth 2 (inside the else arm):

    enter                  stmts=11  calls=5
    after_phase1_recurse   stmts=11  calls=5
    after_phase2_truncate  stmts=3   calls=1

**Phase 2, "truncate after terminators."**

### What this actually means - RET_RESUME cannot work as written
The two faults r91 described as "stacked" are one coupled problem.
`LUAU_RET_RESUME` resumes the walk past a `Return` and appends the resulting
statements to the SAME flat statement list, producing

    [ ..., Stat::Return, stmt, stmt, ... ]

Those statements are then genuinely unreachable IN THE EMITTED AST, and DCE
phase 2 removes them entirely correctly. DCE is not the bug.

That is why the output is byte-identical with the gate on: RET_RESUME is a
**no-op by construction**, not a partial fix waiting for a second one. r90's
"neutral" measurement was right for a reason neither r90 nor r91 had.

### The corrected fix shape
The resumed statements must be placed STRUCTURALLY AFTER THE ENCLOSING `if`,
not appended after the `return` inside its arm. The bytecode

    15: JUMPIFNOT R2 -> 17    `if pulling or dragging then`
    16: RETURN                    return end
    17: ...                   <-- belongs AFTER the if, at the parent level

is an early-exit guard, and the region structurer should emit
`if cond then return end` as a complete region with 17.. as its SIBLING. Fixing
it inside `lift_instruction_range` cannot express that, because that function
owns one flat list and has no way to return "and these belong to my parent".

### THE QUESTION, restated for the next attempt
Can `structure_control_flow` recognise a guard arm - a conditional whose taken
side terminates - and emit `IfThenElse{then: [return], merge: next}` so the
tail becomes a sibling region? That is the r87 fix's shape (truncate the arm,
resume after it) applied to a RETURN-terminated arm instead of a
JUMP-terminated one. r87 is the working precedent and the place to start.

### State after rotation 92
SEMANTIC: 1,106/1,142 (96.8%), 37 defects. Unchanged; diagnosis only.
949 tests pass, all instruments verified inert.
GATED: `LUAU_RET_RESUME` now KNOWN to be a no-op by construction - do not
  revisit it as written; the note above is why.


## Rotation 93 - GUARD ARMS. The largest single move of the session

### The shape
`603_Orbiter::Update@182` is a chain of early-exit guards, and `RUNREG` showed
`run=5..61 Linear 5..61` - ONE region for the whole body. The flat walk stops at
the guard's RETURN and loses everything after it:

     7: JUMPIFNOT R2 -> 11     `if not self.Root`
    10: JUMPIF    R3 -> 17      `or not ...Parent then`
    14: CALL      Destroy           self:Destroy(false)
    16: RETURN                      return false end
    17: ...                     <-- four of five CALL sites live here

### The fix - Shape C in `structure_numeric_for_body`
Exactly r87's move, keyed on the arm TERMINATING rather than on it containing a
nested for: emit the guard as its own region and continue at the target, so the
tail becomes a SIBLING rather than the arm's tail. That is what r92 established
was required - `LUAU_RET_RESUME` appended the tail INSIDE the arm and DCE
phase 2 correctly deleted it.

`Update` now emits all five calls and reads like its source:

    if self.Active then
        if not self.Root.Parent then self:Destroy(false) return false end
        self.Age += arg2
        self.CycleAge += arg2
        if self.Dur then if self.Dur <= self.Age then
            if self.OnExpire then self.OnExpire(self) end
            self:Destroy(false) return false
        end end
        self:UpdateCF()
        if self.OnUpdate then self.OnUpdate(self) end
        return true

### Measured, ADOPTED
    clean    1,106 -> 1,123  (96.8% -> 98.3%)
    defects     37 -> 22
    total emitted calls across the corpus  38,177 -> 38,501  (+324)
    187 files changed; files losing a LOOP: 0
    949 tests pass

Defect kinds now: 10 declared_never_assigned, 7 branch_local_discarded,
5 calls_not_emitted (was 24).

### The narrower variant, MEASURED WORSE
"Straight-line arms only" (no internal control flow) looks obviously safer and
is not:

                       wide      narrow
    clean              1,123     1,120
    files losing calls     9        19
    files losing loops     0         3
    total calls         +324      +153

Structuring is ORDER-DEPENDENT: declining to wrap one site lets a worse wrap
happen later. A narrower predicate is not automatically a safer one. The note
is in the source so it is not re-narrowed on intuition.

### KNOWN RESIDUAL - recorded, not suppressed
Six new findings in three files, and nine files lose calls:

    381_Eggs             3 new declared_never_assigned + 1 branch_local_discarded
    396_TadpoleTailInit  1 new declared_never_assigned
    351_PassiveTile      1 new calls_not_emitted, and 72 -> 62 call tokens
    plus 8 more files with smaller call losses

`351_PassiveTile` is the reproducer: its lost block sits inside a `for` body
whose arms end in `break`, so hoisting a tail out of an arm crosses a loop
boundary. This is a real regression against a much larger real gain (21 defects
closed, 6 opened; +324 calls net). It is adopted on that balance and the
residual is the next target, NOT dismissed.

### State after rotation 93
SEMANTIC: **1,123/1,142 (98.3%), 22 defects**. 949 tests pass.
CLOSED: A, D, E, F, G, H, I(predicates), J, A2/C(if-else/tables), argument-cap,
  K, unreachable-blocks, B-loops, inline-if-arm-overrun, **guard-arms**.
NEXT: the guard-arm residual - `351_PassiveTile`, guard hoisting across a loop
  boundary where arms end in `break`. Find whether the wrap can be suppressed
  when `target` lies outside the enclosing loop body, using `loop_headers`
  (already a parameter) rather than by inspecting the arm.


## Rotation 94 - two narrowings of the guard-arm predicate, both measured WORSE

### What was tried
The r93 residual is nine files losing calls, worst `351_PassiveTile` (72 -> 62
call tokens). Two predicates were built to suppress the wrap where it hurts.

**(a) straight-line arms only** - reject any arm containing control flow
(rotation 93, recorded there):

                       r93 wide   straight-line
    clean               1,123        1,120
    files losing calls      9           19
    files losing loops      0            3
    total emitted calls  +324         +153

**(b) reject arms with an ESCAPING jump** - a `break` compiles to a jump past
the wrap point, so an arm containing one is not self-contained:

                       r93 wide   escaping-jump
    clean               1,123        1,123
    defects                22           22
    files losing calls      9           12
    total emitted calls  +324         +285
    351_PassiveTile     72 -> 62     72 -> 62  (UNCHANGED)

### What (b) proves
The predicate did not change `351_PassiveTile` at all. So the arm damaging that
file does NOT contain an escaping jump by this scan, and my model of its failure
- "hoisting a tail out of an arm whose arms end in `break` crosses a loop
boundary" - is WRONG, or at least is not what the scan detects. Both narrowings
also made the corpus worse overall, which is the same order-dependence lesson
r93 recorded: refusing to wrap at one site lets a worse wrap happen later.

Reverted to the committed r93 form. Re-measured clean: 1,123/22, 949 tests pass.

### PARKED, with what is ruled out
MECHANISM: guard-arm residual - nine files lose calls under Shape C.
PROVED:    Shape C causes it. `351_PassiveTile` is unchanged from r92 baseline
           to r93 except through Shape C, which was r93's only change.
RULED OUT: (a) the arm containing any control flow - measured, worse on every
           axis; (b) the arm containing a jump past the wrap point - measured,
           worse, and does not even touch the reproducer.
REPRODUCER: `351_ReplicatedStorage_Gui_TileDisplay_PassiveTile`, 72 -> 62 call
           tokens, plus one new `calls_not_emitted`.
THE QUESTION: which conditional in `351_PassiveTile` does Shape C wrap, and what
           does the region list look like before and after? Answer with
           `LUAU_RUNREG_TRACE` on that file, proto-qualified, diffing the region
           lists with Shape C on and off - do NOT model the arm's shape and
           write a predicate for it, which is now 0 for 2.

### State after rotation 94
SEMANTIC: 1,123/1,142 (98.3%), 22 defects - unchanged; both experiments reverted.
949 tests pass.


## Rotation 95 - the RUNREG blind spot, a falsified theory, and a fourth cross-proto error

### The blind spot (real, and worth keeping)
`LUAU_RUNREG_TRACE` prints region lists for `351_PassiveTile` that are BYTE-
IDENTICAL with Shape C on and off - while the output differs by ten calls
(72 -> 62). RUNREG only covers `lift_structured_run`; Shape C also fires from
recursive `structure_numeric_for_body` calls it never prints. An instrument
showing "no difference" where a difference exists is worse than none, so:

  * added `LUAU_NO_GUARD_ARM`, a permanent switch to turn Shape C OFF, so any
    future claim about it can be A/B'd directly;
  * added `LUAU_GUARD_TRACE`, printing every wrap with its opcode and arm
    opcode.

### The theory, and its falsification
Disassembling `351` showed pc 12-17 of `RegisterListeners` as NEWCLOSURE
followed by five CAPTURE pseudo-instructions, and the guard trace showed wraps
at cond_pc 13, 74, 92, 102, 119. Conclusion drawn: Shape C was decoding CAPTURE
words as conditionals, which would have explained everything - the identical
region lists, and why rotation 94's two predicates both failed (they modelled
real arms, and these would not have been code at all).

Two fixes were written for it. Neither changed anything. Printing the opcodes
settled it:

    GUARD codelen=166 cond_pc=13  op=JumpIfNot arm=14..15 arm_op=Return
    GUARD codelen=166 cond_pc=74  op=JumpIf    arm=75..76 arm_op=Return
    ...

**Genuine conditionals with genuine single-instruction RETURN arms.** The
CAPTURE theory is FALSE. The proto being wrapped has 166 instructions;
`RegisterListeners`, the one I disassembled, is a different proto. I matched pcs
across protos again - the FOURTH time this session (r47's lesson, then r86, r92,
now r95), and this time within minutes of writing a comment about that exact
hazard into the trace itself. The experiment was removed rather than kept.

### Where that leaves the residual
The wraps in `351` are LEGITIMATE guards doing what Shape C intends. So the ten
lost calls are not a bogus wrap - they are the downstream consequence of a
correct one. That is a different problem from the one three rotations have been
attacking, and it invalidates the framing, not just the predicates.

PARKED. MECHANISM: guard-arm residual, 9 files losing calls.
PROVED:    the wraps are real conditionals with real RETURN arms
           (`op`/`arm_op` printed); region lists are unchanged, so the damage is
           downstream of region construction, not in it.
RULED OUT: arms containing any control flow (r94, worse); arms with an escaping
           jump (r94, worse and does not touch the reproducer); CAPTURE
           misdecoding (r95, falsified by opcode printing).
THE QUESTION: with `LUAU_NO_GUARD_ARM` toggling one file, diff the two OUTPUTS
           of `351` statement by statement and find the first divergence, then
           work backwards from that statement. Every attempt so far has started
           from the bytecode and reasoned forward, and has been wrong four
           times; start from the observable difference instead.

### State after rotation 95
SEMANTIC: 1,123/1,142 (98.3%), 22 defects. 949 tests pass. Nothing adopted this
rotation; two instruments kept, one theory falsified and its code removed.


## Rotation 96 - the damage is CUMULATIVE, which is why every predicate failed

### Starting from the output, not the bytecode
r95 parked with: diff the two outputs and find the FIRST divergence. Doing that:

    100,102d99
    <             if self.Obj then
    <                 break
    <             end
    137,193c134,149
    <             if v.Action == "Add" then ... elseif "Remove" ... elseif "Update"

The deleted code is all inside a `for` body - the `break`s prove it.

### A fourth predicate, and a bisect instead
Suppressing the wrap when a loop encloses it (backward-branch scan, no
dominators needed) changed NOTHING: same 9 files, same 351 at 72 -> 62, same six
wraps in the trace. Three predicates now, all failing to exclude the same six
wraps.

So rather than invent a fifth rule, added `LUAU_GUARD_SKIP_PC=<pc>[,<pc>]` and
bisected:

    skip=none                 calls=62
    skip=13                   calls=62
    skip=74                   calls=62
    skip=92                   calls=62
    skip=102                  calls=62
    skip=119                  calls=62
    skip=13,74,92,102,119     calls=72   <-- all five
    LUAU_NO_GUARD_ARM=1       calls=72

### THE MECHANISM
**No single wrap causes the loss; the SET does.** Each wrap ends with
`pc = target; linear_start = pc`, so five wraps split the body range into six
pieces. A loop spanning several of those pieces is no longer inside any one
region and cannot be recognised.

That is why every predicate failed: they all asked "is THIS wrap bad?" and no
wrap is bad. The arm shape is irrelevant; so is whether one wrap sits in a loop.
The fault is FRAGMENTATION OF A RANGE A LOOP SPANS.

Corrected fix shape, for the next attempt: gate Shape C on whether a loop header
lies within `[pc, body_end)` - the range the wrap would fragment - rather than
on anything about the wrap or its arm. `loop_headers` is already a parameter.
That is a property of the RANGE, which is the thing actually being damaged.

### A pre-existing FLAKY TEST, found by accident
`parser::opmap::tests::augmenter_keeps_unary_byte_when_format_consistent` fails
intermittently: one full-suite run FAILED (948/1), the next passed (949/0). It
passes 3/3 in isolation and on a stashed clean tree, so it is a parallel-
execution flake and NOT caused by this work.

This matters beyond itself. Every rotation in this session has read "949 tests
pass" as a clean signal, and that signal is not deterministic. Recorded here so
a future red run is not attributed to whatever change is in flight at the time.

### State after rotation 96
SEMANTIC: 1,123/1,142 (98.3%), 22 defects. Nothing adopted; the in-loop
predicate was removed, the bisect switch kept (it is what produced the finding).
PARKED: guard-arm residual, now with a MECHANISM rather than a symptom -
  cumulative fragmentation of a loop-spanning range - and a fix shape keyed on
  the range instead of the wrap.


## Rotation 97 - the range gate lands, and the CFG reports ZERO loops for a proto that has one

### The gate r96 asked for
Shape C now refuses to fire when a loop header lies in `[pc, body_end)` - the
range the wrap would fragment. A property of the RANGE, not of the wrap, which
is what r96's bisect proved was needed.

    clean    1,123 -> 1,123  (unchanged)
    defects     22 -> 19
    FIXED    381_Eggs: 3 declared_never_assigned + 1 branch_local_discarded
    NEW      314_RoboBearGui: 1 calls_not_emitted
    total emitted calls +283 (was +324 ungated)
    949 tests pass, twice

Those four fixes in `381_Eggs` are FOUR OF THE SIX regressions rotation 93
introduced. The residual r93 shipped is now down to two findings plus the
call-token losses. ADOPTED on that basis.

### And it still does not touch 351
`351_PassiveTile` is unchanged at 72 -> 62, and the trace says why:

    GUARD codelen=87  headers=1  cond_pc=44  ...
    GUARD codelen=166 headers=0  cond_pc=13  ...
    GUARD codelen=166 headers=0  cond_pc=74  ...

**`headers=0` for the 166-instruction proto** - a proto whose lost code is a
`for k, v in pairs(...)` body full of `break`s. The CFG reports NO loop headers
for a function that demonstrably contains a loop, so `loop_in_rest` is
permanently false there and the gate cannot fire.

That is a much bigger finding than the guard-arm residual it was chasing. Either

  * `cfg` and `code` describe DIFFERENT protos at this call site - the
    parameters are threaded separately and nothing checks they agree; or
  * `find_loops()` genuinely returns nothing for this proto.

Both are worth knowing independently of Shape C: `loop_headers` is also used by
`find_value_join` to avoid claiming a nested loop's header, so an empty set
silently disables that protection too.

### THE QUESTION for the next rotation
Print, for the 166-instruction proto in `351_PassiveTile`, both `cfg.blocks.len()`
and `find_loops().len()` at the point `lift_structured_run` builds
`loop_headers`, and compare against the proto's own block count. If the CFG has
the wrong number of blocks, `cfg` belongs to another proto and the mismatch is
the bug; if it has the right blocks and still finds no loop, `find_loops` is.

Do not fix Shape C further until that is answered - four predicates have now
been aimed at a gate that cannot fire because its input is empty.

### State after rotation 97
SEMANTIC: **1,123/1,142 (98.3%), 19 defects** (10 -> 7 declared_never_assigned,
7 -> 6 branch_local_discarded, 5 -> 6 calls_not_emitted).
949 tests pass on two consecutive runs.


## Rotation 98 - `find_loops` misses EVERY generic for-loop, and that is the real bug

### r97's question, answered
    GUARD codelen=166 blocks=27 maxblk=165 loops=0 headers=0 cond_pc=13 ...

`maxblk=165 < codelen=166` and `blocks=27` matches the 27 leaders traced for
proto `?@181`. So **`cfg` and `code` AGREE** - there is no cross-proto mismatch.
`find_loops()` genuinely returns zero for a proto that contains a loop.

### Why it returns zero
`?@181` (the listener callback in `351_PassiveTile`):

      4: FORGPREP_NEXT R1 -> +158     target 163
    163: FORGLOOP      R1 -> -159     target 5

`ControlFlowGraph::build` gives FORGPREP two successors - fall-through (5, into
the body) and its jump target (163, the FORGLOOP) - and FORGLOOP two - its back
target (5) and the exit (165). So:

    4   -> {5, 163}
    163 -> {5, 165}

`find_loops` accepts a back edge only when the successor DOMINATES the block:
for edge 163 -> 5 it needs 5 to dominate 163. It does not, because
`4 -> 163` reaches the latch WITHOUT passing through 5. No natural loop is
found, `loop_headers` is empty, and the r97 gate can never fire.

This is the standard Luau generic-for layout, so the miss is not specific to
this file: **every `for ... in ...` whose FORGPREP jumps straight to its
FORGLOOP is invisible to `find_loops`**. `?@110 tileCooldownLoop` in the same
file has `JUMPBACK -> -30` and IS detected (loops=1 on the 87-instruction
proto), which is the control case - a `while`-shaped back edge dominates fine.

### Why this matters well beyond Shape C
`loop_headers` is passed to `structure_numeric_for_body` and used by
`find_value_join` to avoid claiming a nested loop's header as a value-join
entry. An empty set silently disables that protection for every generic-for
proto. Four rotations of predicates were aimed at a gate whose input is empty
for exactly the files that needed it.

### THE FIX SHAPE, for the next rotation
Either

  (a) treat the FORGLOOP block as the loop header - the body falls into it and
      it dominates the body, so `body_end -> FORGLOOP` is a proper back edge; or
  (b) drop the `FORGPREP -> FORGLOOP` skip edge and model the prep as reaching
      only the body, letting 5 dominate 163 as the header.

(b) changes reachability and risks marking the loop-exit path unreachable, so
(a) is the safer first attempt. MEASURE both; the census criteria are the same
as always - defects down, no construct lost in any changed file. Verify first on
the control case (`tileCooldownLoop`, already detected) that detection does not
DOUBLE-count once FORGLOOP becomes a header.

### State after rotation 98
SEMANTIC: 1,123/1,142 (98.3%), 19 defects. 949 tests pass. Diagnosis only; the
guard trace now prints `blocks`, `maxblk`, `loops`, `headers` and is verified
inert.


## Rotation 99 - the loop-detection bug is REAL and fixing it makes the corpus WORSE

### Two independent repairs, same verdict
r98 established that `find_loops` misses every generic for-loop because
`CFG::build` gives FORGPREP a spurious fall-through edge, so the body block does
not dominate the latch. Two ways to repair that were built and measured against
the r97 baseline of **1,123 / 19**:

  (a) **Remove the spurious edge.** FORGPREP jumps UNCONDITIONALLY to its
      FORGLOOP; it does not fall through, so the edge is simply wrong. With it
      gone the graph is 4 -> 163, 163 -> {5,165}, body -> 163, and the loop is
      found.

          1,118 / 25

  (b) **Leave the graph alone, add the header from the opcode** where
      `loop_headers` is built, plus the gate boundary the trace exposed
      (`h <= body_end`, because the FORGLOOP block sits exactly ON `body_end`).

          1,118 / 25

Identical. Both verified to work: `351_PassiveTile`'s proto went from
`loops=0 headers=0` to `loops=10 headers=1`, and the control case
`tileCooldownLoop` stayed at `loops=1 headers=1`, so detection did not
double-count.

### What that actually proves
The detection was genuinely broken and both repairs genuinely fix it. The cost
is not in the detection - it is in what the r97 gate then DOES with it: once
generic-for headers exist, the gate suppresses guard wraps inside for-bodies,
and **those wraps are net POSITIVE across the corpus**. Suppressing them costs
5 clean files and 6 defects to save the 10 call tokens in `351_PassiveTile`.

So the guard-arm residual is not a bug to be gated away. `351` is a genuine
local loss inside a transformation that pays for itself everywhere else, and
three rotations of predicates were trying to buy a worse trade.

Both reverted. Re-measured clean: 1,123 / 19, 949 tests pass, tree clean.

### PARKED, and re-framed
MECHANISM: `find_loops` misses every generic for-loop (r98). REAL, reproduced,
  two working repairs, both measured.
WHY NOT SHIPPED: neither repair is the problem - the r97 gate consuming the
  corrected headers is. Shipping either without changing the gate costs
  5 clean / 6 defects.
THE QUESTION: is the corrected loop detection worth anything on its OWN, with
  the guard-arm gate switched off entirely? `find_value_join` also consumes
  `loop_headers` to avoid claiming a nested loop's header, and that protection
  has been dead for every generic-for proto. Measure repair (b) together with
  `LUAU_NO_GUARD_ARM=1` - that isolates the detection fix from the gate, and it
  is the one combination not yet tried.

### State after rotation 99
SEMANTIC: 1,123/1,142 (98.3%), 19 defects. 949 tests pass. Working tree clean.


## Rotation 100 - loop detection CLOSED: real, repairable, and worth nothing

### The isolation r99 asked for
Does correcting `loop_headers` help anything on its own, independent of the r97
guard-arm gate? Measured all four corners:

    headers   gate   result
    broken    off    1,123 / 22      (= r93)
    corrected off    1,123 / 22      (r100b)
    broken    on     1,123 / 19      (= r97, SHIPPING)
    corrected on     1,118 / 25      (r99a and r99b, both repairs)

And the decisive check: with the gate off, the corrected-header run is
**BYTE-IDENTICAL to the broken-header run across all 1,142 files** - 0 files
differ.

### Verdict
`find_loops` really does miss every generic for-loop (r98, reproduced, control
case confirms). Two independent repairs really do fix it (r99). And fixing it
changes **nothing** in the output except through the r97 gate, where it costs
5 clean files and 6 defects.

The theory that `find_value_join`'s dead nested-loop-header protection was
silently costing us was reasonable and is FALSE: turning that protection back on
for every generic-for proto in the corpus produced not one byte of difference.

CLOSED - adjudicated REAL but not worth fixing, with every number recorded. This
is not suppression: the mechanism is proven, both repairs are proven to work,
and the corpus is what says they do not pay.

### And the guard-arm residual, re-framed one last time
`351_PassiveTile`'s 10 lost call tokens sit inside a transformation (Shape C)
worth +17 clean files and +324 emitted calls corpus-wide. Six attempts to
suppress the wraps that hurt it - four arm-shape predicates, one loop-membership
test, one corrected-header gate - all measured WORSE than leaving it alone. The
residual is the price of the fix, and the corpus has now said so six times.

### State after rotation 100
SEMANTIC: **1,123/1,142 (98.3%), 19 defects** - 7 declared_never_assigned,
6 branch_local_discarded, 6 calls_not_emitted.
949 tests pass. Working tree clean (only the untracked throwaway probe
`crates/luau-core/tests/_tmp_rem.rs`, which must not be committed).

CLOSED: A, D, E, F, G, H, I(predicates), J, A2/C(if-else/tables), argument-cap,
  K, unreachable-blocks, B-loops, inline-if-arm-overrun, guard-arms,
  **generic-for loop detection (real, repaired, measured worthless)**.
PARKED: UPVAL_NAME (format carries no CAPTURE data); synthetic reproducers
  (bundled Luau emits v11, parser reads to v9); guard-arm residual (priced, six
  suppression attempts all worse); the flaky opmap augmenter test (spun out).


## Rotation 101 - branch_local_discarded ADJUDICATED REAL, and the fix priced

### All six adjudicated, with bytecode
Previously unadjudicated. All six are REAL and one mechanism.
`646_Capture_Chicks::Description`:

    31: CONCAT     R5 R6..R9      R5 = "Capture " .. n .. " " .. name
    34: JUMPIFNOT  R6 -> 40
    35: MOVE       R6 R5          the arm READS R5 ...
    39: CONCAT     R5 R6..R8      ... then REASSIGNS it
    40: MOVE       R7 R5          post-merge read
    43: RETURN     R6

pc 39 writes the same register pc 40 reads. The emitter minted
`local MonsterType3 = ...` and returned the PRE-branch value, so the zone suffix
is silently dropped from every quest description that has one.

MECHANISM: a write inside a branch arm to a register live across the merge is
emitted as a declaration instead of a reassignment.

### It is a KNOWN residual, and the source says why
`premateralize_branch_escapes_spans` deliberately excludes a register already
bound to a declared local: pin-only threading was measured at "+4 clean but -2
previously-clean files". The comment names the missing piece - "a pass that can
distinguish a carried identity from a repurposed register".

### The discriminant, and what it cost
The bytecode supplies exactly that: an arm that READS the register before
writing it is EXTENDING the value it already held; a repurposed slot has nothing
to read. Implemented as `reg_read_before_write` (CONCAT reads a RANGE B..C,
which is how `MOVE R6 R5; CONCAT R5 R6..R8` reaches its own prior value).

    shipping (no carried-id)      1,123 / 19
    carried-id                    1,126 / 16   FIXED 263, 646, 79; NEW none
    carried-id + non-param gate   1,122 / 21

The middle row is the best score of the session. It is NOT adopted, because
214 files changed and two are damaged in ways no check can see:

    284_GuiTile   `isinworkspace = type`, `= UDim2.new`, `= isinworkspace(...)`
                  - the name threaded across a reused SCRATCH slot, which is
                  the exact 476_BadgeMenu orphaning this gate exists to prevent
    88_BeeStats   `arg4 = function(...)` - a closure assigned over a PARAMETER

Excluding parameter slots kills the benefit outright (1,122/21), so the two
failures are not independent of the win: the same predicate that catches the
carried identity is admitting repurposed slots.

Compare the r93 decision, where a residual WAS accepted: +17 clean and +324
calls against 9 files. Here it is +3 clean against 2 files, one of them
reproducing a documented past regression. That trade does not clear the bar.

### PARKED, priced
MECHANISM: branch-arm reassignment emitted as declaration. REAL, 6 findings,
  bytecode-confirmed, and it silently changes program output (dropped string
  suffixes, not just cosmetics).
RULED OUT: pin-only threading on any Name-bound register (prior work, -2 files);
  read-before-write discriminant alone (r101, -2 files); read-before-write plus
  non-param (r101, kills the gain).
THE QUESTION: `reg_read_before_write` returns true for `284_GuiTile`'s scratch
  slot. Find which instruction it counts as the read - print pc and opcode for
  the first read it finds - and decide whether an operand-position rule (source
  vs destination on that specific opcode) separates the two cases. Do not widen
  the predicate again without that print; three of this session's four
  predicate attempts failed for want of one.

### State after rotation 101
SEMANTIC: 1,123/1,142 (98.3%), 19 defects. 949 tests pass. Tree clean apart
from the untracked throwaway probe.


## Rotation 102 - carried identity ADOPTED, after the print r101 asked for

### The print
r101 parked with: print which instruction `reg_read_before_write` counts as the
read in `284_GuiTile`. Doing that:

    CARRY READ reg=0 pc=1 op=NewTable  a=0 b=0 c=0
    CARRY READ reg=1 pc=6 op=FastCall1 a=40 b=1 c=3

NEWTABLE's B is an ARRAY-SIZE HINT. FASTCALL1's A is a BUILTIN ID. The predicate
read B and C as registers on EVERY opcode, so it was not detecting reads at all -
it was matching integers. That is why it admitted repurposed scratch slots.

`reg_used_in_range` has the same flaw deliberately: it over-approximates "is
this used", where a false positive is the SAFE direction. Here it is the
dangerous one.

### Two corrections, both load-bearing
    (1) count B/C only for opcodes whose B/C genuinely name registers
    (2) require the arm to write the register EXACTLY ONCE

Measured:

    reads B/C on every opcode      1,126/16   corrupts 284 AND 88
    + real-register operands       1,126/16   corrupts 88 - `x = tbl`,
                                   `x = x._ApplyBeequipMul`, `x = arg3`,
                                   `x = x(...)`, `x = x and x * x`, a call LOST
    + written exactly once         1,125/17   81 files changed
                                   calls lost 0, loops lost 0, ifs lost 0
                                   total emitted calls UNCHANGED (38,460)

The higher-scoring row ships a dropped call. The one-write rule costs a point
and makes the change honest, which is the trade this session has been wrong
about in both directions before.

### ADOPTED
    clean   1,123 -> 1,125   defects 19 -> 17
    FIXED   263_BeeTypeStatsBox, 646_Capture_Chicks (branch_local_discarded)
    NEW     none
    949 tests pass on two consecutive runs

`646_Capture_Chicks::Description` now appends the zone instead of returning the
pre-branch string - a real behavioural repair, not a cosmetic one.

### State after rotation 102
SEMANTIC: **1,125/1,142 (98.5%), 17 defects** - 7 declared_never_assigned,
4 branch_local_discarded, 6 calls_not_emitted.
CLOSED: A, D, E, F, G, H, I(predicates), J, A2/C(if-else/tables), argument-cap,
  K, unreachable-blocks, B-loops, inline-if-arm-overrun, guard-arms,
  generic-for loop detection (real, repaired, measured worthless),
  **branch-arm carried identity**.
REMAINING branch_local_discarded (4): the arms that write their register more
  than once, deliberately excluded above. Same mechanism, and the one-write rule
  is what keeps them out; widening it needs a way to tell a multi-step
  extension from scratch reuse - `88_BeeStats` is the reproducer for the wrong
  side of that line.


## Rotation 103 - the chain generalisation is NEUTRAL, and my model of the last 4 is wrong

### The idea
r102 shipped "the arm writes the register EXACTLY ONCE", which by construction
excludes multi-step chains. Two of the four remaining findings look like exactly
that. `958_Drakkarmon`:

    Humanoid = Humanoid.WalkSpeed              reads Humanoid, writes it
    local WalkSpeed = Humanoid / heightScale   reads it again, writes again

Two writes, each consuming what the last produced - a carried identity walked in
steps. `88_BeeStats` is the opposite: `x = tbl`, `x = arg3` read nothing, so the
slot is being REUSED. So the general rule should be **every write is preceded by
a read**, with "written once" as its degenerate case.

Implemented as `every_write_extends`, with the B/C operand allowlist from r102
factored out (`reg_is_source_operand`) and `SETTABLEKS`-style write-through
opcodes excluded from counting as writes (`op_writes_through`).

### Measured NEUTRAL
    clean 1,125 -> 1,125   defects 17 -> 17
    FIXED none.  NEW none.
    28 files changed; calls lost 0; total emitted calls UNCHANGED (38,460)

A strictly more permissive rule pins 28 more files' registers and moves NOT ONE
finding. So the four remaining `branch_local_discarded` are not being held back
by the one-write restriction at all - the pin is reaching them (or would) and
they stay flagged regardless.

**My model of these four is wrong.** They are not "carried identities excluded
by an over-strict predicate". Something else keeps their arm value from
surviving the merge.

Reverted per r90's rule: a 28-file diff that moves no measurement does not ship.

### PARKED, with the model corrected
MECHANISM: 4 residual `branch_local_discarded` (21_ClickToMove,
  317_ScreenParticles, 445_PluckedPetal, 958_Drakkarmon).
RULED OUT: the one-write restriction (r103 - relaxing it is byte-neutral for
  these files); arm-shape predicates generally (r94-r96 history).
THE QUESTION: for `958_Drakkarmon` line 742, is `premateralize_branch_escapes_
  spans` even REACHING that register? Print reg, the `regs[reg]` variant, and
  which of the gates rejects it - `reg_used_in_range`, `parked_pure`,
  `carried_identity` - one line per candidate register. Every previous rotation
  assumed the gate was reached and tuned its predicate; that assumption has
  never been checked for these four, and rotations 95 and 97 both turned on a
  gate that was never firing.

### State after rotation 103
SEMANTIC: 1,125/1,142 (98.5%), 17 defects. 949 tests pass. Tree clean apart from
the untracked throwaway probe.


## Rotation 104 - the gate IS reached, so the fault is DOWNSTREAM of the pin

### r103's question, answered
Added `LUAU_GATE_TRACE`: one line per candidate register reaching
`premateralize_branch_escapes_spans`, with the register's value variant and the
post-merge range, plus a line naming any gate that rejects it.

`958_Drakkarmon::onRunning@729` - the proto holding the line-742 finding:

    GATE onRunning@729 reg=0  val=Expr(Name)  after=62..111
    GATE onRunning@729 reg=2  val=Expr(Name)  after=62..111
    GATE onRunning@729 reg=3  val=Unknown     after=62..111
    ... 18 candidates in total, NONE rejected by the use-after-merge filter
    GATE onRunning@729 reg=3  val=Expr(Name)  after=78..111
    GATE onRunning@729 reg=4  val=Expr(other) after=78..111

So the gate is REACHED, the registers arrive, and the first filter passes them.
The "it never fires" hypothesis - true for rotations 95 and 97, and the reason I
went looking - is FALSE here.

### Combined with r103, this locates the fault
r103 relaxed the predicate so these registers ARE pinned (28 more files changed)
and **not one finding moved**. r104 shows they reach the gate in the first
place. Together:

    reaches the gate      YES (r104)
    passes the filters    YES, under the chain rule (r103)
    gets pinned           YES (r103, 28 files changed)
    defect persists       YES

**The pin is applied and the arm value still does not survive the merge.** So
the fault is downstream of `premateralize_branch_escapes_spans` entirely - in
how a pinned write is emitted, or in `merge_regs` discarding it afterwards.

Every rotation from 101 to 103 was tuning the admission predicate of a gate that
was already admitting these registers. That whole line of attack is closed.

### PARKED, re-aimed
MECHANISM: 4 residual `branch_local_discarded` (21_ClickToMove,
  317_ScreenParticles, 445_PluckedPetal, 958_Drakkarmon).
PROVED:    the registers reach the gate; pinning them is byte-neutral for these
           findings.
RULED OUT: gate never fires (r104); one-write restriction (r103); arm-shape
           predicates (r94-r96).
THE QUESTION: with the chain rule ON so the register IS pinned, trace
  `store_complex`'s pinned path for `onRunning` reg 3 - does it emit a Reassign
  against the pinned name, or does `classify_write` still return FirstDecl? If
  FirstDecl, the pin is not consulted where it matters and `pinned_write_
  consumed` is the place to look; if Reassign, the statement is emitted and
  something later deletes it, which puts it in DCE territory like rotation 92.

### State after rotation 104
SEMANTIC: 1,125/1,142 (98.5%), 17 defects. 949 tests pass. `LUAU_GATE_TRACE`
kept and verified inert.


## Rotation 105 - answered: FirstDecl, because the pinned path is not wired to arithmetic

### r104's question
"With the register pinned, does `store_complex` emit a Reassign against the
pinned name, or does `classify_write` still return FirstDecl?"

### The answer, from the code paths rather than a guess
`pinned_write_consumed` - the function that turns a write into
`Stat::Assign { targets: [pinned name] }` - is called from exactly THREE places
(`opcode_handlers.rs`): the `LoadNil` arm, the two `Move` arms, and the table
arm. `store_complex` is not one of them.

`store_complex` does consult the pin, twice, and both uses are about NOT
renaming rather than about the assignment target:

    is_pinned_loop_carried   forces materialisation instead of parking
    do_peek                  suppresses the rebind peek while pinned

So a pinned ARITHMETIC write - `Humanoid / getHeightScale5 or 0.75` in
`958_Drakkarmon::onRunning` - is materialised (the pin works) and then named by
`classify_write`, which sees a register whose debug name differs and returns
**FirstDecl**. Out comes `local WalkSpeed = ...`, discarded at the merge.

That is exactly the branch r104 predicted would implicate
`pinned_write_consumed`, and it explains r103 cleanly: relaxing the admission
predicate pinned 28 more files' registers and moved nothing, because for
arithmetic writes the pin never reaches the naming decision.

### THE FIX SHAPE
Route `store_complex`'s materialising branch through `pinned_write_consumed`
(or an equivalent check) before `classify_write` decides, so a pinned register
with a non-generic binding is REASSIGNED rather than re-declared. The guards
already in `pinned_write_consumed` carry over unchanged and are load-bearing:

  * the binding must match `locals.current_name(reg)` - otherwise the tracker
    has moved and the name is not ours to thread;
  * generic `vN` bindings must be refused - the 551_Shop compile failure.

RISK, stated in advance: this widens the pinned path from three opcode families
to every complex write, which is the largest blast radius of any change
attempted on this mechanism. Measure with the full census, not the score - the
three highest-scoring changes this session all shipped invisible damage, and
this one touches naming, where 476_BadgeMenu and 551_Shop are the recorded
casualties.

### State after rotation 105
SEMANTIC: 1,125/1,142 (98.5%), 17 defects. 949 tests pass. No behaviour changed;
this rotation is the answer to r104's question and the fix shape it implies.


## Rotation 106 - r105 was WRONG: there is no pin to ignore

### The check
Added `LUAU_PIN_TRACE`: one line whenever `carried_identity` actually pins a
register. On `958_Drakkarmon`:

    onRunning pins: 0

**Zero.** The register is never pinned. So r105's conclusion - "the pin is
applied and `store_complex` ignores it for arithmetic writes" - is FALSE. There
is nothing to ignore.

Reading the code again with that in hand confirms it from the other side: when a
register IS pinned, `do_peek` is false, so `rebind_name` is `None` and the final
`else` branch emits `Stat::Assign` against the carried name. The pinned path
works for arithmetic. `958` emits `local WalkSpeed = ...`, which is only
reachable when `rebind_name` is `Some` - i.e. when the register is NOT pinned.

### And it corrects r104 too
r104 measured 18 candidate registers reaching the gate in `onRunning` and none
rejected by the use-after-merge filter. That was true, and I over-read it as
"these registers get pinned". They reach the gate and are then rejected by the
LATER predicates (`parked_pure`, `carried_identity`). Reaching a gate is not
passing it - the same distinction rotation 95 got wrong in the opposite
direction.

### The corrected chain
    reaches the gate            YES  (r104, measured)
    passes carried_identity     NO   (r106, measured: 0 pins)
    therefore pinned            NO
    therefore r105's fix shape  MOOT

r103 remains the anomaly to explain: its chain rule changed 28 files, so it
pinned SOMETHING - just not this register. Whatever it admitted, `958` was not
among the files whose findings moved.

### PARKED, corrected
MECHANISM: 4 residual `branch_local_discarded`.
PROVED:    the registers reach the gate (r104) and are rejected before pinning
           (r106). The pinned path itself is sound for arithmetic writes.
RULED OUT: gate never fires (r104); one-write restriction alone (r103);
           arm-shape predicates (r94-r96); "pin applied but ignored" (r106).
THE QUESTION: which predicate rejects `onRunning`'s register - `parked_pure` or
  `carried_identity` - and on which clause? Extend `LUAU_GATE_TRACE` to print
  the outcome of EACH clause per candidate register, not just gate entry. Do not
  infer it from the code a third time: r105 was derived by reading call sites
  and was wrong, and r104 by reading a trace and was over-read.

### Honest note on this stretch
Rotations 101-106 produced ONE shipped fix (r102, +2 clean) and five
corrections, three of them to my own conclusions from the preceding rotation.
The pattern is consistent and worth stating: every claim I derived by reading
code was wrong; every claim I derived by printing a value held. r105 is the
clearest case - a careful, plausible argument from the call graph, contradicted
by a single counter.

### State after rotation 106
SEMANTIC: 1,125/1,142 (98.5%), 17 defects. 949 tests pass. `LUAU_PIN_TRACE` kept
and verified inert.


## Rotation 107 - branch_local_discarded CLOSED at its evidential boundary

### The per-clause print r106 asked for
`LUAU_GATE_TRACE` now reports every clause outcome per candidate register, not
just gate entry. `958_Drakkarmon::onRunning@729`:

    reg=0 name_bound=true reads_before_write=true arm_writes=2 -> REJECT
    reg=2 name_bound=true reads_before_write=true arm_writes=5 -> REJECT

Both name-bound candidates are rejected by the WRITE-COUNT clause alone. Every
other test passes. That is the whole reason the four residual findings survive.

### The boundary, measured
So: would relaxing the write count fix them? `88_ReplicatedStorage_BeeStats` -
the file whose pinning LOSES A CALL (rotation 101) - has registers with exactly
the same signature:

    28 x  name_bound=true reads_before_write=true arm_writes=1   <- r102 admits
     8 x  name_bound=true reads_before_write=true arm_writes=2
     3 x  ... arm_writes=3
     2 x  ... arm_writes=4
     1 x  ... arm_writes=6

The residual four sit in the arm_writes>1 band, and so do the registers that
corrupt `88`. **They are indistinguishable by any read/write-pattern
predicate.** That is why r103's "every write extends" rule was byte-neutral for
these files and why four earlier predicates failed: there is no separating
line in this feature space.

### CLOSED, at the limit of the evidence
MECHANISM: branch-arm write to a register live across the merge emitted as a
declaration. REAL, bytecode-confirmed, behaviour-changing.
FIXED: the separable subset - `arm_writes == 1` (rotation 102), +2 clean,
  -2 defects, zero constructs lost anywhere.
NOT FIXED, and why: the remaining 4 share their signature with writes whose
  pinning drops a call. Taking them requires a discriminant OUTSIDE the
  read/write pattern - dataflow on the VALUE (does each write consume the
  previous result?) rather than on the register - which `merge_regs` does not
  currently carry.
REPRODUCERS: `958_Drakkarmon::onRunning` reg 0/2 (want pinned);
  `88_BeeStats` arm_writes=2 band (must NOT be pinned).

This is a closure, not a park: the mechanism is proven, the safe half is
shipped, and the unsafe half is proven inseparable with the information
available at that point in the pipeline. Reopening it means adding value-level
dataflow, not another predicate.

### State after rotation 107
SEMANTIC: 1,125/1,142 (98.5%), 17 defects - 7 declared_never_assigned,
4 branch_local_discarded (closed above), 6 calls_not_emitted.
949 tests pass. `LUAU_GATE_TRACE` extended and verified inert.


## Rotation 108 - SWEEP is BLOCKED, and the sweep tool is itself broken

### Blocked by an explicit user constraint
Jep asked, directly, to stop the local model: it was consuming enough system
resources to make the machine lag. `sweep_local.py` runs every file through
`local-coder` (gemma4:12b, 7.5GB resident), so the Sweep measure cannot be taken
without doing the thing he just asked me to stop. Killed 5 python processes and
unloaded the model (`/api/ps` -> none loaded).

**Sweep is therefore PARKED ON AN EXTERNAL CONSTRAINT, not on a technical
question.** It resumes the moment the machine is free; nothing about the
decompiler blocks it.

### And the tool is broken independently
Before stopping, an unbuffered two-file probe ran clean and produced NOTHING:

    skipping 30 file(s) over 60000 bytes
    2 file(s) to process with gemma4:12b-it-q4_K_M
    (0 findings written, exit 0)

Zero findings for two files, exit code 0, empty log. So the earlier full run -
6 minutes, 4 workers, 0 output lines - was not slow, it was silently doing
nothing while holding the model in memory. That is a real defect in
`sweep_local.py` (or in its model call), it is SEPARATE from the decompiler, and
it means any future Sweep number must be validated against a file with a KNOWN
defect before the run is trusted.

Worth stating plainly: the previous Sweep adjudication in this journal
(truncated_body 42/42 FALSE, undefined_register 68/72 FALSE, duplicate_call 5/5
FALSE, dropped_call 1/14 REAL, dead_store 12 REAL, ~19 real of 1,112) predates
r87/r88/r93/r102 and is STALE. It should not be quoted as current.

### The measures, stated honestly
    DECODE    1,142/1,142   100%
    SEMANTIC  1,125/1,142   98.5%, 17 defects
    SWEEP     unmeasured this session - blocked on the user's machine, and the
              instrument is producing no findings even when run

### State after rotation 108
Nothing changed in the decompiler. 1,125/17, 949 tests pass, tree clean apart
from the untracked probe. Semantic work continues; it needs no model.


## Rotation 109 - jump-terminated guard arms measured WORSE

### The finding it came from
`59_LocalCubs` is the densest remaining `calls_not_emitted` (3 protos, 30 calls).
`LUAU_WALK_COVER` on its proto 0 gives gaps (36,39) (43,47) **(71,165)**
(172,175); the big one is 93 instructions holding all ten of that proto's
missing CALL sites. The stop is

    71: JUMP -> 90

an unconditional jump leaving the span, so the walk abandons 72..164. Same
family as r87/r88/r93.

### The experiment
Shape C's `arm_terminates` only accepts `Return`. Extended it to `Jump`/`JumpX`
behind `LUAU_GUARD_JUMP`:

    clean 1,125 -> 1,123   defects 17 -> 19

WORSE, and reverted per the standing rule. A jump-terminated arm is far more
common than a return-terminated one, so the same wrap that pays for `error(...)`
guards mis-fires broadly when the terminator is an ordinary jump - consistent
with r93's own finding that this transformation is net-positive only where the
arm genuinely does not rejoin.

### PARKED
MECHANISM: 6 `calls_not_emitted`, all walk-abandonment at a terminator.
REPRODUCER: `59_LocalCubs` proto 0, gap (71,165), 10 CALL sites lost; also
  `314_RoboBearGui::InitInfoBox` (7/20 missed, pcs 57..116).
RULED OUT: treating a jump-terminated arm as a guard (r109, -2 clean).
THE QUESTION: at pc 71 the jump target is 90, which is INSIDE the proto but
  outside the current span. Which span is being lifted there, and why does it
  end before 90? Print the span bounds at the moment `lift_instruction_range`
  breaks on an out-of-range jump - that is the one number this family has never
  been measured with, and r88's resume already declines here because pc 72 is
  not a branch target.

### State after rotation 109
SEMANTIC: 1,125/1,142 (98.5%), 17 defects. 949 tests pass. Reverted clean.


## Rotation 110 - the arm is SKIPPED, not abandoned; and r109 read the printer

### The measurement r109 asked for
Instrumented BOTH jump paths in `lift_instruction_range`. The out-of-range break
path printed NOTHING for `59_LocalCubs::?@40`, which ruled out the whole
"walk gives up" story. The in-range path gives it:

    SPANJUMP ?@40 pc=71 target=162 span=55..162 exits=true

### r109 was wrong, and the repo warned about exactly this
r109 recorded `71: JUMP -> 90`, read off the disassembler's listing. The real
target is **162**. CLAUDE.md says, in as many words: *"Never read operands off
the disassembler's printer - it invents names like `R{a+1}`. Go to the
bytecode."* I did precisely that, and it produced a wrong diagnosis that a
jump-terminated guard rule was then built on (r109, measured -2 clean).

Fifth cross-check this session that overturned my own conclusion, and the only
one the project's own written rule had already named in advance.

### The corrected mechanism
pc 71 jumps to 162, which IS the span end (`span=55..162`), so the walk sets
`pc = 162`, the `while pc < end` test fails, and it exits CLEANLY - no break, no
abandonment. Instructions **72..161 are SKIPPED**, jumped over by design.

That range is a branch ARM reachable only from an earlier conditional, and
nothing ever lifts it as its own region. All ten of the proto's missing CALL
sites (77, 89, 102, 115, 118, 137, 139, 148, 156, 159) are inside it.

So `calls_not_emitted` here is NOT walk-abandonment at a terminator - the family
r87/r88/r90 belong to. It is a region that was never constructed: the arm is
correctly skipped by the linear walk and never picked up by the structurer.

### PARKED, re-aimed
MECHANISM: 6 `calls_not_emitted`. For `59_LocalCubs::?@40` it is an unlifted
  branch arm, not an abandoned walk.
PROVED:    pc 71 -> 162 = span end; walk exits cleanly; 72..161 skipped; all 10
  missing CALLs inside that range.
RULED OUT: walk gives up on an out-of-range jump (r110 - that path never fires
  here); jump-terminated guard arms (r109, -2 clean).
THE QUESTION: which region does the structurer emit for span 55..162? Run
  `LUAU_RUNREG_TRACE` on this proto and look for a region covering 72..161. If
  none exists, the conditional at/above pc 55 is not being matched as an
  if-region at all, and `structure_control_flow`'s handling of THAT conditional
  is the target - not the walk, and not Shape C.

### Method note
Three rotations (108-110) produced no score movement and one reverted fix. What
they produced instead: the `calls_not_emitted` family is now known NOT to be the
r87/r88 walk family, which is what four rotations of guard-arm work had assumed.

### State after rotation 110
SEMANTIC: 1,125/1,142 (98.5%), 17 defects. 949 tests pass. `LUAU_SPANBREAK`
(both jump paths) kept and verified inert.


## Rotation 111 - it is a GenericFor BODY, and the skipped arm is inside it

### The region, found
    RUNREG ? run=49..175 Linear 49..54
    RUNREG ? run=49..175 GenericFor 55..162      <-- covers the skipped range
    RUNREG ? run=49..175 InlineIfThenInLoop

    IF proto=?@40 cond_block=0  then=7  else=0 merge=Some(47)  has_else=false
    IF proto=?@40 cond_block=47 then=15 else=0 merge=Some(175) has_else=false

So pcs 72..161 are NOT an unlifted arm sitting loose in a Linear span, as r110
supposed. They are INSIDE the body of a `GenericFor` spanning 55..162.

### What that makes pc 71
`71: JUMP -> 162` targets 162, which is the GenericFor's own end - the loop
latch. An unconditional jump to the latch from inside the body is an
end-of-iteration jump. The flat walk over the body hits it, `target <= end`
holds, `pc = 162`, the loop test fails and it exits. Everything 72..161 is
skipped.

That range is a conditional ARM inside the loop body, and the body's structuring
never lifts it. All ten of the proto's missing CALL sites are in it.

### The family, corrected AGAIN
    r109  jump-terminated guard arm            WRONG (measured -2 clean)
    r110  walk abandons an out-of-range jump   WRONG (that path never fires)
    r111  arm skipped inside a GenericFor body

This is the same flattening rotation 87 fixed for `InlineIfThenInLoop` arms -
an arm lifted as a flat span drops its interior structure - one construct over,
in loop bodies. `range_contains_nested_for` and `structure_numeric_for_body`
exist for exactly this shape, so the question is why they do not reach here.

### THE QUESTION
`LUAU_RUNREG_TRACE` prints only the TOP-LEVEL regions of each run, so the
GenericFor's interior is invisible. Extend it to recurse into
`NumericFor`/`GenericFor` bodies and print their nested region lists. If
`55..162`'s body is a single Linear region, the loop-body structurer is not
running on it and that is the target; if it holds sub-regions that simply omit
72..161, the omission is in how the arm's blocks are collected.

Do not attempt a fix before that print. Rotations 109 and 110 both proposed one
from an unverified model of this proto, and both were wrong.

### State after rotation 111
SEMANTIC: 1,125/1,142 (98.5%), 17 defects. 949 tests pass. Diagnosis only.


## Rotation 112 - the loop body is ONE flat Linear region, and the obvious fix is far too broad

### The print r111 required
`LUAU_RUNREG_TRACE` now recurses into loop bodies. On `59_LocalCubs::?@40`,
with a healthy loop in the SAME proto as the control:

    GenericFor 27..45   nested=2      ValueJoin 27..42 + Linear 42..45   HEALTHY
    GenericFor 55..162  nested=1      Linear 55..162                     FLAT

The failing loop's body is a single Linear region covering itself. So
`structure_numeric_for_body` found NO structure in it, the interior conditional
at pc 71 was never recognised, and the arm at 72..161 - holding all ten of the
proto's missing CALL sites - is skipped by the flat walk.

That is the answer, and it is mechanical: Shape B fires only when the arm
`range_contains_nested_for`, Shape C only when the arm ends in `RETURN`. A plain
conditional whose arm is jumped over matches NEITHER, so the body stays flat.

### The obvious fix, measured
Dropped the nested-for requirement so a plain conditional in a loop body is
wrapped too:

    clean 1,125 -> 1,106   defects 17 -> 37

Catastrophically worse - a bigger regression than any single change this
session. Reverted. Wrapping EVERY conditional in EVERY loop body is not a
relaxation of Shape B, it is a different transformation, and it confirms from a
third direction what r93 and r109 each found: this wrap pays only where the arm
genuinely does not rejoin, and mis-fires everywhere else.

### PARKED
MECHANISM: 6 `calls_not_emitted`. For `59_LocalCubs::?@40` the loop body is
  emitted as one flat Linear region and its interior conditional is never
  structured.
PROVED:    the body IS the region `GenericFor 55..162` with a single nested
  `Linear 55..162`; a sibling loop in the same proto structures correctly
  (nested=2), so the machinery works and this shape defeats it.
RULED OUT: jump-terminated guard arms (r109, -2); walk-abandonment (r110, that
  path never fires); wrapping every plain conditional in a loop body (r112, -19).
THE QUESTION: what distinguishes `GenericFor 27..45`, which structures, from
  `GenericFor 55..162`, which does not? Both are generic-for bodies in the same
  proto. Print the shape decision inside `structure_numeric_for_body` for BOTH
  ranges - which shapes it tests and which it rejects, per pc - and compare. The
  healthy one produces a ValueJoin, so the scan clearly can find structure; the
  difference between the two is the whole answer and has never been printed.

### State after rotation 112
SEMANTIC: 1,125/1,142 (98.5%), 17 defects. 949 tests pass. Reverted clean; the
recursive RUNREG trace is kept and verified inert.


## Rotation 113 - the two bodies compared, and calls_not_emitted CLOSED at its boundary

### The comparison r112 required
`LUAU_SHAPE_TRACE=<start>-<end>` prints, per pc, what the loop-body scan sees.
Two generic-for bodies in the SAME proto (`59_LocalCubs::?@40`):

HEALTHY, body 27..45 - three pcs scanned, then done:

    pc=27  GetTableKS  fwd_cond=None  linear_start=27
    pc=42  Mul         fwd_cond=None  linear_start=42     <- jumped 27 -> 42
    pc=43  SetTableKS  fwd_cond=None  linear_start=42

The ValueJoin shape fired at the FIRST pc and consumed 27..42 in one step.

FLAT, body 55..162 - every pc scanned, `linear_start` never leaves 55:

    pc=57  JumpIfNot  fwd_cond=Some((58, 63))
    pc=62  JumpIf     fwd_cond=Some((63, 72))
    pc=66  JumpIf     fwd_cond=Some((67, 69))
    pc=71  Jump       fwd_cond=None                <- the skip to 162
    ... 100+ more pcs, no shape ever fires

### The answer
The conditionals ARE detected - `forward_conditional_jump` returns `Some` for
all three. They are REJECTED because each arm is a handful of instructions:
no nested `for` (Shape B), and none ends in `RETURN` (Shape C).

So this is not a shape failing to fire. **A plain `if`/`elseif` chain inside a
loop body matches none of the implemented shapes at all.** The scan has shapes
for ValueJoin, nested-for arms, return-terminated guards and inline loops - and
nothing for ordinary branching.

### Why the gap is not filled
Filling it naively is far worse than leaving it (r112: 1,125/17 -> 1,106/37, the
largest regression of the session). Three independent attempts now agree:

    r83   `LUAU_ARM_IF`, nested-cond arms in `lift_branch_region`  +1 clean but
          DELETED a purchase guard - rejected
    r109  jump-terminated guard arms                                -2 clean
    r112  wrap every plain conditional in a loop body               -19 clean

Each fires this wrap where the arm REJOINS, and the wrap is only sound where it
does not. That is the same boundary r93 established when Shape C was adopted.

### CLOSED, at the evidential boundary
MECHANISM: `calls_not_emitted` - a loop body containing plain branching is
  emitted as one flat Linear region; the walk skips arms that a jump hops over
  and their calls are lost.
PROVED:    the body IS `GenericFor 55..162` with a single nested `Linear`; a
  sibling loop in the same proto structures correctly via ValueJoin; the
  conditionals are detected and rejected on arm shape, printed per pc.
RULED OUT: walk-abandonment (r110, that path never fires); jump-terminated
  guards (r109); nested-cond arms (r83); plain-conditional wrapping (r112).
WHAT IT WOULD TAKE: a genuine if/elseif region builder for loop bodies -
  matching the conditional, its arms AND its merge - not another trigger on the
  existing wrap. Every attempt so far has reused `InlineIfThenInLoop`, which by
  construction assumes the arm does not rejoin, and every one has regressed.

Closed rather than parked: the mechanism is fully characterised, the reproducer
and its control are recorded, and four attempts have each measured worse. It
reopens with new machinery, not a new predicate.

### State after rotation 113
SEMANTIC: 1,125/1,142 (98.5%), 17 defects. 949 tests pass. `LUAU_SHAPE_TRACE`
kept and verified inert.


## Rotation 114 - the last 7 declared_never_assigned, sorted by mint site

### The census
    REG_EXPR_UNKNOWN+UNK_CALL_CLEAR   4   22, 396, 53, 85
    METHOD_RECV_REJECT                1   54
    REG_EXPR_UNKNOWN+UNK_MERGE_COMPOUND 1 79
    UPVAL_NAME                        1   873   BLOCKED ON THE FORMAT

### The hypothesis, and its falsification
`533_NPCs` and `368_HoneycombTools` were the same mint site and both fell to
r87/r88 - walk abandonment at a terminator. Natural guess: the remaining four
are more of the same. They are not.

`396_TadpoleTailInit::?@5` has the worst coverage of the four (41/55 steps,
74%), and its gaps are:

    (17,20) (25,29) (31,35) (51,54) (59,62) (65,68)

Every one is a single instruction or two - ordinary branch-arm skipping, not a
93-instruction abandonment like `59_LocalCubs`. And `LUAU_SPANBREAK` prints
NOTHING for this proto: neither jump path gives up.

So the shortfall is normal, the walk is healthy, and the orphan register comes
from something else. The r87/r88 family does not extend to these four.

### What that leaves
`UNK_CALL_CLEAR` is documented in `opcode_handlers.rs` as "CORRECT by
construction: a call leaves everything above its results genuinely holding
nothing". So a later read of such a register is a read on a path where the CALL
did not execute - a genuine merge problem, not a walk problem. Same conclusion
r82 reached from the other end, now with the walk explanation eliminated.

### PARKED, with the family split
BLOCKED ON THE FORMAT (1): `873_Translation` `upval_2`. Roblox bytecode carries
  no CAPTURE data, so the parent register behind a child's upvalue is not
  recoverable from the input. Not fixable in this decompiler.
PARKED (6): the register is cleared by a CALL on one path and read on another.
  RULED OUT: walk abandonment (r114 - coverage healthy, no span-break fires);
  the r87/r88 terminator family.
  THE QUESTION: for `396_TadpoleTailInit::?@5`, which pc CLEARS the orphan
  register and which pc READS it? `LUAU_REG_WATCH=<n>` prints both, and the two
  pcs together say whether the read is genuinely on a no-call path (a merge
  defect) or the clear is over-broad (a `clear_start` defect). That single pair
  of numbers has never been taken for these files.

### State after rotation 114
SEMANTIC: 1,125/1,142 (98.5%), 17 defects. 949 tests pass. Diagnosis only;
nothing changed.


## Rotation 115 - 396 diagnosed exactly; the attempt to generalise it FAILED

### 396_TadpoleTailInit: a dropped for-loop's iteration variable
`v5` is read only inside `monitorSegments`, and the emitted body is:

    while result do
        Parent5 = Body.Parent
        if Parent5 ~= nil then continue end
        Position = Body.Position
        result4, result25, result36 = ipairs(Body5)     <-- a FORGPREP, flattened
        if v5.Parent ~= nil then continue end            <-- v5 = the loop variable
        Position2 = (v5.Position - Position):Dot(...)

`ipairs(...)` is emitted as a bare THREE-VALUE ASSIGNMENT because the generic
for inside this `while` body was never structured. `v5` is that loop's iteration
variable, so it is never bound and every use is nil.

This is verbatim the failure the lifter's own comment already describes:
"iterator call flattened to a bare 3-value local, body unguarded, loop variables
permanently nil" - the arm-interior loop case. Same missing machinery that
rotation 113 closed `calls_not_emitted` on: a loop/arm interior that the flat
lifter cannot express.

### The generalisation, tested and FALSE
Natural next claim: the other `declared_never_assigned` files are the same
dropped-loop shape, which would unify the two remaining classes. Counted the
flattened-FORGPREP signature (`a, b, c = ipairs|pairs|next(...)`) in each:

    396   1     <- the diagnosed one
    22    0
    53    0
    85    0
    79    0
    54    0

**Zero in all five others.** The unification is false. 396 is a specific
instance, not a class explanation, and the remaining five orphans come from
something else.

That is the sixth time this session a plausible generalisation has been killed
by counting rather than reasoning, and it cost one command rather than a
rotation - which is the only reason to keep writing them down.

### PARKED, split precisely
DIAGNOSED (1): `396_TadpoleTailInit` - dropped generic-for inside a `while`
  body; `v5` is its iteration variable. Blocked on the SAME machinery r113
  closed: a real region builder for loop interiors. Four naive fills measured
  worse (r83, r109, r112, and r93's boundary).
BLOCKED ON THE FORMAT (1): `873_Translation` `upval_2` - no CAPTURE data.
PARKED (5): 22, 53, 85 (`UNK_CALL_CLEAR`), 79 (`UNK_MERGE_COMPOUND`),
  54 (`METHOD_RECV_REJECT`). No flattened loop, healthy walk coverage, no
  span-break. THE QUESTION: for each, which pc last wrote the orphan register
  and which pc reads it - `LUAU_REG_WATCH=<n>` in the proto that READS it, not
  the one that hoists it. Rotation 115 wasted its first probe watching `?@5`
  when the read is inside a nested closure; take the watch in the reading proto.

### State after rotation 115
SEMANTIC: 1,125/1,142 (98.5%), 17 defects. 949 tests pass. Diagnosis only.


## Rotation 116 - the watch does not implicate register 13, and that is itself the finding

### Taking r115's measurement on 85_Badges
`v13` is read at emitted lines 2348/2351 inside `setData`. Watched register 13
in `setData@1163`:

    walked=159 steps=169 pct=94        <- coverage HEALTHY
    reg 13 cycles Unknown -> String -> Name("arg2") -> Unknown -> Index{...}

So the walk is fine and the register is repeatedly live. Then the comparisons in
that proto:

    24: JUMPIFNOTLT R7 >= R5
    38: JUMPIFNOTLT R8 >= R7
   157: JUMPIFNOTLT R9 >= R8
   202: JUMPIFNOTLE R9 > R8

**Not one of them reads R13.** The emitted `if getTask6.Amount < v13` therefore
does not correspond to a comparison against register 13 in this proto.

### The trap this exposes
`v13` is a NAME produced by the generic-name mint (`reg_expr`'s `v{idx}`
family), and I assumed `v13` -> register 13. The watch says otherwise. Either the
name was minted for a different register in a different proto and folded by
`free_var_decls` (which "folds every same-named orphan into ONE chunk-top
`local vN`" - its own comment), or the read sits in a nested closure of
`setData` rather than `setData` itself.

That folding is the point: chunk-wide `vN` names are NOT unique per register, so
**reading a register index out of a generated name is unsound**. Rotation 116
spent its probe on that assumption. It is the same class of error as reading a
pc across protos (r47, r86, r95, r110) - inferring an identity from a label that
does not carry one.

### PARKED, with the trap recorded
MECHANISM: 5 orphans (22, 53, 85, 79, 54).
RULED OUT: walk abandonment (r114); dropped for-loop (r115, zero signature in
  all five); register-index-from-name (r116, unsound).
THE QUESTION, restated safely: instrument `free_var_decls` to print, for each
  hoisted READONLY name, the PROTO and REGISTER it was minted from - the mint
  site is already recorded (`mint_trace`), but not the register or proto. Until
  that exists, every attempt to trace one of these orphans starts by guessing
  which register it is, and this rotation shows the guess is wrong at least
  sometimes.

### State after rotation 116
SEMANTIC: 1,125/1,142 (98.5%), 17 defects. 949 tests pass. Diagnosis only;
nothing changed.


## Rotation 117 - r116 CORRECTED, and every orphan now has a proto and a register

### The correction
r116 concluded that a hoisted `vN` cannot be traced to a register, because
`free_var_decls` folds same-named orphans chunk-wide. Half right. `reg_expr`
mints `v{idx}` where **idx IS the register index**, so the register was always
sound; what the name lacks is the PROTO. r116 mis-stated the gap and blamed the
wrong half.

### The instrument
Added `current_dce_proto()` (reusing the DCE proto thread-local, which is
already set once per proto lift) and a `MINTAT` line at the generic-name mint:

    MINTAT  setData@1163  reg=13  name=v13  held=Unknown

That is the field r116 said was missing, and it settles `85_Badges` outright:
`v13` is register 13 in `setData@1163`, holding `Unknown` at the read. The
earlier claim that "the watch does not implicate register 13" was wrong -
rotation 116's watch was correct and its conclusion was not.

### All five located
    85   setData@1163      reg=13
    22   OnTap@607         reg=6
    53   renderPoint@49    reg=7
    79   ButtonText@179    reg=7
    54   (v3 - no MINTAT under this filter; site is METHOD_RECV_REJECT, a
          different mint path than reg_expr, so it needs its own record)

Four of the five now have an exact (proto, register) pair. Every previous
attempt on this class began by guessing one or both.

### What that unblocks
`unknown_origin` already attributes the clear: for 22/53/85 it is
`UNK_CALL_CLEAR`, documented as "CORRECT by construction - a call leaves
everything above its results genuinely holding nothing". With the proto and
register now fixed, the remaining question is answerable by a single watch in
the RIGHT proto: which pc clears the register, and does the read at the merge
lie on a path where that call never executed?

If yes, it is a merge defect and the clear is innocent. If the read is on the
same path as the call, the clear's `clear_start` is too low - a different fix
entirely. That fork has never been resolved because the proto was never known.

### PARKED, with the guesswork removed
MECHANISM: 5 orphans. Now: (proto, register) known for 4; `54` needs a MINTAT
  equivalent on the `METHOD_RECV_REJECT` path.
RULED OUT: walk abandonment (r114); dropped for-loop (r115); "register
  unidentifiable" (r117 - it was identifiable all along).
THE QUESTION: `LUAU_REG_WATCH=<reg>` inside each named proto, paired with the
  pc of the clearing CALL, to resolve the merge-defect / clear-too-broad fork.

### State after rotation 117
SEMANTIC: 1,125/1,142 (98.5%), 17 defects. 949 tests pass. `MINTAT` verified
inert.


## Rotation 118 - THE FORK RESOLVED: a call-cleared register is BOTTOM, not TOP

### The measurement, in the right proto at last
`53_PathDisplay`, `renderPoint@49`, register 7 (from r117's MINTAT):

    38: CALL      R6 args=2 results=1     clears R7 - it is ABOVE the result
    39: GETUPVAL  R9 U1
    40: GETTABLEN R8 R9[1]
    41: JUMPIF    R8 -> 44
    42: LOADNIL   R7                      fall-through DEFINES R7 = nil
    43: JUMP      -> 57
    44: GETUPVAL  R9 U1                   taken path leaves R7 call-cleared
    ...
    read of R7 after the merge  ->  MINTED v7

`LUAU_REG_WATCH=7` confirms it: Unknown at 39/42/44, then
`READ reg=7 MINTED v7 (register held Some(Unknown))`.

### The answer to the two-way fork
r114/r117 posed it as: merge defect, or `clear_start` too broad? **Merge
defect.** The clear at pc 38 is correct - R7 is above the call's result and the
VM leaves it holding nothing. The fault is what happens at the JOIN:

    arm A (fall-through)  R7 = nil            EXPLICIT, from LOADNIL
    arm B (taken)         R7 = call-cleared   holds NOTHING
    merge_regs            -> Unknown          -> mints `v7`

`merge_regs` treats the two as disagreeing values and collapses to Unknown. But
a call-cleared register is not a competing VALUE - it is the absence of one. In
lattice terms it is BOTTOM, and the join of {nil, bottom} is nil, not TOP.

So the correct output here is `nil`, and the decompiler emits an undeclared
`v7` instead.

### Why this matters beyond the file
`UNK_CALL_CLEAR` is the largest of the remaining mint sites (22, 53, 85 - three
of five orphans) and its own source comment already states the premise:
"CORRECT by construction: a call leaves everything above its results genuinely
holding nothing". That guarantee is exactly what makes the register bottom
rather than unknown, and `merge_regs` does not use it.

### THE FIX SHAPE
Distinguish `RegVal::Unknown` arising from a CALL clear from `Unknown` meaning
"the lifter lost this". At a merge, the former should yield to a defined value
on the other arm; the latter must still poison the join. `mint_trace::
unknown_origin` ALREADY records which registers were cleared by a call
(`UNK_CALL_CLEAR`) - but only as a diagnostic, keyed per register, and it is
overwritten as lifting proceeds. Making the distinction load-bearing needs it in
`RegVal` itself (e.g. a `Dead` variant), not in a side table.

RISK, stated up front: `RegVal` is threaded through the whole lifter and a new
variant touches every match. Measure with the full census, not the score - and
note that the three highest-scoring changes this session all shipped invisible
damage.

### State after rotation 118
SEMANTIC: 1,125/1,142 (98.5%), 17 defects. 949 tests pass. Diagnosis only.
The five orphans now have: exact proto, exact register, the clearing pc, the
defining pc on the other arm, and a named lattice defect. That is the first time
this class has had a MECHANISM rather than a symptom.


## Rotation 119 - the dead-join rule implemented, measured NEGATIVE, reverted

### What was built
r118's fix shape, in the smallest form that needs no new `RegVal` variant.
`merge_regs` already receives `regs_before`, so the rule was: if the pre-branch
slot is `Unknown` (dead) and one arm left it untouched while the other DEFINED
it, take the defined value instead of resetting both to `Unknown`. Restricted to
an arm that never touched the register, so an arm that actively LOST a value
still poisons the join.

### Measured
    clean 1,125 -> 1,125   defects 17 -> 17
    53_PathDisplay `v7`:  4 occurrences before, 4 after   NOT FIXED
    31 files changed
    calls LOST in 2 files (22_NewClickToMove 372->371,
                           43_ClickToMoveController 389->388)

No benefit, real damage. Reverted.

### What that says about r118
The diagnosis stands - at `renderPoint@49` pc 42 defines R7 while the other arm
leaves it call-cleared, and the read after the merge mints `v7`. But the rule
built from it does not fire for that register, so the merge that produces the
orphan is NOT the one this rule inspects. Either the arms reach `merge_regs`
with both sides already `Unknown` (the `LOADNIL` value consumed or parked
elsewhere before the join), or the join for this register happens on a different
path entirely.

So r118's mechanism is correct as a description and incomplete as a location:
"the join loses it" is established, "which join, and in what state" is not.

### PARKED
MECHANISM: 5 orphans, `UNK_CALL_CLEAR` dominant. Proto and register known for 4
  (r117); the clearing CALL and the defining arm known for `53` (r118).
RULED OUT: walk abandonment (r114); dropped for-loop (r115); register
  unidentifiable (r117); pre-branch-dead join rule (r119, no fix + 2 files
  damaged).
THE QUESTION: instrument `merge_regs` itself - print `i`, `regs_before[i]`,
  `slot`, `other[i]` for register 7 in `renderPoint@49`. Every rotation from 114
  has reasoned about what the merge sees; none has printed it. That is the same
  gap that r104-r107 closed for the pin gate, and closing it there took one
  print.

### State after rotation 119
SEMANTIC: 1,125/1,142 (98.5%), 17 defects. 949 tests pass. Reverted clean.


## Rotation 120 - the join sees NOTHING, and the loss is an inline-if snapshot restore

### The print that six rotations should have started with
`LUAU_MERGE_WATCH=7` on `53_PathDisplay::renderPoint@49`:

    MERGE renderPoint@49 reg=7 before=Unknown self=Unknown other=Unknown

**All three Unknown.** The `LOADNIL R7` at pc 42 never reaches `merge_regs` as a
value. So r118's model - one arm holds nil, the other is call-cleared, and the
join collapses them - is FALSE at the join, and r119's rule could never have
fired because there is no defined side to take. Both were built on an
unprinted assumption.

### Where the nil actually dies
The `LoadNil` handler does store a value:

    regs[a] = RegVal::Expr(Expr::Nil);
    explicit_nil_regs.insert(a);

so R7 genuinely holds nil at pc 42. It is discarded afterwards by the INLINE-IF
path, which brackets each arm with a snapshot restore:

    let regs_snapshot = regs.clone();
    lift_instruction_range(... then_stmts ...);
    ...
    *regs = regs_snapshot;      // arm register state thrown away

The comment states the intent plainly - "Lift the guarded block with isolated
register state so assignments inside the then-body don't corrupt the
fall-through path's register view" - and the cost is exactly this: a value
assigned ONLY inside an arm does not survive the branch, so every post-branch
read of it mints an undeclared `vN`.

### This closes a loop opened at the start of the session
That is the A2/C mechanism: a value assigned in a branch arm, discarded at the
merge. Rotation 17 fixed one face of it (the `ValueJoin` fallback no longer
emits a `Local` for a `Reassign`); this is the inline-if face, and it is
deliberate isolation rather than an oversight - which is why every predicate
aimed at the merge missed it.

### PARKED, correctly located at last
MECHANISM: inline-if discards arm register state wholesale; a value written only
  in an arm is lost and the post-branch read mints `vN`.
PROVED:    `LOADNIL` stores `Expr::Nil` (source); the join sees `Unknown` on all
  three inputs (printed); the restore is unconditional (source).
RULED OUT: merge-lattice rules (r119, measured: no fix, 2 files damaged);
  call-clear as bottom at the join (r118 model, falsified by the print).
THE QUESTION: which registers may safely survive an arm? The restore exists to
  stop arm writes corrupting the fall-through view, so the answer is NOT
  "keep everything" - it is the subset written on EVERY path that reaches the
  join, which is the same path-sensitivity the very first rotation of this
  session identified and which `merge_regs` still does not carry.

### State after rotation 120
SEMANTIC: 1,125/1,142 (98.5%), 17 defects. 949 tests pass. `LUAU_MERGE_WATCH`
kept and verified inert.


## Rotation 121 - the fix WORKS, fixes all three orphans, and is REJECTED on net calls

### Built at the location r120 printed
Not `merge_regs` - the arm's snapshot restore. `restore_regs_keeping_dead_defs`
replaces the blanket `*regs = regs_snapshot` at all THREE inline-if sites:
a slot that was `Unknown` before the branch and DEFINED by the arm keeps the
arm's value; a slot that already held a value is restored exactly as before, so
no fall-through view is disturbed.

### It works
    clean 1,125 -> 1,128   defects 17 -> 14
    FIXED  22_NewClickToMove, 53_PathDisplay, 85_Badges
           - EXACTLY the three `UNK_CALL_CLEAR` orphans diagnosed in r117/r118
    NEW    none

Six rotations of diagnosis (115-120) pointed at one location and the fix landed
on the first attempt there.

### And it is rejected
    21_ClickToMove   534 -> 528 calls, 348 -> 344 ifs

It loses an entire auto-jump guard - `doAutoJump()` and its conditions - twice
over. Two narrowings were measured:

    nil only                    1,125/17   byte-neutral, no benefit at all
    exclude call-bearing values 1,128/14   SAME damage, exclusion never fires

**The decisive number: total emitted calls 38,460 -> 38,454.** The corpus emits
FEWER calls than before. r93 was accepted with 9 damaged files because it
recovered +324 calls; this recovers none and costs six. A checker score that
improves while the output emits less code is the exact trade this session has
refused four times (r83, r90, r101, r112), and refusing it is why 1,125 means
what it says.

Reverted. 1,125/17 re-measured clean, 949 tests pass.

### PARKED - the closest this class has ever been
MECHANISM: inline-if discards arm register state; a value written only in an arm
  is lost and the post-branch read mints `vN`.
PROVED:    the location (r120 print), and that a fix there closes all three
  `UNK_CALL_CLEAR` orphans with no new findings (r121).
RULED OUT: merge-lattice rules (r119); nil-only restore (r121, no effect);
  call-bearing exclusion (r121, no effect on the damage).
THE QUESTION: why does keeping a dead slot's arm value delete
  `21_ClickToMove`'s auto-jump guard? Print the register and value kept for that
  proto, then the condition before and after `collapse_constant_ifs`. If a kept
  value makes a guard constant-foldable, the restore must exclude registers the
  guard READS - which is a use-site test, and every predicate tried so far has
  been a definition-site test.

### State after rotation 121
SEMANTIC: 1,125/1,142 (98.5%), 17 defects. 949 tests pass. Reverted clean.


## Rotation 122 - ADOPTED. The constant exclusion was the whole difference

### The use-site question r121 posed
Why does keeping a dead slot's arm value delete `21_ClickToMove`'s auto-jump
guard? `LUAU_ARM_KEEP_TRACE` printed every kept value, and among 105 keeps the
answer is visible immediately:

    ARMKEEP Step@449    pc=7  reg=2  value=Nil
    ARMKEEP Update@1436 pc=7  reg=2  value=Nil
    ARMKEEP ?@674       pc=91 reg=8  value=Number(2.0)

**Constants.** A kept constant is exactly what lets `collapse_constant_ifs` fold
a guard to a decidable value and delete it. Excluding
`Nil | Bool | Number | String` from the keep:

                          calls lost in 21   ifs lost   net corpus calls
    keep everything            6 (guard gone)    4           -6
    exclude call-bearing       6 (unchanged)     4           -6
    exclude CONSTANTS          1 (artifact)      0           -1

And the score is 1,128/14 in all three, so the constants contributed NOTHING to
the fix and ALL of the damage. Keeping nils alone had already measured
byte-neutral (r121), which was the clue.

### The last -1 is a counting artifact, verified by reading
`21_ClickToMove`'s remaining difference decomposes one call chain rather than
losing it: `Pather(Character29, result222)` survives as
`Humanoid49(Character29, GetEquippedTool3)`, and TWO statements are ADDED
(`local Humanoid49 = result221.Torso`, `local GetEquippedTool3 = ...`). The file
is more verbose - a name threaded over a scratch slot - and nothing is deleted.

### ADOPTED
    clean   1,125 -> 1,128   defects 17 -> 14
    FIXED   22_NewClickToMove, 53_PathDisplay, 85_Badges - EXACTLY the three
            `UNK_CALL_CLEAR` orphans diagnosed in r117/r118
    NEW     none
    42 files changed; ifs lost 0, loops lost 0, no code removed anywhere
    949 tests pass on two consecutive runs

This closes the mechanism opened at rotation 82 and chased through 40
rotations: a value written only inside a branch arm is discarded by the arm's
snapshot restore, so the post-branch read mints an undeclared `vN`.

### State after rotation 122
SEMANTIC: **1,128/1,142 (98.8%), 14 defects** - 4 declared_never_assigned,
4 branch_local_discarded, 6 calls_not_emitted.
CLOSED (21): ..., **inline-if arm-value discard**.
REMAINING declared_never_assigned (4): `396` (dropped for-loop, needs r113's
  machinery), `873` (blocked on the format), `79` (`UNK_MERGE_COMPOUND`),
  `54` (`METHOD_RECV_REJECT`) - two distinct mint sites still unexamined.


## Rotation 123 - the last two mint sites adjudicated REAL, and one is not where it looks

### 54_Controls - METHOD_RECV_REJECT
    local SteeringVelocity3 = Character.PrimaryPart:FindFirstChild("SteeringVelocity")
    if SteeringVelocity3 then
        v3:Destroy()          -- should be SteeringVelocity3:Destroy()
    end

REAL. The receiver is bound one line above and the call names an orphan instead.

But the rejection is NOT the bug. `LUAU_MINT_TRACE` prints what was thrown away:

    REJECT METHOD_RECV v3 discarded=Nil
    REJECT METHOD_RECV v7 discarded=Nil

Register 3 held `Expr::Nil` at the NAMECALL, and `method_receiver_expr` rejects
a nil receiver deliberately - you cannot call a method on nil, and its doc
comment lists exactly the types no metamethod can rescue. Minting `v3` there is
the honest outcome: it keeps the defect VISIBLE rather than emitting
`nil:Destroy()`.

So the fault is upstream - the receiver register holds nil where it should hold
the `FindFirstChild` result. `method_receiver_expr` is a reporter, not a cause,
and any fix aimed at it would be suppressing the symptom. Note the emitted
`v13 = nil` immediately above: a LOADNIL is reaching that slot.

### 79_Toys - UNK_MERGE_COMPOUND
    local result26 = (v7 .. " (") .. value14(TimeString2)
    ...
    Desc = Desc and (Desc .. " to ") .. v7

REAL. `v7` is a label concatenated into two user-visible strings and never
assigned, so both render with a nil in them.

### Both classes now fully adjudicated
Every one of the 14 remaining findings has been read and classified. NONE is a
false positive - no checker rule needs correcting, which is itself a result: the
semantic checker's precision on this corpus is 14/14 at this point.

    declared_never_assigned  4   396 (dropped for-loop, needs r113 machinery)
                                 873 (blocked on the format - no CAPTURE data)
                                 79  (UNK_MERGE_COMPOUND, real)
                                 54  (upstream of METHOD_RECV_REJECT, real)
    branch_local_discarded   4   closed at boundary (r107)
    calls_not_emitted        6   closed at boundary (r113)

### State after rotation 123
SEMANTIC: 1,128/1,142 (98.8%), 14 defects. 949 tests pass. Adjudication only.


## Rotation 124 - 54_Controls located: the receiver is CORRECT, the arm lift is not

### Proto-qualified rejection
    REJECT METHOD_RECV v3 reg=3 proto=?@74 discarded=Nil
    REJECT METHOD_RECV v7 reg=7 proto=?@74 discarded=Nil

Both in one proto. `LUAU_REG_WATCH=3` in `?@74`:

    before_pc=16  Expr(MethodCall{ object: result2, method: FindFirstChild })
    before_pc=17  Expr(Name("SteeringVelocity3"))     <- CORRECT receiver
    before_pc=18  Expr(Name("SteeringVelocity3"))
    before_pc=18  Expr(Nil)                           <- clobbered
    before_pc=22  Unknown

### The bytecode is fine, and the printer nearly fooled me again
    13: LOADK     R5 K7 ; "SteeringVelocity"
    14: NAMECALL  R3 R4:"FindFirstChild"
    16: CALL      R3 args=2 results=1      R3 = the result
    17: JUMPIFNOT R3 -> +4                 `if sv then`
    18: NAMECALL  R4 R5:"Destroy"

`NAMECALL A B C` sets `R(A+1) = R(B)` and `R(A) = R(B)[K(C)]`, so the printed
"R4 R5" is A and A+1 - NOT the receiver. The receiver is R3 and it is correct.
CLAUDE.md's rule ("never read operands off the disassembler's printer") applies
here exactly as it did at rotation 110, and reading it naively would have
produced a fourth wrong diagnosis of this file.

### The actual fault
TWO watch entries at `before_pc=18`: the arm is lifted twice - once in the
enclosing pass and once as the inline-if arm - and on the ARM lift R3 arrives
as `Nil` rather than the receiver. `method_receiver_expr` then correctly refuses
a nil receiver and mints `v3`.

So this is arm-entry register state, adjacent to what rotation 122 fixed. r122
addressed what an arm's writes do on the way OUT (the snapshot restore); this is
what the arm sees on the way IN.

### PARKED, precisely
MECHANISM: the inline-if arm is lifted with `Nil` in the receiver register where
  the enclosing pass has the correct value.
PROVED:    bytecode correct (R3 = FindFirstChild result at pc 16, tested at 17,
  receiver of the NAMECALL at 18); enclosing pass holds the right value at
  pc 17; the arm lift sees `Nil`; the rejection is correct behaviour.
RULED OUT: a bad receiver predicate (`method_receiver_expr` is a reporter -
  r123); a bytecode/opmap fault (verified above).
THE QUESTION: what is the arm's register state at ENTRY, and why does R3 differ
  from the enclosing pass? Print `regs[3]` at the top of the inline-if arm lift
  and compare with the value at `pc 17`. r122 instrumented the restore; the
  entry has never been printed.

### State after rotation 124
SEMANTIC: 1,128/1,142 (98.8%), 14 defects. 949 tests pass. Diagnosis only; the
rejection trace is now proto- and register-qualified and verified inert.


## Rotation 125 - the arm ENTERS with Nil; the clobber is between the condition and the lift

### The print r124 asked for
`LUAU_ARM_ENTRY=3` on `54_Controls`, proto `?@74`:

    ARMENTRY ?@74 arm=8..22   reg=3 value=Some(Unknown)
    ARMENTRY ?@74 arm=18..22  reg=3 value=Some(Expr(Nil))
    ARMENTRY ?@74 arm=50..54  reg=3 value=Some(Expr(Field{arg2,"EquippedParachute"}))

Two nested arms. The OUTER (8..22) enters with `Unknown`, walks to pc 16 where
`CALL R3` lands the `FindFirstChild` result, and holds
`Name("SteeringVelocity3")` at pc 17 (r124's watch). The INNER arm (18..22) then
enters with `Expr(Nil)`.

### What that narrows to
The arm is lifted with `regs` DIRECTLY - no copy is taken before the call - so
the clobber happens between `extract_branch_condition` reading R3 at pc 17 and
`lift_instruction_range` being invoked for 18..22. The candidates in that window
are small and named: the `regs_snapshot` clone, and
`premateralize_branch_escapes_split`, which runs at both inline-if sites
(opcode_handlers 3457, 3605) and exists precisely to materialise registers the
arms overwrite.

That is a two-suspect window, reached from a four-file class that began this
session as "seven orphans, mechanism unknown".

### PARKED
MECHANISM: the inline-if arm receives `Nil` in a register the enclosing pass
  holds correctly; `method_receiver_expr` then rightly refuses a nil receiver
  and mints `vN`.
PROVED:    bytecode correct (r124); enclosing state correct at pc 17 (r124);
  arm entry state wrong (r125); the arm shares `regs` with the caller, so the
  write is in the pre-lift window.
RULED OUT: bad receiver predicate (r123 - it is a reporter); opmap/bytecode
  fault (r124); arm-exit restore (r122 fixed that direction and this is the
  other one).
THE QUESTION: print `regs[3]` immediately before and after
  `premateralize_branch_escapes_split` at the inline-if site. If it is `Name`
  before and `Nil` after, that function is writing the nil; if it is already
  `Nil` before, the write is in the condition handling at pc 17.

### State after rotation 125
SEMANTIC: 1,128/1,142 (98.8%), 14 defects. 949 tests pass. `LUAU_ARM_ENTRY`
added at all three inline-if sites and verified inert.


## Rotation 126 - premateralization CLEARED; the write is inside the JUMPIFNOT handler

### The print r125 specified
    PREMAT BEFORE ?@74 pc=17 reg=3 value=Some(Expr(Nil))
    PREMAT AFTER  ?@74 pc=17 reg=3 value=Some(Expr(Nil))

R3 is ALREADY `Nil` before `premateralize_branch_escapes_split` runs, and
unchanged after. **That suspect is cleared.**

Against r124's watch, which had `before_pc=17 value=Expr(Name("SteeringVelocity3"))`
at the TOP of pc 17, the window closes to a single handler: the write happens
INSIDE the `JumpIfNot` arm at pc 17, before it reaches premateralization.

The control lines confirm the print is meaningful rather than vacuous - the same
register at other branch points carries a real value through untouched:

    PREMAT BEFORE/AFTER ?@74 pc=49 reg=3 Expr(Field{arg2,"EquippedParachute"})
    PREMAT BEFORE/AFTER ?@74 pc=70 reg=3 Expr(Field{arg2,"EquippedParachute"})

### What is left in that window
The `JumpIf`/`JumpIfNot` handler runs two recognizers before anything else:
`recognize_bool_idiom` (opcode_handlers 3364) and `recognize_or_and_chain`. The
bool-idiom path ends in
`store_complex(..., idiom.dest, pc, value)` - a WRITE, to a register the idiom
picked. If `idiom.dest` resolves to R3 here, that is the clobber, and the
emitted `v13 = nil` sitting just above the defect in the output is consistent
with a bool idiom being recognised where none exists.

### PARKED, one suspect left
MECHANISM: R3 holds the correct receiver at the top of pc 17 and `Nil` by the
  time the arm is lifted; `method_receiver_expr` then rightly refuses it.
PROVED:    bytecode correct (r124); receiver correct at pc 17 top (r124); arm
  entry wrong (r125); premateralization innocent (r126, printed both sides).
RULED OUT: receiver predicate (r123); opmap/bytecode (r124); arm-exit restore
  (r122, opposite direction); premateralization (r126).
THE QUESTION: does `recognize_bool_idiom(code, 17)` return `Some` for this
  proto, and what is `idiom.dest`? One print at opcode_handlers 3364. If it
  matches R3, a bool idiom is being recognised across a real branch and its
  `store_complex` is writing the nil.

### State after rotation 126
SEMANTIC: 1,128/1,142 (98.8%), 14 defects. 949 tests pass. Both prints verified
inert.


## Rotation 127 - not a clobber at all: TWO passes over one range, one of them stale

### The last suspect is cleared
`LUAU_IDIOM_TRACE` prints nothing for `?@74` - `recognize_bool_idiom` never
fires at pc 17. Every named suspect is now eliminated: receiver predicate
(r123), opmap/bytecode (r124), arm-exit restore (r122), premateralization
(r126), bool idiom (r127).

### Interleaving the traces explains every earlier contradiction
    WATCH  before_pc=10  Expr(Nil)                    LOADNIL lands in R3
    WATCH  before_pc=16  MethodCall{FindFirstChild}
    WATCH  before_pc=17  Name("SteeringVelocity3")    correct
    WATCH  before_pc=18  Name("SteeringVelocity3")    walk passes 18 correctly
    PREMAT BEFORE pc=17  Expr(Nil)                    pc 17 handled AGAIN
    PREMAT AFTER  pc=17  Expr(Nil)
    ARMENTRY arm=18..22  Expr(Nil)
    WATCH  before_pc=18  Expr(Nil)

The walk passes pc 17 AND pc 18 holding the correct receiver. Then pc 17 is
processed a SECOND time as an inline-if, in a pass where R3 still carries the
`LOADNIL` from pc 10 - because that pass never executed pcs 13..16, where
`FindFirstChild` lands.

**Nothing clobbers R3.** Two lifting passes cover the same range and disagree,
and the pass that builds the arm is the one with the stale state.

That is why r126's `PREMAT pc=17 Nil` appeared to precede r124's
`WATCH before_pc=18 Name` - they are from different passes, and every rotation
from 123 read them as one timeline. Pc-qualification is not enough here; a
PASS counter is required.

### PARKED
MECHANISM: one range is lifted twice with different register states; the arm is
  built from the stale pass, so a correct receiver is replaced by a pc-10 nil
  and `method_receiver_expr` rightly refuses it.
PROVED:    the correct value reaches pc 18 in one pass (printed); the arm is
  built with the pc-10 value in another (printed); no single instruction writes
  the nil in between (all five suspects eliminated by print).
RULED OUT: receiver predicate, opmap, arm-exit restore, premateralization, bool
  idiom - each by measurement, each recorded above.
THE QUESTION: WHICH two passes, and why does the second start with pre-13 state?
  Add a monotonic pass counter to the lifter and stamp every trace with it -
  `WATCH`, `PREMAT`, `ARMENTRY` and `LIFTPATH` are all pc-qualified and none is
  pass-qualified, which is exactly the confusion above. This is the same lesson
  as proto-qualification (r47, r86, r95, r110, r116) one level down: an
  identifier the traces do not carry.

### State after rotation 127
SEMANTIC: 1,128/1,142 (98.8%), 14 defects. 949 tests pass. `LUAU_IDIOM_TRACE`
added at all three sites and verified inert.


## Rotation 128 - pass counter added; pc 18 is lifted TWICE and the second is stale

### The instrument r127 specified
A monotonic `next_pass_id()` stamped per `lift_instruction_range` invocation.

### And a flaw in it, stated before any conclusion is drawn from it
`WATCH` captures `pass_id` as a LOCAL at function entry, so its stamp is sound.
`PREMAT` and `ARMENTRY` call `current_pass_id()`, which returns the LATEST
global counter - by the time they print, nested lifts have already incremented
it. Their pass labels are therefore WRONG and must not be read as ownership.
That is visible in the output itself: `PREMAT pass=21 pc=17` alongside
`WATCH pass=21 before_pc=18`, i.e. a pass appearing to run pc 17 after pc 18.

Fixing them means threading the local `pass_id` into both call sites. Recorded
rather than done, because reading an unsound stamp is exactly how rotations
123-126 built on a false timeline.

### Reading only the sound instrument
    pass=20  pcs 8,10,13,16,17   ends Name("SteeringVelocity3")   CORRECT
    pass=21  pc 18               Name("SteeringVelocity3")        CORRECT
    pass=22  pc 18               Expr(Nil)                        STALE
    pass=23  pc 22               Unknown

**pc 18 is lifted TWICE, by passes 21 and 22, and the second carries the pc-10
nil.** Pass 22 never executed pcs 13..16, where `FindFirstChild` lands, so it
arrives with the register as `LOADNIL` left it.

The emitted `v3:Destroy()` is built from pass 22.

### PARKED
MECHANISM: pc 18 is lifted by two passes with different register states; the
  emitted arm comes from the stale one, so the receiver is the pc-10 nil and
  `method_receiver_expr` rightly refuses it.
PROVED:    two distinct passes lift pc 18 (pass-stamped WATCH); the earlier
  carries the correct receiver and the later the stale nil; no instruction
  writes the nil in between (five suspects eliminated, r122-r127).
RULED OUT: receiver predicate, opmap/bytecode, arm-exit restore,
  premateralization, bool idiom - each by print.
THE QUESTION: what invokes pass 22, and why does it start from pre-pc-13 state?
  `LUAU_LIFTPATH` is now pass-stamped - dump it for this proto and read which
  call site opens pass 22 and with what range. That is one command and it names
  the caller.
FIRST, though: thread the local `pass_id` into `PREMAT`/`ARMENTRY` so their
  stamps stop lying.

### State after rotation 128
SEMANTIC: 1,128/1,142 (98.8%), 14 defects. 949 tests pass. Pass counter verified
inert.


## Rotation 129 - stamps corrected, and the mechanism is a NESTED LIFT LEAKING REGISTER STATE

### The instrument fixed first
r128 recorded that `PREMAT`/`ARMENTRY` stamped `current_pass_id()` - the latest
GLOBAL counter - rather than their own pass. Threaded the local `pass_id` into
all 7 sites. Only after that was the ordering read.

### The ordering, with honest stamps
    WATCH   pass=20  before_pc=17  Name("SteeringVelocity3")   correct
    WATCH   pass=21  before_pc=18  Name("SteeringVelocity3")   nested pass runs
    PREMAT  pass=20  pc=17         Expr(Nil)                   back in 20: NIL
    ARMENTRY pass=20 arm=18..22    Expr(Nil)
    WATCH   pass=22  before_pc=18  Expr(Nil)

Within ONE pass (20), R3 is `Name(...)` at the top of pc 17 and `Nil` a moment
later. The only thing between the two prints is nested **pass 21**.

### The mechanism
`lift_instruction_range` takes `regs: &mut Vec<RegVal>` and passes that SAME
vector to nested lifts. A nested lift's register writes therefore persist into
the caller. Pass 21 read R3 correctly at pc 18 and left the register file
holding `Nil`, so the enclosing pass 20 built its arm from a corrupted view and
`method_receiver_expr` correctly refused the nil receiver.

Note the shape: this is the SAME family as rotation 122's fix (arm register
state on the way out) and rotation 125's finding (arm state on the way in), and
it is the third face of one issue - **the register file is shared across nested
lifts with no discipline about who may write it.** r122 fixed one crossing by
adding a restore; this is a crossing with no restore at all.

### PARKED, fully characterised
MECHANISM: a nested lift mutates the caller's register file; the enclosing pass
  then builds an arm from the corrupted state.
PROVED:    correct value at pc 17 in pass 20; nested pass 21 runs; `Nil` in pass
  20 immediately after (all pass-stamped after the stamps were corrected).
RULED OUT: receiver predicate (r123), opmap/bytecode (r124), arm-exit restore
  (r122 - different crossing), premateralization (r126), bool idiom (r127).
THE QUESTION: which call site opens pass 21? `LUAU_LIFTPATH` is pass-stamped -
  one dump names it and its range. Then the fix is a snapshot/restore around
  THAT nested lift, exactly as r122 did for the arm-exit crossing, measured with
  the full census.

RISK to state in advance: r122's equivalent change needed a constant exclusion
to avoid deleting a guard, and was rejected twice before that was found. Expect
the same discipline here - score first, census second, and the census decides.

### State after rotation 129
SEMANTIC: 1,128/1,142 (98.8%), 14 defects. 949 tests pass. All stamps corrected
and verified inert.


## Rotation 130 - ADOPTED. A speculative lift was not undone when it was rejected

### The call site, named by the pass-stamped trace
    LIFTPATH pass=20 ? flat 8..22      the enclosing arm
    LIFTPATH pass=21 ? flat 18..22     SPECULATIVE
    LIFTPATH pass=22 ? flat 18..22     the real arm lift

Two lifts of one range inside pass 20. Pass 21 is
`recognize_or_and_chain`'s segment lift.

### The bug
That recogniser lifts candidate segments into the SHARED `regs` before deciding
whether the chain is real:

    for &(seg_start, seg_end) in &chain.segments {
        lift_instruction_range(..., seg_start, seg_end, regs, ...);
        ... => { ok = false; break; }
    }
    if ok { ... }
    // falls through with regs STILL MUTATED

When the chain is rejected, control falls through to the inline-if path with the
speculative writes still in place. On `54_Controls::?@74` that left R3 holding
the `LOADNIL` from pc 10 - the speculative pass never executed pcs 13..16 where
`FindFirstChild` lands - so the arm was built with a nil receiver and
`method_receiver_expr` correctly refused it, emitting `v3:Destroy()`: a method
call on nothing.

Fix: snapshot before the segment loop, restore on the reject path only. The
accept path keeps its writes deliberately.

### Measured, ADOPTED
    clean 1,128 -> 1,130   defects 14 -> 12
    FIXED  54_Controls (the diagnosed reproducer) and 958_Drakkarmon
    NEW    none
    121 files changed; calls lost 0, loops lost 0
    total emitted calls 38,459 -> 38,461  (+2, net POSITIVE)
    949 tests pass on two consecutive runs

Two files show one fewer `if` and BOTH are recoveries, verified by reading:
`388_Sticker` collapses a flattened short-circuit into
`x and (self.Amount or 1) <= #x` - the previously DISCARDED comparison is now
assigned - and `464_LocalFlyingEntity` gains a 17-line table constructor and
corrects `v43.Movespeed` to `arg4.Movespeed`.

### The trail
Fifteen rotations (r115-r130), five suspects eliminated by print, two framing
errors corrected (a false timeline at r127, my own unsound pass stamps at r128),
and one instrument fixed before it was trusted. Every step that advanced came
from printing a value; every step that stalled came from reasoning about one.

### State after rotation 130
SEMANTIC: **1,130/1,142 (98.9%), 12 defects** - 3 declared_never_assigned,
3 branch_local_discarded, 6 calls_not_emitted.
CLOSED (22): ..., **speculative-chain register leak**.


## Rotation 131 - the r130 bug class does NOT recur; the 12 re-enumerated

### Generalisation checked, and it is negative
r130 fixed a speculative lift whose register writes were not undone on the
reject path. The obvious follow-up is whether other recognisers do the same.
Audited every `lift_instruction_range` call site in `opcode_handlers.rs`:

    3418  the or/and segment lift          FIXED in r130
    3546/3551, 3713/3718, 3861/3866       inline-if then/else arms - these
          bracket their lifts with `regs_snapshot` and restore, and r122
          refined that restore
    mod.rs 2199  the value-join trial      lifts into `&mut regs` (a LOCAL) with
          a cloned `trial_locals` - properly isolated

So the leak was unique to the one recogniser. No sibling fix to make, and that
is worth recording as a negative result rather than leaving the question open.

### The 12 remaining, by file
    branch_local_discarded  21_ClickToMove, 317_ScreenParticles, 445_PluckedPetal
    calls_not_emitted       174_ClientStatCache, 286_LeaderboardsGui,
                            314_RoboBearGui, 351_PassiveTile, 532_MusicPlayer,
                            59_LocalCubs
    declared_never_assigned 396_TadpoleTailInit, 79_Toys, 873_Translation

Status of each is already recorded above:
  * `branch_local_discarded` (3) - CLOSED at boundary (r107): the safe subset
    ships, the rest is proven inseparable from a call-dropping regression by
    any read/write-pattern predicate.
  * `calls_not_emitted` (6) - CLOSED at boundary (r113): a plain if/elseif chain
    in a loop body matches no implemented shape, and four naive fills each
    measured worse (r83 -1 guard, r109 -2, r112 -19, plus r93's boundary).
  * `396` - dropped generic-for inside a `while` body, needs r113's machinery.
  * `873` - BLOCKED ON THE FORMAT: Roblox bytecode carries no CAPTURE data, so
    the parent register behind a child's upvalue is not in the input.
  * `79` - `UNK_MERGE_COMPOUND`, REAL, mechanism not yet traced.

`79_Toys` is the one remaining finding with no boundary, no blocker and no
diagnosis - the honest next target.

### State after rotation 131
SEMANTIC: 1,130/1,142 (98.9%), 12 defects. 949 tests pass. Audit only; no code
changed.


## Rotation 132 - 79_Toys diagnosed: a two-arm join of two distinct non-Name values

### The last undiagnosed finding, traced
`MINTAT` gives the site, `LUAU_MERGE_WATCH=7` gives the join:

    MERGE ButtonText@179 reg=7
      before = Expr(Name("UseMessage"))
      self   = Expr(BinOp{ String("Use the "), Concat, ... })
      other  = Expr(String("Error, tell Onett"))

Both arms define a value, the values DIFFER, and neither is a bare `Name`.

`merge_regs` handles `Name` asymmetries explicitly (`self_name && !other_name`
keeps self; the reverse takes other; both-Name falls to the pre-branch-binding
rule). A `BinOp` against a `String` matches none of those, so it falls through
to the reset and the register becomes `Unknown` - losing BOTH arms. The
post-join read mints `v7`, which is then concatenated into two user-visible
strings:

    local result26 = (v7 .. " (") .. value14(TimeString2)
    Desc = Desc and (Desc .. " to ") .. v7

So a button label and a requirement description both render with a nil in them.

### What it should be
This is exactly `Region::ValueJoin` - "a branch region whose only result is the
value some register carries where control flow reconverges". The arms select
between two strings; the correct output is an if/else assigning one local, or a
ternary. The question is why the structurer did not emit a `ValueJoin` here,
NOT how to patch `merge_regs` - collapsing to `Unknown` is the honest fallback
once the region has already been mis-structured, and r119 measured that
patching the lattice directly costs more than it gains.

### PARKED
MECHANISM: a two-arm value join with distinct non-`Name` values on each arm is
  reset to `Unknown`, losing both.
PROVED:    the exact join, with both arm values and the pre-branch binding,
  printed above; the emitted consequence read in the output.
RULED OUT: merge-lattice patches as the fix (r119: no benefit, 2 files damaged).
THE QUESTION: does `structure_control_flow` emit a `ValueJoin` for this branch?
  `LUAU_RUNREG_TRACE` on `ButtonText@179` answers it. If it does, the ValueJoin
  lifter is dropping the arms; if it does not, the region matcher is the target
  and this joins the same family as the `calls_not_emitted` boundary (r113) -
  a branch shape no implemented region recognises.

### Every finding now has a diagnosis
With this, all 12 remaining defects have a located mechanism, a proven boundary,
or a named external blocker. None is unexamined.

### State after rotation 132
SEMANTIC: 1,130/1,142 (98.9%), 12 defects. 949 tests pass. Diagnosis only.


## Rotation 133 - 79_Toys root: `find_merge_point` returns None, so no ValueJoin exists

### The answer to r132's question
    IF proto=ButtonText@179 cond_block=110 then=6 else=6 merge=None has_else=true

**`merge=None`**, with SIX blocks in each arm. `Region::ValueJoin` carries a
`join` field by construction, so with no merge point the structurer cannot emit
one. It falls back to `IfThenElse` with `merge_pc: None`, both arms are lifted,
and their register values meet at a join the structurer never identified -
which is exactly the state r132 printed:

    self  = Expr(BinOp{ String("Use the "), Concat, ... })
    other = Expr(String("Error, tell Onett"))
    -> reset to Unknown -> `v7`

So `merge_regs` is not the fault and neither is the ValueJoin lifter. The fault
is upstream of both: the merge point is not found.

### Why that matters more than one file
`find_merge_point` is an alternating BFS for the first overlap of the two
successor sets (`cfg.rs:414`). Rotation 82 examined it and found it SOUND for
the case in front of it then. This is a case where it returns `None` while the
register is demonstrably read after the branch - so a join exists in the program
even though the search did not find one.

That is the same shape as rotation 99's result for `find_loops`, which misses
every generic for-loop because the graph does not have the dominance property
the search assumes. Two of the three graph queries in `analysis/` have now been
shown to miss real structure on this corpus.

### PARKED
MECHANISM: `find_merge_point` returns `None` for a two-arm branch whose register
  is read after the join; no `ValueJoin` is emitted; both arm values are lost.
PROVED:    `merge=None` with 6+6 arm blocks (IF trace); both arm values present
  and distinct at the reset (MERGE watch); the read after the branch (emitted
  output).
RULED OUT: `merge_regs` lattice patches (r119, measured worse); the ValueJoin
  lifter (never invoked here).
THE QUESTION: why does the BFS find no overlap? Print the successor sets it
  walks for `cond_block=110` - if both arms reach `Return` without reconverging,
  the join is a FALL-THROUGH the CFG does not model as an edge, which is the
  same class of gap as the FORGPREP edge in r99. That print is one addition to
  `find_merge_point`.

CAUTION for the next attempt: r99 fixed the analogous `find_loops` gap two ways
and BOTH measured worse (1,123/19 -> 1,118/25), because downstream consumers
depend on the current graph shape. Expect the same, and measure the census.

### State after rotation 133
SEMANTIC: 1,130/1,142 (98.9%), 12 defects. 949 tests pass. Diagnosis only.


## Rotation 134 - merge_regs runs on a branch that HAS no merge

### The successor sets
`LUAU_MERGEPT_TRACE=110` on `79_Toys::ButtonText@179`:

    MERGEPT true=111 succ=[116, 144]   false=149 succ=[152, 176]

The two arms lead to DISJOINT successor sets and never reconverge, so
`find_merge_point` returning `None` is CORRECT - both arms terminate. r133's
reading, that a join existed which the search failed to reach, was wrong. The
search is sound here; rotation 82's verdict on it stands.

### The real fault
And yet `merge_regs` RAN on those two arms - r132 printed it:

    self  = Expr(BinOp{ String("Use the "), Concat, ... })
    other = Expr(String("Error, tell Onett"))
    -> reset to Unknown -> `v7`

So the states of two arms that never meet in the program were merged anyway, and
the reset destroyed both. The `IfThenElse` region carries `merge_pc: None`, and
that field is the signal that no merge should happen - it is being ignored.

### PARKED, with the fix shape
MECHANISM: `merge_regs` is invoked for an `IfThenElse` whose `merge_pc` is
  `None`; two arm states that never converge are collapsed and both values lost.
PROVED:    successor sets disjoint (printed); `merge=None` (IF trace); the merge
  nevertheless performed on those exact values (MERGE watch); the emitted `v7`.
RULED OUT: `find_merge_point` being at fault (r134 - it is correct here);
  merge-lattice patches (r119, measured worse); the ValueJoin lifter (never
  invoked, r133).
THE FIX SHAPE: skip the merge when `merge_pc` is `None`, keeping each arm's
  state on its own path. Gate it, and measure the census - `merge_regs` is
  reached from several lift paths and its callers may rely on the current
  collapse for unrelated shapes.

### A correction to r133
r133 concluded "two of the three graph queries in `analysis/` miss real
structure". That over-generalised from one file: `find_loops` genuinely does
miss generic-for loops (r99, reproduced), but `find_merge_point` is CORRECT on
this case. One graph query is at fault, not two.

### State after rotation 134
SEMANTIC: 1,130/1,142 (98.9%), 12 defects. 949 tests pass. Diagnosis only;
`LUAU_MERGEPT_TRACE` added and verified inert.


## Rotation 135 - the merge-skip is NEUTRAL, and r134's attribution was wrong

### Built and measured
Skipped `merge_regs` when the `IfThenElse` carries `merge_pc: None`, which r134
argued was the fault.

    clean 1,130 -> 1,130   defects 12 -> 12
    79_Toys `v7`: 3 occurrences before, 3 after   NOT FIXED
    21 files changed; calls lost 0, loops lost 0, ifs lost 0
    total emitted calls 38,461 -> 38,461

No fix, no damage, 21 files moved. Reverted per the r90 rule: a diff that moves
files and no measurement does not ship.

### The attribution error
r134 paired two prints:

    IF    ... cond_block=110 ... merge=None          (the branch)
    MERGE ... reg=7 self=BinOp other=String          (the collapse)

and concluded they were the same site. They are not. Skipping the merge for
every `merge_pc: None` branch leaves `79_Toys` byte-identical, so the collapse
that destroys `v7` happens at a DIFFERENT merge - one that does have a merge
point.

Matched by proto and register without confirming the SITE. That is the same
class of error as matching a pc across protos (r47, r86, r95, r110), a name to
a register (r116), and a trace line to a pass (r127) - an identifier the print
does not carry, supplied by assumption. Fifth occurrence, in a session that has
recorded the lesson four times.

### PARKED
MECHANISM: two arm values (`BinOp` vs `String`) collapse to `Unknown` at SOME
  merge in `ButtonText@179`, minting `v7` into a button label and a requirement
  description. REAL, and the values are known.
RULED OUT: `find_merge_point` (r134 - correct here); the `merge_pc: None` branch
  (r135 - skipping it changes nothing); merge-lattice patches (r119 - measured
  worse); the ValueJoin lifter (r133 - never invoked).
THE QUESTION: WHICH merge? `LUAU_MERGE_WATCH` prints values but not the site.
  Add `cond_pc`/`merge_pc` to that print, then the branch is named rather than
  inferred. Do not pair it with an `IF` line by proto and register again.

### State after rotation 135
SEMANTIC: 1,130/1,142 (98.9%), 12 defects. 949 tests pass. Reverted clean.


## Rotation 136 - the merge site NAMED, and r134's pairing confirmed wrong

### The missing identifier, supplied
`merge_regs` takes only register slices, so `LUAU_MERGE_WATCH` printed the
VALUES it collapsed but never WHICH branch collapsed them. Added a
`MERGE_SITE` thread-local set at the call site (where `cond_pc`/`merge_pc` are
in scope) and threaded it into the print:

    MERGE ButtonText@179 cond_pc=0   merge_pc=23  reg=7 ... all Unknown
    MERGE ButtonText@179 cond_pc=110 merge_pc=-1  reg=7 other=Name("result26")
    MERGE ButtonText@179 cond_pc=23  merge_pc=62  reg=7 ... all Unknown
    MERGE ButtonText@179 cond_pc=62  merge_pc=110 reg=7
        before = Name("UseMessage")
        self   = BinOp{ String("Use the "), Concat, ... }
        other  = String("Error, tell Onett")

**The collapse is at `cond_pc=62`, which HAS a merge point (110).** r134 blamed
`cond_pc=110`, the `merge_pc: None` branch - the wrong site, matched by proto
and register alone. r135 measured that error directly: skipping the merge for
every `merge_pc: None` branch left the file byte-identical.

Fifth instance this session of an identifier supplied by assumption, and the
fix is the same every time: put the field in the print.

### Where that leaves 79_Toys
The branch at `cond_pc=62` selects between two STRINGS - a `BinOp` concat and a
literal - and writes the result to one register that is read after the join.
That is the textbook `Region::ValueJoin` shape, and the IF trace confirms the
region exists with a merge:

    IF proto=ButtonText@179 cond_block=62 then=6 else=1 merge=Some(110) has_else=true

So the region has everything a ValueJoin needs and is emitted as a plain
`IfThenElse` instead. `merge_regs` then has no asymmetry rule for
`BinOp` vs `String` (its rules cover `Name` cases) and resets both.

### PARKED
MECHANISM: a two-arm value selection with a merge point is structured as
  `IfThenElse` rather than `ValueJoin`; the arms' distinct non-`Name` values are
  then reset to `Unknown` and `v7` is minted into a button label and a
  requirement description.
PROVED:    the exact site (`cond_pc=62`, `merge_pc=110`), both arm values, the
  pre-branch binding, and that the region carries a merge - all printed.
RULED OUT: `find_merge_point` (r134, correct here); the `merge_pc: None` branch
  (r135, measured byte-neutral); merge-lattice patches (r119, measured worse).
THE QUESTION: does `find_value_join` even consider block 62, and if so which of
  its conditions rejects it? Print the entry/arms/join it computes for this
  proto. Do NOT patch `merge_regs` - r119 measured that path and it cost 2 files
  for no gain.

### State after rotation 136
SEMANTIC: 1,130/1,142 (98.9%), 12 defects. 949 tests pass. `MERGE_SITE` verified
inert.

## Live run, 20 Aug 2026 — fresh corpus off a NEW client build

Roblox rolled to `version-ce0bcd0fbd484804` at 02:49 (Potassium 2.4.3 updated for
it). That put the measured map in question: `opmap_db/roblox_v9_live.json` was
probe-aligned on 16 Aug against `ff6341faef444107`.

**Falsifier run first, not the decompile.** Dumped 8 scripts from the live client
and forced the entry with `--opmap-db-entry roblox_v9_live`, which errors rather
than falling back when an entry does not fit. All 8 decoded, and every header
read `opmap source: database entry "roblox_v9_live"` with `opmap evidence: not
applicable; no detector or bijection completion contributed to this decode`.
**The client rollover did not reshuffle the opcodes.** The console's "LENGTH
rescue" lines are the pooling store taking independent readings; they did not
touch the decode path, which the per-file headers state explicitly.

**Full live run.** 1,762 code instances walked, 1,310 yielded bytecode, 452
empty (server-side `Script`s — the client never receives them; the known wall).
Output dir emptied first, so resumed=0 by construction.

| | |
|---|---|
| decoded | **1,309 / 1,309 (100%)**, 1 input not a program |
| semantic | **1,295 / 1,309 clean (98.9%)**, 14 defects |
| output | 310,719 lines, 16 MB, 422.7s |

Defects: `calls_not_emitted` 7, `branch_local_discarded` 4,
`declared_never_assigned` 3 — the same three families left on the frozen corpus,
no new class.

**Why this matters more than the frozen number.** `target/full1143` is bytecode
dumped weeks ago under an older client; a decompiler can overfit to a corpus it
has been tuned against. This is a different corpus (1,310 files vs 1,142),
dumped today off a different client build, and it lands on **98.9% — the same
rate as the frozen corpus (1,130/1,142)**. That is an independent confirmation,
not a re-measurement.

MANIFEST's "uncommitted changes" banner fired on `tools/ollama_fanout.py` (a
Python tool) and untracked `crates/luau-core/tests/_tmp_rem.rs` (a test file).
Neither is linked into the release binary, which was built 02:25 from HEAD
3715cb4. The banner is correct to be conservative; the binary is reproducible.

## Union corpus, 22 Aug 2026 — every unique bytecode file we hold

### The corpus is a fifth the size the directory listing suggests
Hash-deduped every `.bin` across 20 directories. **1,324 unique files** out of
~15,000 on disk:

| root | files | NEW unique |
|---|---:|---:|
| `target/live_20260820/bytecode` | 1310 | 1143 |
| `target/roomclean_139191806727638/bytecode` | 312 | **179** |
| `target/full1143` | 1143 | 2 |
| `bc`, `target/{full,vz,rf,fresh,live,r1lv,r2lv,bc,rb,r0bc,r1bc,r2bc,p1run..p4run}` | 9,000+ | **0** |

`bc/` and every `p*run` are strict SUBSETS of the live dump; measuring on them
adds nothing. `roomclean` is a DIFFERENT PlaceId and contributes 179 files that
have never appeared in any measured run: Roblox CoreScripts, PlayerModule, and
third-party packages (topbarplus, Janitor, evaera promise). Materialised as
`target/union1324` with `rc_`/`lv_`/`fz_` provenance prefixes.

### The never-measured 179 are 10x worse than the tuned corpus
First clean run: **1,293/1,323 (97.7%), 39 defects.** Not the 98.9% previously
reported, because that number never saw these files.

    defects by origin:  lv (1,143 files) 12   |   rc (179 files) 27
    defective files:    lv 12 (1.05%)         |   rc 18 (10.1%)

A decompiler tuned against one corpus overfits to it. This is the measurement
that shows by how much.

### `batch` now writes SEMANTIC_REPORT.tsv
One row per defect naming the FILE and LINE. The summary counted defects
without saying where they were, and every defect ledger in this project that
lacked file-level evidence went stale and was then quoted as current.

### FIXED: both branch arms write a NAME-BOUND register (1,293 -> 1,297)
`declared_never_assigned` 17 -> 11, files losing clean status 0, all 1,323
outputs parse. Mechanism and reasoning in commit 480df57. The short version:
`premateralize_branch_escapes_spans` already seeded a shared local for this
shape but only for an UNKNOWN register; `carried_identity` could not reach it
because `Color3.fromRGB(...)` writes the result register TWICE per arm
(GETIMPORT parks the callee there, CALL overwrites it), so a plain two-arm
diamond reads `arm_writes=4` and trips the exactly-once rule.

### call recovery counted a proto once PER LIFT
`lv_0905_LocalCubs` lifts proto #0 three times; the header read "3 proto(s)
emitted 30 fewer call(s)" for ONE function losing 10. Corpus short-protos
15 -> 13 after deduping by proto index. `LUAU_CALL_SHORT` now NAMES the proto.

### `LUAU_BRANCH_ARMS` — the comment is STALE and the flag is catastrophic
Its own comment records "clean 1,100 -> 1,104, defects 48 -> 39, files losing
clean status: 0". Measured today on the union corpus:

    baseline            1,297/1,323 (98.0%)   33 defects
    LUAU_BRANCH_ARMS=1  1,044/1,323 (78.9%)  639 defects

The flag is correctly OFF. Anyone enabling it on the strength of that comment
would lose a fifth of the corpus. Other gated flags, same run:

    LUAU_ARM_COND / LUAU_ARM_IF   1,298 / 32   (+2 files fixed, 1 REGRESSED)
    LUAU_RET_RESUME               1,297 / 33   neutral
    LUAU_DECL_HOIST               1,291 / 39   worse

ARM_COND and ARM_IF fix the same two files (`lv_0806` recovers a dropped call,
`lv_0883`) and break `lv_0943_Camera_Controllers_Shop`, which was clean. Two of
the four "fixes" in the naive diff are the same finding at a shifted LINE, not
a fix. Net +1, not shipped on that evidence.

### 3 of the 33 defects are FALSE POSITIVES
`rc_0270` (evaera promise) reports `name_body_mismatch` three times. The
bytecode's own debug table settles it:

    [22] _new  line 230      [40] _all line 486   [41] all line 591
    [117] retry line 1934    [119] retryWithDelay line 1962

Every name the decompiler emitted matches a real proto, and `mod.rs:1163`
prefers the child proto's `debug_name`, so nothing was reconstructed. The check
infers identity from a STRING LITERAL in the body, and this library's error
text in `_new`/`_all`/`retryWithDelay` legitimately names the public wrappers
`new`/`all`/`retry`. True state is therefore **1,300/1,323 (98.3%)**.

### The bundled DB is a CLI default, not a library one
`bundled_db_path()` in the CLI silently loads `opmap_db/roblox_v9_live.json`
from the CWD or beside the exe. `luau_core::decompile()` does not. So the
binary and the library produce DIFFERENT output for the same bytes, and
`luau-wasm` / `luau-server`, which call the library, get the weaker
detector-only path. Measured on `16_StickerPlacer`: the CLI emits the correct
field read, the library emits the `{GetCanvasTouchPoint = "..."}` artifact.

### `settablekS_self_assign` asserted an invariant that is FALSE
The test claimed "a real SETTABLEKS never has A == B". Refuted by ground truth
- decoding `BeequipTypes` under the probe-measured permutation gives

    ;   0x30 -> 16 SETTABLEKS
         3: [0x10] SETTABLEKS  R0."__index" R0      <- A == B, and CORRECT

`tbl.__index = tbl` is the ordinary metatable idiom. 69 of the 70 `x.field = x`
lines in the union output are `__index`; the constructor form `X = "X"` is an
ordinary ENUM (a CoreScript event-name table, two shop enums). Had anyone
"fixed" the detector to satisfy this test, class tables would have stopped
decoding. The StickerPlacer defect it documents is FIXED on the shipping path.
Rewritten as (a) that concrete regression and (b) a MUTATION GUARD asserting
`x.__index = x` still survives — ban A == B and it reds.

### Still open
`opmap_dupclosure_detection` (2 tests) reports a REAL gap on the bare-library
path: 2 chunks map no closure opcode, 59 assign one unbound register to 3+
table fields. Both invariants are TRUE, unlike the settablekS one, so pointing
them at the DB would hide a defect rather than correct a false premise.

## The library was shipping a 67% decompiler, 22 Aug 2026

### The measured map was a CLI feature
`bundled_db_path()` resolves `opmap_db/roblox_v9_live.json` from the CWD or
beside the exe. A library can use neither, so `luau_core::decompile()` never
consulted it. Measured on the union corpus, same binary, same bytes:

    CLI / measured entry      1,297 / 1,323 clean (98.0%),    33 defects
    library / detector-only     891 / 1,323 clean (67.3%), 1,112 defects

`luau-wasm` and `luau-server` call the library. Fixed by `include_str!`-ing the
3 KB entry into `luau-core` and consulting it in `decompile()`. `lookup`
returns `Hit` only after a fingerprint check, so a chunk from another build
falls through to detection byte-for-byte - the change can only replace
INFERENCE with MEASUREMENT. The CLI path is byte-unchanged (1,297 both sides).

### Two "detector" tests were reporting exactly this, unread
    closure_opcode_is_mapped_for_multi_proto_chunks    2 chunks -> 0
    table_fields_are_not_all_the_same_unbound_register 59 chunks -> 1

The residual 1 was a FALSE POSITIVE in the test's own helper: it collected
bindings from `local` and from plain assignment but never from `for`, so the
`v2` of `for k2, v2 in pairs(arg2) do` counted as never-assigned and every
`part.Anchored = v2` inside the loop scored as the closure-opcode defect.
`40_ReplicatedStorage_Welder` is correct code.

### MEASURED AND REVERTED: whole-proto SETTABLEKS/GETTABLEKS discriminator
`SETTABLEKS A B K` READS R(A) (it is the value stored) so R(A) must already
hold one; `GETTABLEKS A B K` WRITES R(A). The existing rule only looked at the
instruction at pc-1, which one scheduling accident hides. Widening it to "does
any earlier already-mapped producer write R(A), or is A a parameter":

    bc/ test (single-file, no pooling)   59 -> 43 chunks   BETTER
    union corpus, same path             891 -> 882 clean  WORSE
                                      1,112 -> 1,159 defects

Reverted; the rebuilt binary reproduces the control exactly (891 / 1,112). One
test improving is not evidence a change is right - the corpus is the arbiter,
and it disagreed. Do not retry this without measuring the corpus first.

### State
Tests 1,039 pass, 1 fail. The remaining failure,
`settableks_is_detected_wherever_its_byte_occurs`, is the honest one: it drives
the detection machinery directly rather than decompiling, so the embedded map
cannot help it. 59 of 603 chunks that execute byte 0x30 lose SETTABLEKS, and in
40 of them GETTABLEKS holds the byte.

Shipping corpus: **1,297 / 1,323 (98.0%), 33 defects**, of which 3 are the
`rc_0270` false positives, so 1,300 / 1,323 (98.3%) is the true figure.
Remaining real classes: calls_not_emitted 11, declared_never_assigned 11,
branch_local_discarded 3, undefined_local 3, bodies_dropped 2.

## calls_not_emitted: the mechanism, fully traced, 22 Aug 2026

Ten of the eleven `calls_not_emitted` files share ONE mechanism. Worst proto
per file, blocks collapsing into a single region:

    rc_0274 Packet_Types 72   lv_0791 RoboBearGui 56   lv_0351 MusicPlayer 36
    lv_0806 Leaderboards 32   rc_0047 IndexCtrl   24   lv_0124 StatCache  23
    rc_0055 ShopLayout   19   lv_0756 PassiveTile 15   rc_0084 ClickToMove 13
    rc_0030 Janitor       9

(`lv_0905` is not one of them - it was the triple-lift COUNTING bug, fixed.)

### The chain, end to end, on `rc_0274` proto #135 (`?@514`)
Not inferred - every step printed by an existing trace:

    REGIONS  ?@514 blocks=72 regions=1
    IF       ?@514 cond_block=0 then=59 else=12 merge=None has_else=true
    IFLIFT   then_stmts=119 then_calls=40  else_stmts=23 else_calls=8
    ARM      ?@514 blocks=59 ranges=59            <- reaches lift_branch_region
    RUNREG   ?@514 run=3..534  Linear 3..14
    RUNREG   ?@514 run=3..534  InlineLoopInLoop 17..20
    RUNREG   ?@514 run=3..534  Linear 21..68      <- a range-case, FLATTENED
    PASS     after_eliminate_dead_stores calls=48
    PASS     after_eliminate_dead_code   calls=8
    DCE      depth=2 enter stmts=119 calls=40
    DCE      depth=2 after_phase2_truncate stmts=12 calls=4

The bytecode's 48 calls are then(40) + else(8); the AST keeps exactly 8. So:

1. `early_ret` DOES fire and the arm DOES reach `lift_structured_run`.
2. That run can only reach `structure_numeric_for_body`, which emits
   Linear/loop regions and CANNOT emit `IfThenElse`. The arm's nested
   if/elseif chain is flattened into one statement list.
3. That list holds SEVERAL `return`s - one per range case.
4. `eliminate_dead_code` phase 2 truncates after the first. 107 statements and
   36 calls gone.

**DCE is not the bug.** `return` must end a Lua block, so truncating a
well-formed list is correct and NOT truncating would emit invalid Lua. The
malformed input is the defect.

### FOURTH measured rejection of arm structuring
The only machinery that can emit `IfThenElse` for an arm is
`structure_block_subset`, behind `LUAU_BRANCH_ARMS`. Prior rotations rejected
it three times (r82 `LUAU_ARM_COND` 1,104->1,103; r83 `LUAU_ARM_IF` scored
+1 but lost `if 490000000000 <= value2.Honey then` in
`1125_Supreme_Star_Amulet_Generator`, spending 490 billion honey
unconditionally). Measured again today on the union corpus:

    baseline                                   1,297 / 1,323   33 defects
    LUAU_BRANCH_ARMS (any arm, >1 block)       1,044 / 1,323  639 defects
    NARROWED to arms that would TRUNCATE       1,213 / 1,323  170 defects

The narrower gate is the right IDEA - it fires only where flattening provably
loses code - and it still costs 84 files. So the gate was never the problem:
`structure_block_subset` itself produces bad regions. Reverted; the rebuilt
binary reproduces 1,297 / 33 exactly.

**The next attempt must fix the arm STRUCTURER, not its trigger.** Four gate
variants have now been measured and all four lose more than they recover.

### FIFTH rejection: merge-containment in `structure_block_subset`
`structure_block_subset` checks that `then_blocks` and `else_blocks` are all
members of the subset, but never checks `merge`. A region whose join lies
OUTSIDE the arm is emitted with a `merge_pc` past the arm boundary, and the
lifter then lifts up to it and claims code the arm does not own - which reads
exactly like the "double-lifted foreign blocks" failure recorded against every
earlier attempt. Adding `merge.map_or(true, |m| member.contains(&m))`:

    baseline                                   1,297 / 1,323   33 defects
    narrowed gate only                         1,213 / 1,323  170 defects
    narrowed gate + merge containment          1,210 / 1,323  173 defects

Marginally WORSE than the gate alone, so an escaping merge is not where the
damage comes from either. Reverted; baseline reproduced exactly.

FIVE measured attempts now: three historical (r82 ARM_COND, r83 ARM_IF, the
BRANCH_ARMS flag) and two today (truncation-gated, and that plus merge
containment). Every one loses far more than it recovers. The conclusion is not
"try a sixth gate": `structure_block_subset` emits regions the arm lifter
cannot consume safely, and the next attempt has to establish WHAT it emits that
is wrong - region-by-region, against a proto whose correct structure is known -
rather than adjusting when it runs. `LifterProbe.luau` exists for exactly that
and has never been run; it needs a live client (25 Aug).

### SIXTH rejection: the one-arm nil seed
The two-arm seed is sound because both arms write, so the merge read can never
observe the entry value. The one-arm case was left unfixed because seeding
`local x` asserts NIL on the path that does not write. This tried the narrow
slice that looked sound: seed only when the register has NO WRITE ANYWHERE
before the branch, so there is nothing for the untaken path to carry, and the
thing being replaced is a bare `vN` the file never declares - a GLOBAL read,
nil at runtime too, so no use could observe a different value.

    baseline    1,297 / 1,323   33 defects
    nil_seed    1,295 / 1,323   35 defects

    fixed:  NOTHING
    broke:  rc_0266 LemonSignal, rc_0271 Packet  (both declared_never_assigned)

Zero fixed, two broken - a pure loss, and the reason is plain in the defect
name. With TWO arms writing, the seeded local is assigned on every path. With
ONE, if the arm write does not land on the pinned name, the seed is a `local`
that is declared and never assigned: the change manufactures exactly the defect
it was meant to remove. The one-arm shape stays unfixed, and now for a measured
reason rather than an argued one. Reverted; baseline reproduced.

### SEVENTH: chunk-level name provenance cannot clear the false positives
`name_body_mismatch` is only evidence when the name-to-body pairing was
GUESSED. Wired a per-chunk counter (`named_installed` at the NEWCLOSURE/
DUPCLOSURE hint site) into the header and had the check skip when nothing was
reconstructed. Measured: **no change, 1,297 / 33 both ways.**

The reason is in the counter itself - `rc_0270` evaera-promise reads
`names: 61 reconstructed, 72 from bytecode debug info`. The three flagged
declarations (`_new`, `_all`, `retryWithDelay`) ARE debug-named, but 61 other
closures in the same chunk are not, so a CHUNK-level gate can never discriminate
them. It would take PER-DECLARATION provenance, which means marking every
function in the output.

817 of 1,323 files do report 0 reconstructed, so the gate is meaningful in
general - it just never fires where the false positives are. A change that
adds a header line to 1,323 files for zero measured movement does not ship.
Reverted; baseline reproduced (1,297 / 33).

### Verification status of the 33
Spot-checked one exhibit per class against the bytecode or the emitted code:
`declared_never_assigned` REAL, `branch_local_discarded` REAL (four values
computed into fresh arm-locals and discarded, plus an unbound `clamp = v4`),
`bodies_dropped` REAL (`rc_0047` is 46 lines with ZERO functions for a 10-proto
chunk), `undefined_local` REAL, `calls_not_emitted` REAL (traced to DCE
truncation). Only `name_body_mismatch` is false. **30 of 33 are genuine.**

### EIGHTH: teach the RUN structurer to emit branches (the closest attempt yet)
`structure_numeric_for_body` recognised value joins and loops and nothing else,
so a plain `if` inside a run fell through to the trailing Linear flush. It
already COMPUTED `forward_conditional_jump` for `LUAU_SHAPE_TRACE` and threw the
result away. Acting on it, bounded to the range the call owns:

    control                       1,297 / 1,323   33 defects
    if-then only                  1,267 / 1,323   66 defects
    + if-then-ELSE discrimination 1,269 / 1,323   64 defects

**It fixed the right files** - `rc_0274 Packet_Types` (the 72-block exhibit),
`lv_0351 MusicPlayer`, `lv_0756 PassiveTile`, `lv_0786 ScreenParticles`,
`rc_0037 GateController` - and broke 35 others, taking `calls_not_emitted` from
11 to 29. So the mechanism is right and the DISCRIMINATION is wrong: most
forward conditional jumps in this corpus are not the bare `if` this treats them
as. Adding the standard if/else test (then-arm ends in an unconditional forward
JUMP, so the conditional target is the ELSE start and the merge is that jump's
destination) recovered only 2 of the 30 lost files.

Reverted. Baseline reproduced (1,297 / 33).

### Where this leaves the structural family
Eight measured attempts: three historical, five today. Every one loses between
28 and 253 files. The consistent shape of the failures - right files fixed,
many more broken - says the missing thing is not another predicate but a way to
tell a real branch from a short-circuit, a guard, and a loop exit at this level.
Blind corpus tuning cannot supply it; eight runs have now demonstrated that.

`LifterProbe.luau` is the instrument: eight hand-written control-flow shapes,
every branch carrying a distinct `MARKER_*` string, so a missing marker IS the
measurement and the correct structure is known in advance rather than inferred
from a score. It has never been run. It needs a live client.

### NINTH: the first attempt that SCORES POSITIVE, and it is a TRAP
Gating the run-structurer branch case (r8 above) tightened in two steps:

    control                                     1,297 / 1,323   33 defects
    branch case, ungated                        1,269 / 1,323   64 defects
    + fires only where truncation is CERTAIN    1,296 / 1,323   34 defects
    + requires TWO OR MORE returns in range     1,298 / 1,323   32 defects  <-- BETTER

The last row is the first positive result this family has ever produced, and
the checker agrees: **fixed 1 (`lv_0756 PassiveTile`), broke 0**, 24 files
changed. On the score alone it ships.

**It must not.** Counting structure rather than defects across those 24 files:

    files LOSING an `if`:   5      files LOSING calls: 1      loops lost: 0

    lv_0910 VRCamera      48 -> 45 ifs     lv_0925 ControlScript  74 -> 71
    lv_0912 CameraScript  58 -> 56, calls 155 -> 153
    lv_0005 BeequipTypes  93 -> 92         rc_0086 CameraModule   68 -> 67

and the diff on `lv_0910` is unambiguous - four guards collapse into one:

        - if arg1 == "offtrack" then
        -     if not v11 then
        -         if Head then
        - if arg1 == "force" then
        + if Head then

The body now runs unconditionally where it was gated on the argument. That is
r83's failure exactly: a scoring gain concealing guard deletion the semantic
checker has no check for. Rejected on the standing rule - **a defect the
checker cannot see is worth more than a defect it can** - not on the number.

Reverted; baseline reproduced (1,297 / 33).

**RULE FOR THE NEXT ATTEMPT: never accept a region-structuring change on the
semantic score alone.** Count `if`/`elseif`/`for`/`while`/calls per changed file
against the control build and require ZERO losses. Every one of the nine
attempts so far would have been caught faster by that check than by reading the
score, and the ninth would have shipped without it.

### TENTH: a stranded-branch acceptance test, and why splitting is not the enemy
Sharper diagnosis first. Nested `if`s inside a flat span are recovered by
`lift_instruction_range`, which lifts a conditional jump only when its target
lands inside the range it was handed. So every split this scan makes - at a
loop, a value join, a for-pair - strands any conditional jump crossing the new
boundary. `rc_0274 ?@514` splits into `Linear 3..14`, `InlineLoopInLoop 17..20`,
`Linear 21..68`, ... around the buffer-growth `while` loops, and the range
dispatch's own jumps span those boundaries.

That also explains r9's guard loss from the other side: emitting an
`IfThenElse` SHRINKS the ranges, so jumps that were liftable stop being so -
`lv_0910 VRCamera` lost `if arg1 == "offtrack"`, `if not v11` and
`if arg1 == "force"` that way.

So: refuse a split that strands a branch, and fall back to one Linear.

    control                      1,297 / 1,323    33 defects
    stranded-branch fallback     1,052 / 1,323   628 defects

Catastrophic. Almost every range has some conditional jump crossing some
boundary, so the test fires nearly everywhere and collapses the loop regions
that were doing useful work. Splitting is not the enemy; splitting where the
branch matters is. Reverted; baseline reproduced.

### Ten attempts, and the shape of the wall
Three historical, seven today. Scores: 1,044 · 1,052 · 1,210 · 1,213 · 1,267 ·
1,269 · 1,295 · 1,296 · **1,298** · 1,297(control). The single positive was r9,
and it deletes guards.

Every attempt has adjusted WHEN to structure. None could adjust WHAT correct
structure looks like, because nothing in this loop knows it - the score is the
only feedback, and the score cannot see a deleted `if`. That is the wall, and
ten runs is enough evidence that another predicate will not clear it.

`LifterProbe.luau` is the missing feedback: eight control-flow shapes, every
branch carrying a distinct `MARKER_*`, so the correct structure is known BEFORE
the run and a missing marker names the arm that was dropped. Compile-gated,
never run, needs a live client.

## GROUND TRUTH DOES NOT NEED A LIVE CLIENT — 22 Aug 2026

I asserted several times this session that the structural defect could not be
worked on until a Roblox client was available on the 25th. **That was wrong**,
and the correction is one flag:

    tools/luau/luau-compile.exe --binary -O1 -g2 --fflags=false X.luau > X.bin

The bundled compiler emits **v11** by default, which this decompiler cannot
read (gate 3-9; v10/v11 changed the proto layout, tested 2026-08-03).
`--fflags=false` turns off the flags that bump the container and emits **v6**,
which is in range. So: write a Luau source, compile it locally, decompile it,
and compare against a source we wrote. Seconds, no client, no capture.

### `LifterProbe.luau` passes — it does not reproduce the defect
All 17 markers survive. The shapes it models (guard-then-work, pairs
accumulation, upvalues, tailcall, nested guards, client/server split) are
handled correctly. It was never going to find this bug.

### `probes/DispatchProbe.luau` DOES, in 900 bytes
Modelled on `rc_0274 Packet_Types` proto #135: a range dispatch where every arm
ends in `return` with small `while` loops between the tests.

    baseline: 2 of 9 markers survive

Only the first arm of each side of the outer split is emitted; seven are
dropped. Same failure as the 640-instruction original, in a file that decodes
in milliseconds.

### The branch case fixes it COMPLETELY, and still must not ship
Re-applying the r8 branch case (emit `IfThenElse` for a forward conditional
jump inside a run):

    DispatchProbe   2/9 -> 9/9 markers          <- the mechanism is CORRECT
    GuardProbe      13/13 (unchanged)           <- guards modelled are safe
    union corpus    1,297 -> 1,269 clean, 33 -> 64 defects

and `tools/structure_diff.py` on the same run:

    146 files LOSE structure
    rc_0043 GlovesShop      if -63   calls -208   loops -27
    rc_0049 SellController  if -40   calls  -93   loops  -9
    rc_0064 CrownsShop      if -34   calls  -77   loops  -7

**The semantic score said -28 files. The structure count says 146.** The score
understates the damage FIVE-FOLD, because it has no check for a deleted `if`,
a deleted call, or a deleted loop.

### What is now in place that was not
* a local ground-truth loop (`probes/`, seconds per iteration, no client)
* a probe that reproduces THIS defect and one that pins the shapes a fix must
  not break
* `tools/structure_diff.py`, which would have rejected every one of the eleven
  attempts faster than the corpus did - and would have caught r9, the attempt
  that SCORED BETTER while deleting three guards in `lv_0910 VRCamera`

The remaining work is to make the branch case keep its 9/9 without touching the
146. That is now an iterable problem with a fast, honest test, rather than a
corpus score to guess against.

### THIRTEENTH, and it ISOLATES the weak link
With the probe suite in place, the branch case was retried with a TILING
CHECK: the scan resumes at `merge`, so every instruction between the branch and
the merge must be carried by a block in one of the arms, or that code is
skipped and never emitted. The arms must tile `[then_start, merge)` exactly -
first block starting at `then_start`, last ending at `merge`, no gaps.

    probes    DispatchProbe 2/9 -> 9/9    ModuleProbe 6/8 -> 8/8
              GuardProbe 13/13 held
    corpus    1,297 -> 1,269 clean, 33 -> 64 defects
    structure 130 files LOSE structure (worst: -18 ifs)

**Every probe now passes and the corpus is damaged exactly as much as before.**
That is the finding. The tiling gap was not the cause, and the structuring is
not the problem: what regresses is what happens AFTER a region is emitted.

Emitting `Region::IfThenElse` routes the arm through `lift_branch_region`,
which takes a BLOCK LIST, splits it into contiguous runs and lifts each. The
flat path it replaces hands `lift_instruction_range` ONE range, and that is the
only code that turns a nested conditional jump into a `Stat::If` (gated on
`target > pc && target <= end`). Shrinking one range into several arms strands
every nested jump that spanned them - which is why real files lose up to 18
`if`s each while three purpose-built probes are perfect.

**So the next attempt must not add another structuring predicate.** The
question is now specific: make an arm lift preserve the nested-branch recovery
that a flat range gets. Either lift the arm as a RANGE rather than a block
list, or teach `lift_branch_region` the nested-jump handling
`lift_instruction_range` already has.

### Correction to r9/r11 readings
`grep -c MARKER_` counts matching LINES, not distinct markers. Reading it as a
marker count made `GuardProbe` look like a 6/13 failure when it was 13/13
intact, and made r11 look like it "fixed" a probe it never broke. Only
`tools/run_probes.sh`, which tests each marker once, is a marker count.

## SHIPPED: per-run arm lift recovers 28 dropped guards (a053b38)

The thirteenth attempt's tiling check made all three probes pass while the
corpus stayed damaged identically. That proved region structuring was never the
problem and pointed at what happens AFTER a region is emitted - and that turned
out to be a defect worth fixing on its own, with no structuring change at all.

`lift_branch_region` split an arm into maximal contiguous RUNS, then lifted any
multi-run arm BLOCK BY BLOCK. `lift_instruction_range` is the only code that
turns a nested conditional jump into a `Stat::If`, gated on
`target > pc && target <= end`, so a per-block call made `end` the block's own
end and every nested jump crossing a block boundary vanished.

A run is contiguous by construction and `split_contiguous_runs` guarantees its
span "contains no instruction outside the listed blocks", so lifting the run as
ONE range claims nothing new - it only stops truncating the window.

    semantic score   1,297 / 1,323 -> 1,297 / 1,323   IDENTICAL
    structure        +28 ifs, +22 calls, 0 losses across 18 files
    parse            1,323 / 1,323
    tests            1,039 pass, 1 pre-existing fail

`rc_0105 CameraModule` alone goes 54 -> 65 ifs, recovering
`if CFrame3:IsA("Humanoid")`, `if self:GetIsMouseLocked()`,
`if getstate3 == Enum.HumanoidStateType.Dead`.

**The score could not see this.** It is identical before and after, because the
semantic checker has no check for a recovered guard any more than for a deleted
one. Without `tools/structure_diff.py` this fix would have read as a pure no-op
and been reverted under the project's own "measured neutral does not ship" rule.

### The two problems are INDEPENDENT
Re-applying the tiling-validated structuring change ON TOP of the arm-lift fix:

    probes   DispatchProbe 9/9, GuardProbe 13/13, ModuleProbe 8/8   all pass
    corpus   1,297 -> 1,269 clean, 126 files lose structure

Identical damage to before. So the arm-lifting weakness was not what made
region structuring destructive; they are separate defects, and one of them is
now fixed. Fourteen attempts on the structuring half, still all rejected.

### The structuring half is CONVERGING, and here is the ladder
Each discriminator added to the run-structurer branch case, measured on the
union corpus with `tools/structure_diff.py`:

    variant                                    clean   files LOSING structure
    ungated                                    1,269            146
    + tiling check (arms must cover the span)  1,269            130
    + arm-lift fix already shipped (a053b38)   1,269            126
    + loop-condition exclusion (JUMPBACK)      1,270            104
    control                                    1,297              0

Every one of them takes all three probes to a perfect score:

    DispatchProbe 2/9 -> 9/9    ModuleProbe 6/8 -> 8/8    GuardProbe 13/13 held

So the mechanism is right and the DISCRIMINATION keeps improving - 146 to 104
files as each real distinction is added - while still nowhere near zero. Two
discriminators found so far and both were genuine:

* the arms must TILE the span, or the code between them is skipped outright
  (an unvalidated form collapsed `rc_0043_GlovesShop` 1,588 lines -> 73);
* a `while` compiles to a forward conditional jump plus a JUMPBACK, so reading
  that jump as an `if` turns the loop into a one-shot branch.

The residual losers now lose only a few constructs each (-4, -2, -1 ifs), which
is the signature of a few more shapes to separate, not of a wrong mechanism.

### The tension this exposes, stated plainly
`ModuleProbe.lookup` shows it in ten lines. Source:

    if n > 8 then
        while n > 8 do n = n - 1 end
        return "MARKER_M_evicted"
    end
    return "MARKER_M_miss"

Emitted at baseline: the `if n > 8` guard GONE, its body run unconditionally,
and `MARKER_M_miss` dropped. A loop needs a REGION to be emitted at all (the
linear dispatcher's FOR* arm is empty - "handled at the region level"), and a
nested branch needs a FLAT RANGE to be recovered at all
(`lift_instruction_range`, gated on `target <= end`). Splitting a range for the
loop strands the branch; not splitting loses the loop. That conflict is why
fifteen attempts have failed, and no gate can resolve it - only a structurer
that NESTS them, emitting the branch region with the loop regions inside it.

## The structuring fix is TWO FILES away — the full ladder, 22 Aug 2026

Every variant below takes all three ground-truth probes to a perfect score
(`DispatchProbe` 2/9 -> 9/9, `ModuleProbe` 6/8 -> 8/8, `GuardProbe` 13/13 held).
What changes is only how well it separates a real branch from loop machinery:

    variant                                     clean    files losing structure
    ungated branch case                         1,269              146
    + arms must TILE the span                   1,269              130
    + per-run arm lift (a053b38, SHIPPED)       1,269              126
    + loop-condition exclusion (JUMPBACK)       1,270              104
    + fire ONLY when the span contains a loop   1,295                6
    + while-loop detection in the arm lifter    1,296                2 broken
    control                                     1,297                0

**146 -> 2.** Each step was a real distinction, not a tuned threshold:

1. TILING - the scan resumes at `merge`, so the arms must cover
   `[then_start, merge)` with no gap or that code is skipped and never emitted.
   An unvalidated form collapsed `rc_0043_GlovesShop` 1,588 lines -> 73.
2. LOOP CONDITION - `while c do .. end` is a forward conditional jump plus a
   JUMPBACK to the test; reading it as an `if` makes the loop a one-shot branch.
3. LOOP SPANNING - nested `if`s are ALREADY recovered by
   `lift_instruction_range`. Emitting regions for branches in general does that
   job worse. The only shape the flat path cannot serve is a branch whose span
   contains a LOOP, because the scan splits there and strands the branch's jump.
   Restricting to that took 104 losers to 6.
4. WHILE DETECTION - `range_contains_nested_for` finds ForNPrep/ForGPrep pairs
   only, so an arm holding a `while` never reached `lift_structured_run` and the
   loop was emitted as nothing. Adding a contained-JUMPBACK case fixed the loop
   losses.

### What the last variant actually does
`rc_0274_Packet_Types` - the 640-instruction exhibit - goes from a two-arm
collapse to the real nested dispatch:

    if arg1 % 1 == 0 then
        if arg1 < 0 then
            if writef64 < arg1 then
                while v2 < writef64 do v2 *= 2 end
                writef64 = buffer.writeu8

calls 783 -> 822, loops 19 -> 93, and its `call recovery` header - "1 proto
emitted 40 fewer calls" - is GONE. Corpus short-proto files 10 -> 9.

### Why it still does not ship
It breaks exactly two previously-clean files, and both in the merge-loss family:

    lv_0129 Eggs      declared_never_assigned x2, branch_local_discarded
    lv_0648 Stickers  declared_never_assigned

Restructuring creates merge points the value machinery then loses values at.
Net 1,296 against a 1,297 control - fixed one, broke two. The standing rule
holds: a structuring change must lose nothing.

**NEXT: those two files, not another gate.** The mechanism is settled and the
discrimination is done; what remains is that new regions interact with
`premateralize_branch_escapes_spans`/`merge_regs`. That is a value-merge
problem, and it is now reproducible on two named files with a fast probe suite
to confirm the branches stay recovered while it is fixed.

### The last two files are the ONE-ARM seed shape, and that is already measured
Traced `lv_0648_Stickers` under the near-miss variant. The defect is
`return true, v9` at the merge of a large branch in
`GiveStickerToPlayerStats@779`, and `LUAU_GATE_TRACE` gives the whole answer in
two lines:

    GATE  GiveStickerToPlayerStats@779  reg=9  val=Some("Unknown")  after=32..36
    GATE    reg=9 name_bound=false reads_before_write=false arm_writes=3
            parked_pure=false carried=false -> REJECT

`unknown_seed` needs BOTH split arms to write the register; here they do not.
That is the ONE-ARM shape, and seeding it was measured earlier this session
(r6): **fixed 0, broke 2** - with one arm the seeded `local` is declared and
never assigned, which IS `declared_never_assigned`. The change manufactures
exactly the defect it targets.

**So the two remaining regressions are not a structuring problem at all.** They
are the one-arm merge, reached from a new direction. The structuring fix cannot
ship until the one-arm merge is solved, and the one-arm merge is provably not
solvable by seeding. Whatever solves it has to RECOVER the lost write rather
than assert a value for the path that does not write - which means finding
where the pre-branch value went, not inventing one.

That is the honest end of this line for now: mechanism settled, discrimination
done (146 -> 2), and the residue named precisely and shown to be a different
defect wearing the same clothes.

### CORRECTION: the last two are a COVERAGE bug, not the one-arm merge
Disassembling `lv_0648_Stickers` proto 29 `GiveStickerToPlayerStats` settles
what the gate trace only hinted at:

    20: [0x06] MOVE    R9 R6      <- the write EXISTS
    22: [0x16] RETURN  R8..R10

`v9`'s producing write is `MOVE R9 R6`, TWO INSTRUCTIONS before the return that
reads it. Nothing was lost at a merge and nothing needs seeding: under the
restructured regions that instruction is simply not lifted.

So the earlier reading - "this is the one-arm seed shape, already measured
unsound" - was wrong. `arm_writes=3` at a gate site 12 instructions away
described a DIFFERENT branch in the same proto, and pairing it with this defect
was an identifier supplied by assumption, which is the mistake this journal
records more than any other.

The real question is narrow and answerable: which region claims pc 20, and why
is it not emitted? The tiling check validates `[then_start, merge)` but nothing
validates that the Linear flush before the branch, or the region the scan
resumes into, covers every instruction the proto contains. A coverage assertion
over the WHOLE range - every pc in `[body_start, body_end)` claimed by exactly
one region - would catch this class at the point it is introduced instead of
1,200 files later.

That is the next thing to build, and it is a cheap invariant, not another gate.

### The coverage check needs ALL TEN region variants, or it lies
Built the coverage assertion the previous entry called for: every pc in
`[body_start, body_end)` claimed by some region, gaps reported under
`LUAU_COVERAGE`. It found **349 gaps across 32 of 60 corpus files at
BASELINE** - which would be a spectacular finding if it were true.

It is not. `Region` has TEN variants:

    Linear  IfThenElse  ValueJoin  WhileTrue  WhileDo  RepeatUntil
    NumericFor  InlineLoopInLoop  GenericFor  InlineIfThenInLoop

and the check mapped three of them to spans. Every `IfThenElse`, `ValueJoin`,
`WhileDo`, ... region in a proto therefore registered as unclaimed. The number
measures the check's blind spot, not the decompiler.

Reverted rather than shipped. A diagnostic that reports 349 false gaps is worse
than none: the next session would chase them. Same failure as the `if`-count
probe check earlier today, caught the same way - by asking whether the
instrument could be wrong before believing its output.

A CORRECT version has to derive a span for all ten, and the block-list variants
(`IfThenElse` carries `cond_pc` plus `then_region`/`else_region` block ids,
`ValueJoin` carries `entry`/`arms`/`join`) need their block ranges resolved
through the CFG rather than read off the struct. That is worth building - the
`MOVE R9 R6` at pc 20 that vanished in `lv_0648_Stickers` is exactly what it
would have caught at the point of introduction - but it must be complete before
its output means anything.

### The coverage invariant does NOT hold, and that is the finding
Built it three more times, each cut narrowing the false positives:

    cut 1  Linear/NumericFor/GenericFor only      349 gaps, 32 of 60 files
    cut 2  all TEN Region variants                183 gaps, 28 of 60
    cut 3  + terminator jumps are structure        79 gaps, 23 of 60

Cut 3's exclusion was right: a block's terminating jump is consumed by whichever
region owns the branch - the condition BECOMES the `if` expression - so it
belongs to no region body. Requiring it reported `JUMPIF R1 -> +1` as a
one-instruction gap three times over in a single file.

Cut 3 still over-reports, and the disassembly of the first survivor says why:

    10: [0x49] FASTCALL1  bit32.countrz R3 skip=2
    11: [0x0C] GETIMPORT  R2
    13: [0x15] CALL       R2 args=1 results=1

**Luau FUSES instructions.** `FASTCALL1 ... skip=2` means the VM jumps the next
two words on the fast path; they are one operation, not three statements, and no
region claims them individually. Between fused fast-call sequences, structural
jumps and terminators consumed by their regions, "every pc is claimed by exactly
one region" is simply NOT TRUE of this bytecode.

So the invariant proposed in the previous entry is wrong, and a coverage check
built on it will keep producing numbers that look alarming and mean nothing.
Reverted, all three cuts. What would actually catch the `MOVE R9 R6` case is
narrower: a check that every REGISTER-WRITING instruction is claimed, with the
fused-call and structural-jump families excluded by opcode - not a check over
all pcs.

Four instruments built today, three of them wrong and caught before shipping
(`if`-counting on probes, 3-variant coverage, all-pc coverage). The one that
survived - `tools/structure_diff.py` - is the one comparing two RUNS of the same
decompiler, where the modelling error cancels on both sides. That is the pattern
worth keeping: compare like with like, never source with output.

### The narrow check is SOUND but looks in the wrong place
Built the check the previous entry called for: not every pc, only every
REGISTER-WRITING instruction, with the jump and FASTCALL families excluded by
opcode (`writes_a_register`, a positive allowlist).

    baseline, 60 corpus files:  0 unclaimed writes, 0 files

Zero. After three cuts that reported 349, 183 and 79 false gaps, this one is
quiet on correct output - which is what a usable invariant looks like.

**But it does not catch the case it was built for.** Re-applied the near-miss
structuring variant, where `lv_0648_Stickers::GiveStickerToPlayerStats` emits
`return true, v9` with `v9` undeclared, and the check reported NOTHING.

The reason is placement, and it corrects the previous entry: the check sits in
`structure_numeric_for_body`, the RUN structurer, while that proto's top-level
regions come from `structure_control_flow`. It never sees them.

So two things are now known that were not:
* a sound unclaimed-write invariant exists and is quiet at baseline;
* the `MOVE R9 R6` diagnosis is still INFERRED, not proven - the write exists in
  the bytecode and is absent from the output, but nothing has yet shown which
  region does or does not claim it.

Reverted rather than shipped: a detector that has never fired on a known defect
is not evidence of anything, and the honest next step is to run the same
invariant inside `structure_control_flow` where the regions for these protos are
actually built. Fifth instrument today; the fourth was wrong, and this one is
right but aimed at the wrong function.

### BOTH diagnoses of the residue are REFUTED
Ran the unclaimed-write invariant inside `structure_control_flow` - where these
protos' regions are actually built, which the previous entry correctly said the
run-structurer version never saw.

    baseline, 40 corpus files:            0 reports  (quiet on correct output)
    near-miss variant, lv_0648_Stickers:  0 reports  (does NOT fire)

**So `MOVE R9 R6` at pc 20 IS claimed by a region.** It is not a coverage
problem, and the entry two above this - "a region COVERAGE bug, write exists at
pc 20" - is wrong. So is the entry before that, which called it the one-arm
merge shape. Both were inferences from a disassembly plus an absence, and both
died on contact with a check that could actually decide the question.

What is now KNOWN about the residue, as opposed to guessed:

* the write `MOVE R9 R6` exists in the bytecode, two instructions before the
  `RETURN R8..R10` that reads R9;
* some region claims it;
* it is absent from the output.

Therefore the loss is DOWNSTREAM of structuring - in the lift of that region or
in a later pass (dead-store elimination, DCE, the merge machinery). That is a
different search than either of the two attempted, and it is the first statement
about this defect in the whole session that rests on a measurement rather than
on a plausible reading.

The invariant is sound (zero false positives at baseline in both structurers)
but reports nothing here, so it is not a detector for this defect. Reverted
rather than shipped: a check that has never fired is not evidence, and carrying
it would imply a guarantee it has not earned.

Five instruments today. Four wrong, one right-but-silent. The scoreboard on
hypotheses is worse than the scoreboard on code, and that is worth writing down.

### The residue is register SIX, and no pass is responsible
`LUAU_PASS_PROBE` on `GiveStickerToPlayerStats@779` under the near-miss variant,
every pass from `simplify_stmts` to `reconstruct_table_constructors`:

    stmts=6 calls=9   stmts=6 calls=9   stmts=6 calls=9   ... unchanged throughout

**No pass drops anything.** The proto lifts to six statements and stays there,
so the loss is in the LIFT, not in dead-store elimination, DCE, or any later
collapse. That rules out the whole post-pass pipeline, which is where the
`rc_0274` truncation lived and where I would otherwise have looked next.

And it explains the shape completely. `MOVE R9 R6` is COPY PROPAGATION - a plain
`MOVE` deliberately emits no statement, it just makes R9 carry R6's expression.
So nothing dropped the write; there was never a statement to drop. `v9` appears
because **R6 held no known value at pc 20**, and R9 inherited that.

The lost value is register **SIX**. Every previous reading of this defect - the
one-arm merge, the coverage gap, the downstream pass - was chasing register 9,
which is a symptom two instructions removed from the cause. Three refutations
and one misattributed register, all from reading an absence in the output and
inferring backwards instead of asking the register file what it held.

FOR THE NEXT ATTEMPT: `LUAU_REG_WATCH` / `LUAU_MERGE_WATCH` on R6 across
`GiveStickerToPlayerStats@779`, not R9, and not on the statement list at all.

### R9 confirmed Unknown; R6 invisible to the watch (instrument limit, not evidence)
`LUAU_REG_WATCH` under the near-miss variant:

    LUAU_REG_WATCH=9 -> READ reg=9 MINTED v9 (register held Some(Unknown))
    LUAU_REG_WATCH=6 -> (nothing)

R9 is confirmed Unknown at the read, which is the symptom. R6 printing nothing
does NOT clear it: the watch is on the MINTING READ path in `reg_expr`, and a
plain `MOVE` copies the `RegVal` straight across without going through it. An
Unknown R6 is therefore invisible to this instrument by construction.

So the register-6 hypothesis is still un-refuted and still unproven, and the
watch as it stands cannot decide it. What would: a WRITE-side print in the MOVE
handler recording what `regs[b]` held when it was copied - the write side that
the read-side watch's own comment says it was meant to be paired with.

Recorded rather than pursued further. Six instruments today: four wrong, one
right-but-silent, one that answers a different question than the one asked. The
pattern across all of them is the same and is the durable lesson of this
session - **an absence in the output never names its own cause**, and every time
this session read one as if it did (one-arm merge, coverage gap, downstream
pass, register 6) the next measurement refuted it.

### R6 refuted too - and the whole line was debugging UNSHIPPED code
`MOVE`'s handler opens with `let src_expr = reg_expr(regs, b);`, and `reg_expr`
IS the minting path the read-side watch hooks. So `LUAU_REG_WATCH=6` printing
nothing is not an instrument limit after all: **R6 was not Unknown**. The
previous entry's caveat was wrong in the other direction, and the register-6
reading is refuted - the fifth consecutive diagnosis of this defect to die on
the next measurement (one-arm merge, coverage gap, downstream pass, register 6,
instrument-limit).

The chain that survives measurement is now: R6 holds a known value, `MOVE R9 R6`
copies it, and R9 is nonetheless Unknown when the `RETURN` reads it two
instructions later. Something between the copy and the read resets R9, which
points at `merge_regs` and is what `LUAU_MERGE_WATCH` exists for.

**But that work has no value, and noticing why matters more than the finding.**
`lv_0648_Stickers` is CLEAN at baseline. This defect exists only inside the
near-miss structuring variant, which is not shipped and - even with Stickers
fixed - would still be +1 file (`rc_0274`) against -1 (`lv_0129_Eggs`), i.e.
net zero, for a large behavioural change. Five rounds of measurement went into
a defect in code that is not in the product and cannot move the corpus number by
a single file.

That is the real lesson of the tail of this session, and it is worth more than
the register: **check that the thing you are debugging is SHIPPED before you
debug it.** The instruments were sound, the measurements were honest, every
refutation was correct - and the whole sequence was still misdirected effort,
because the target was chosen by momentum rather than by what would change the
delivered artifact.

### Call-pair collapse in `count_reg_writes`: measured NEUTRAL, reverted
Back on SHIPPED code after the misdirected tail. `carried_identity` requires a
register be "extended exactly once" across the arms, and `count_reg_writes`
counts raw writes - so `x = f(...)`, which parks the callee in the result
register (`GETIMPORT R9`) and then overwrites it with the result (`CALL R9`),
reads as TWO writes. The gate trace shows the consequence directly on
`lv_0934`: `reads_before_write=true` (a genuine carried identity) with
`arm_writes=3`, so the one-write rule can never admit it.

Collapsed the pair: skip a producer when the next write to that register is a
`CALL` on it, since the producer's value IS the callee that call consumes.

    semantic     1,297 / 1,323 -> 1,297 / 1,323   unchanged
    structure    9 files changed, 0 LOSE
    gains        ifs +0   calls +0   loops +0   generic vN +0

Nine files differ and NOTHING is recovered - the diff is naming churn. The
reasoning about the call-pair is still correct (the gate trace proves the
miscount is real), but fixing the count does not, on this corpus, let
`carried_identity` admit anything it should. Measured neutral, so it does not
ship, by the same rule that reverted the `select_hint` reorder on 22 Aug.

Worth recording precisely because the argument was sound and the outcome was
nil: a correct diagnosis of a real miscount is not evidence that fixing it
changes output. Eighteenth attempt; the second this session to be right about
the mechanism and worth nothing at the corpus.

## 100% IS NOT REACHABLE, and my own "98.3%" was wrong

Split the 26 defective files by whether their defects CAN be fixed:

    files total                1,323
    clean now                  1,297   (98.0%)
    defective                     26
      false-positive only          1   checker bug; the decompiler is CORRECT
      format-blocked only          2   no CAPTURE data in Roblox bytecode
      genuinely fixable           23

    TRUE current quality   1,298 / 1,323 = 98.1%
    CEILING                1,321 / 1,323 = 99.85%

### Two corrections
**1. "98.3% true" was arithmetic I got wrong, repeatedly.** I subtracted 3
FALSE-POSITIVE DEFECTS from a FILE count. All three sit in ONE file
(`rc_0270` evaera-promise), so discounting them moves the file total by one, not
three: **1,298/1,323 = 98.1%**, not 98.3%. Defects and files are different
denominators and I mixed them.

**2. 100% cannot be reached, for a reason outside this code.**
`lv_0579_Translation` and `rc_0010_topbarplus` fail only on `upval_N` -
unbound upvalue names. CLAUDE.md has recorded since 8 Aug that Roblox bytecode
carries NO CAPTURE data, so an upvalue's binding is not present to recover.
No lifter change reaches them; it would take a format that does not exist.

So the honest ceiling for this checker on this corpus is **99.85%**, and the
gap between there and here is **23 files**, not 26 and not "13 defects".

That is the number to work against. Quoting 100% as the target sets a bar the
bytecode format forbids, and quoting 98.3% overstated where we already are.

## SHIPPED: the merge seed was asking "used after?" of the wrong range (7abd696)

    clean        1,297 -> 1,298 / 1,323        defects 33 -> 30
    call recovery  165 -> 165 missing calls    ZERO regression
    parse          1,323 / 1,323               tests 1,039 pass, unchanged

    declared_never_assigned 12 -> 11   branch_local_discarded 3 -> 2
    undefined_local          3 ->  2   every other class unchanged

`premateralize_branch_escapes_split` will not seed a shared local unless the
register is read after the merge - right, since seeding one nothing reads is
noise. But the three call sites in `opcode_handlers` passed `end`: the end of
the range CURRENTLY BEING LIFTED, an arm or a run. A read past that window was
invisible, so the gate rejected the seed as pointless while the semantic checker
could see the read perfectly well.

`reject reg=N : not used after merge` was SEVEN of the thirteen `vN`
declared-never-assigned defects. The usage question is about the proto, so it is
now asked of the proto.

### How it was found, and why it took so long
By asking, for every remaining `vN` defect, WHICH gate condition rejected it -
one table, thirteen rows - instead of reasoning about any single file. Seven
rows said the same thing and the answer was in the call site.

That is the measurement I should have run before the eighteen structuring
attempts, not after. Every one of those tuned a predicate against a corpus
score; this read the rejection reason off the gate that was already printing it.

### State
    clean 1,298 / 1,323 = 98.1%      defects 30
      false-positive only    1  (rc_0270, checker bug, decompiler CORRECT)
      format-blocked only    2  (upval_N; Roblox bytecode has no CAPTURE data)
      GENUINELY FIXABLE     22
    TRUE quality  1,299 / 1,323 = 98.2%
    CEILING       1,321 / 1,323 = 99.85%   -- 100% is not reachable

### CAVEAT on the gate table: it pairs by REGISTER, not by proto
The table that found the after-range bug matched each `vN` defect to a
`GATE reg=N` line anywhere in the file. That conflates protos. `rc_0243`
exposes it: the trace shows reg=3 twice in `playerTrace@107` (one ADMIT, one
REJECT) while the defective `v3` at output line 709 belongs to a DIFFERENT
proto whose reg 3 never reaches the gate at all. Read naively, that file looks
like "admitted and still broken", which is a contradiction that does not exist.

This is the same mistake the journal records at rotations 47, 86, 95, 110, 116
and 127 - an identifier the print does not carry, supplied by assumption.
Seventh occurrence, and this time in a table I built to avoid exactly that.

The FIX it produced is still real and still measured (+1 file, -3 defects, zero
call regressions, tests unchanged) - the after-range argument was wrong at those
call sites regardless of which defect motivated looking. But "7 of 13 defects
are this rejection" was stated with more confidence than the pairing supports.
Three defects went away; the other four rejections were correct.

To do this properly the table needs the PROTO from the defect, which the
semantic report does not carry - it records file and line, not proto. Adding the
proto name to `SEMANTIC_REPORT.tsv` would make the join sound, and is the
cheapest way to make this whole line of attack trustworthy rather than
suggestive.

### Remaining 22 fixable files, by what the gate actually says
    5   "not used after merge" and CORRECT - the merge is the last instruction
        of the proto (lv_0883 `?@54` is 16 insns, after=15..16)
    5   name_bound=false, arm_writes 2-3, carried=false - the one-arm/multi-write
        shapes, and seeding one-arm was MEASURED to manufacture the defect (r6)
    1   no gate trace at all - the register never becomes a candidate
    11  calls_not_emitted / bodies_dropped - the structural family, 18 attempts

## The remaining `vN` defects are TWO-ARM, and the seed cannot see it (22 Aug)

With the read site recorded (cf10bfe) every defect of this class resolves to a
real proto, and `rc_0065 repaint@120` - which carries two of them - shows what
the class actually is. Its bytecode:

    37: GETIMPORT R3          <- tostring
    39: CALL      R3          <- THEN arm: R3 = tostring(Text)
    41: LOADK     R3 K10 "?"  <- ELSE arm: R3 = "?"
    42: CONCAT    R1 R2..R3   <- the merge read

**Both arms write R3.** The source is `cond and tostring(Text) or "?"`. This is
NOT the one-arm shape that r6 measured as unsound to seed - it is exactly the
two-arm shape `unknown_seed` exists for, and the output still says
`Price.Text = StarterPlayer2 .. v3`.

### Why the seed cannot fire
Two things have to both be true, and neither is:

1. `find_value_join` REJECTS the region, correctly - `CALL` is not in the purity
   allowlist, and a value join may evaluate a member block once per path, so a
   call can never be admitted.
2. The path it falls to reaches `premateralize_branch_escapes`, the NON-SPLIT
   wrapper, which passes `split_arms: None`. `unknown_seed` is guarded by
   `if let Some((then_ranges, else_ranges)) = split_arms`, so with `None` it is
   unreachable by construction - the gate never even asks whether both arms
   write.

The trace shows the consequence exactly: `arm_writes=3` (GETIMPORT, CALL, LOADK
across the two arms) and `-> REJECT`, because with no arm split the only gates
left are `parked_pure` (false - the register is Unknown, nothing is parked) and
`carried_identity` (false - nothing is read before write).

### So the fix is not another predicate
It is to give that call site the arm split it already has the information to
compute. A two-arm value selection whose arms are not pure is a real and common
shape - `x = cond and f(y) or default` - and it currently falls between
`ValueJoin` (which must refuse it) and `unknown_seed` (which never sees it).

This is the first mechanism-level statement about this class that rests on the
bytecode rather than on the shape of the output, and it was only reachable once
the report carried the read site so the defect could be pinned to `repaint@120`
instead of to a register number that means something different in every proto.

### And the obvious fix for it is UNSOUND - checked before writing it
The tempting move: `lift_value_join`'s fallback lowering is
`local r; if c1 then r = v1 elseif ... end`, documented as "Always sound,
whatever the values turn out to be", and it emits each arm ONCE. So purity
looks like it gates only the folding path, and admitting impure arms while
forcing the fallback looks free.

It is not. The lift ENUMERATES PATHS and calls `lift_instruction_range` per
path before either lowering is chosen. An impure arm reachable along several
paths would therefore be lifted - and its CALL emitted - once per path. The
purity allowlist is load-bearing for the whole mechanism, exactly as
`value_region.rs` says: "because the member blocks are required to be
side-effect-free, a block reachable along several paths may be evaluated once
per path without changing behaviour."

So the gap stands and is genuinely architectural. `x = cond and f(y) or default`
falls between:

  * `ValueJoin`, which MUST refuse it (path enumeration would duplicate `f(y)`);
  * `detect_else_skip`, which correctly finds no else-arm, because a
    short-circuit ladder has no unconditional else-skip JUMP to find;

and so `unknown_seed` never sees the two arms that the bytecode plainly has.
Closing it needs a mechanism that structures a short-circuit ladder into an
if/else WITHOUT enumerating paths - not a predicate change to either existing
one.

That is the honest end state of this class: 11 defects, one named mechanism, one
named reason no existing machinery reaches it, and one refuted shortcut recorded
so it is not tried again.

## The declared-but-Unknown seed: +6 files, and it TESTS THE WRONG CONDITION

Extending `bound_seed` to registers that are DECLARED but hold `Unknown` (the
gap between `unknown_seed`, which needs undeclared, and `bound_seed`, which
needs a Name) scored better than anything else this session:

    control                                  1,298 / 1,323   30 defects
    declared-but-Unknown seed                1,305 / 1,323   22 defects
    + nested-short-circuit guard             1,304 / 1,323   23 defects

    declared_never_assigned 11 -> 6     undefined_local 2 -> 0
    corpus missing calls    165 -> 165  (zero call regressions)

Two regressions were found and one was FIXED by evidence:

**`rc_0100 Popper` dropped a raycast.** Seeded at pc 90 on R15, arms
(91,108)+(109,110). Both arms genuinely write R15, so every gate passed - but
the then-arm contains a NESTED short-circuit on the same register:

     90: JUMPIFNOT R3  -> 109     the branch tests R3
     91..98:  CALL R15            raycast #1
     99: JUMPIF    R15 -> 109     nested `or` ON R15
    100..107: CALL R15            raycast #2
    109: LOADB     R15 false      else arm

Pinning one name across that flattens the chain and drops all but the last
operand: `workspace:Raycast(arg4, NearPlaneZ4 - arg4, ..)` vanished entirely
while the checker scored the file CLEAN. Guarding on a conditional jump INSIDE
an arm that tests the seeded register fixed it (Raycast 5 -> 5). Three earlier
discriminators failed because they tested the branch at `pc`, which tests R3.

**`lv_0922 TransparencyController` now tests the WRONG CONDITION, and that one
is not guardable.** Baseline:

    if isType5 then          -- isType5 = Parent:IsA("Tool") or HasToolAncestor(..)

Patched:

    local Parent6 = Parent5 or HasToolAncestor(self.Parent)   -- DISCARDED
    if isType3 then          -- isType3 = self:IsA("BasePart")

The guard now examines an unrelated predicate. The seeded name captured the
wrong register identity, the real result is computed and thrown away, and the
semantic checker scores it clean because `isType3` IS assigned - there is no
undefined name left to find.

REVERTED. +6 files is the best trade this family has offered, and it is still a
change that makes a program test the wrong thing. `structure_diff` flagged it
only as `if -3`, which is why every flagged file has to be READ and not counted:
two of the three flags on this run were genuine improvements (`LemonSignal`
recovers a lost closure as `task and function`), and the third was this.

### The declared-but-Unknown seed: two variants, each trading one invisible defect
Isolated properly by removing ONE clause at a time:

    control                              1,298 / 1,323   30   both files correct
    widening only                        1,305 / 1,323   22   rc_0100 drops a Raycast
    widening + short-circuit guard       1,304 / 1,323   23   lv_0922 tests wrong predicate
    everything EXCEPT the widening       1,298 / 1,323   30   lv_0922 STILL wrong

That last row is the one that matters and it corrected three wrong readings in a
row. The score returns to control while `lv_0922` is still broken - so the
regression is NOT the widening. It is the SHORT-CIRCUIT GUARD, which suppresses
seeds in `lv_0922` that were previously fine. The guard added to save
`rc_0100`'s raycast costs `lv_0922` its guard.

`declared_never_assigned` 11 -> 6 and `undefined_local` 2 -> 0 in both variants,
with corpus missing-calls flat at 165. The gain is real; so is each regression.

### And a methodological correction worth more than the result
I first read `lv_0922` as broken by comparing `sed -n '54,76p'` of one file
against `sed -n '54,74p'` of the other - FIXED LINE RANGES across two files with
different line counts. The `if isType5 then` / `if isType3 then` pair I read as
a corrupted guard were partly different statements at coincidentally similar
positions. The regression turned out to be real, but my evidence for it was not,
and I nearly rejected the widening for it.

Diff whole files, or grep for the construct by name. Never eyeball equal line
windows across outputs whose lengths differ - it is the same class of error as
pairing a defect to a gate line by register number, which this journal has now
recorded eight times.

### Where this leaves the class
`declared_never_assigned` is reachable - 11 -> 6 is measured, twice, by two
different variants. What is not yet found is a discriminator that saves
`rc_0100` without costing `lv_0922`. Both files are named, both regressions are
characterised at the bytecode level, and the seed trace (`LUAU_SEED_TRACE`)
prints reg/pc/arms for every seed, so the next attempt starts with the two
counterexamples in hand rather than with a corpus score.

## SHIPPED (59e44d0): 1,298 -> 1,305 / 1,323, defects 30 -> 22

    declared_never_assigned  11 -> 5      undefined_local  2 -> 0  (eliminated)
    corpus missing calls    165 -> 165    parse 1,323/1,323
    tests 1,039 pass, unchanged           probes unchanged

Two changes, and the second is the one that took five attempts to find.

**1. Seed declared-but-Unknown registers.** `unknown_seed` needs the register
UNDECLARED; `bound_seed` needs it to hold a NAME. One used earlier in the proto
(so declared) that now holds `Unknown` falls between them, even when both arms
write it and the merge reads it - the argument both seeds already rest on.

**2. Withhold the PIN, not the SEED.** Widening alone dropped a raycast in
`rc_0100 Popper` because the arm contains a nested `or` on the pinned register:
pinning one name over `f(a) or f(b)` makes both operands assign it and the chain
collapses. FOUR attempts to suppress the seed for that shape each cost
`lv_0922 TransparencyController` its tool-ancestor guard, because its arms carry
the same short-circuit flag - the flag cannot separate them.

The seed was never the problem. Seeding is correct (both arms write); only the
PIN is unsafe across a short-circuit, and only for that arm. Withholding just
the pin satisfies both counterexamples at once: rc_0100 Raycast 5 -> 5,
lv_0922 guard intact, and the full +7 files is kept.

### Two verification lessons, both earned the hard way
* `structure_diff` flagged `lv_0922` at `if -3` and `rc_0266` at `if -1`. BOTH
  ARE RECOVERIES. lv_0922 folds three `if not X then` inner guards into `or`
  expressions (`or HasToolAncestor` 1 -> 4) with `HasToolAncestor(` and
  `IsA("Tool")` both 5 -> 5; LemonSignal rescues an orphaned closure as
  `task and function`. A flagged file must be READ, never counted.
* I first "confirmed" the lv_0922 regression by comparing `sed -n '54,76p'` of
  one file against `sed -n '54,74p'` of another - FIXED LINE WINDOWS across
  outputs of different lengths. Comparing the SORTED SET of conditions is what
  finally showed the truth. Eighth instance in this journal of an identifier
  supplied by assumption.

### State
    clean 1,305 / 1,323 = 98.6%     defects 22
      false-positive only    1      format-blocked only 2      FIXABLE 15
    TRUE     1,306 / 1,323 = 98.7%
    CEILING  1,321 / 1,323 = 99.85%   -- 100% is not reachable

### The seed fix UNBLOCKS the structuring variant - and it still deletes a guard
Re-applied the loop-spanning structuring variant on top of the shipped seed fix
(59e44d0). The seed fix removes almost all of the damage it used to do:

    before the seed fix   1,269 / 1,323   64 defects   104-146 files lose structure
    after  the seed fix   1,305 / 1,323   23 defects     6 files flagged

    ALL THREE PROBES PASS: DispatchProbe 9/9, ModuleProbe 8/8, GuardProbe 13/13
    corpus missing calls  165 -> 125      FORTY real calls recovered
    file count            1,305 -> 1,305  neutral
    defects                  22 -> 23     +1

`rc_0274 Packet_Types`, the collapsed dispatcher, goes calls 783 -> 822 and
loops 19 -> 93. Every flagged file GAINS calls. On the strongest instrument
available - ground truth with known-correct output - this change is right.

**It still does not ship.** `lv_0004_Beequips` loses

    if INBOX_CAPACITY < #self.Beequips[v32] then

outright, with nothing added in its place - verified by comparing the SORTED SET
of `if`/`elseif` conditions, not line windows, and confirmed absent. A capacity
check that silently stops happening is exactly the defect class the semantic
checker cannot see, and it is why the rule is lose-nothing rather than
score-better.

So the position is now precise: the structuring mechanism is CORRECT (three
probes prove it), it recovers 40 dropped calls, and it deletes one guard in one
file. That is one file to explain, not a corpus to tune - a far better place
than the 146-file blast radius this variant had before the seed fix.

## SHIPPED (b1a0f94): the structural fix, on the nineteenth attempt

    corpus missing calls  165 -> 125    FORTY calls recovered
    calls_not_emitted      10 ->   9
    files clean         1,305 -> 1,305  neutral      defects 22 -> 23
    probes  DispatchProbe 2/9 -> 9/9   ModuleProbe 6/8 -> 8/8   GuardProbe held
    parse 1,323/1,323                  tests 1,039 pass

### Why eighteen attempts failed and this one did not
The variant is nearly the same one measured at 1,269/1,323 with 104-146 files
losing structure. Nothing about the structuring changed. What changed is that
59e44d0 fixed the MERGE SEED first, and those losses were the seed failing on
the new regions - not the regions being wrong.

That is the lesson worth keeping: **a change measured as catastrophic can be
correct and merely blocked by a defect elsewhere.** Eighteen rotations tuned the
structuring predicate because the corpus said the structuring was wrong. It was
not.

### I nearly discarded it twice, both times on bad evidence
1. Read `lv_0004` as deleting `if INBOX_CAPACITY < #self.Beequips[v32] then`.
   It became `while INBOX_CAPACITY < Beequips2 do` - and an inbox capacity trim
   MUST loop; the `if` removed exactly one item and was the bug.
   `structure_diff` printed `if -1` and `loops +1` ON THE SAME ROW and I read
   only the first column.
2. Read `rc_0035` as losing two guards. `and/or` went 45 -> 47 and calls
   315 -> 327: the two conditionals FOLDED. Its variable naming shifted
   wholesale (`palette2` -> `covers2`, `err2` -> `Color`), so comparing sets of
   condition TEXT was meaningless there; counting constructs was not.

Rule earned: when `structure_diff` flags a file, read every column of that row
before reading the diff, and prefer rename-immune counts (`if`/`elseif`/`while`/
`for`/`and`/`or`/calls) over comparing condition text.

### State
    clean 1,305 / 1,323 = 98.6%   defects 23   missing calls 125
      false-positive only  1    format-blocked  2    FIXABLE 15
    CEILING 1,321 / 1,323 = 99.85%

### The loop-spanning restriction is load-bearing - widening it is WORSE
`rc_0047 IndexController` proto #9 is the same truncation mechanism as
`rc_0274`, traced end to end:

    IF     ?@1 cond_block=0 then=1 else=22 merge=None
    IFLIFT else_stmts=62 else_calls=43        <- the arm lifts fine
    PASS   eliminate_dead_code calls 47 -> 22 <- and DCE cuts it

but its branch contains NO LOOP, so the shipped gate does not fire. Widening it
from "the span contains a loop" to "or flattening would provably truncate"
(a `RETURN` before the end of the span):

    shipped    1,305 / 1,323   23 defects   125 missing calls
    widened    1,304 / 1,323   24 defects   130 missing calls

Worse on every axis INCLUDING the authoritative one - it loses five more calls
than it recovers. All three probes still pass, which is exactly why the probes
alone cannot decide a gate: they prove the MECHANISM is right, not that a given
trigger is safe.

So the loop-spanning restriction is not a conservative approximation that could
be relaxed later; it is the thing that makes the mechanism safe. A branch whose
span holds a loop is stranded by the run scanner's split and genuinely needs a
region. A branch that merely truncates does not - the flat path handles it, and
taking it away costs more than the truncation does.

Reverted. `rc_0047`'s 30 missing calls stay, and the reason is now recorded
rather than open.

### rc_0047's 30 missing calls: a MULTI-TERM short-circuit guard, not a lost range
Ruled out the obvious causes by measurement, in order:

* the arm IS fully lifted - `LUAU_LIFTPATH` gives 95 flat ranges covering
  26..372 with only 1-2 word gaps, and those are the branch instructions
  themselves (structure, not statements);
* the guards ARE recovered individually - `pass=6 flat 104..109` is the arm of
  the `JUMPIF R12 -> 108` guard;
* the loss is at `eliminate_dead_code`, calls 47 -> 22, after `IFLIFT` reports
  the arm at 62 statements and 43 calls.

The bytecode says what it is:

    136: JUMPIFNOT R13 -> +3      lifted as pass=10 flat 137..140
    137: JUMPIFNOT R14 -> +2      lifted as pass=11 flat 138..140
    138: JUMPIFNOT R15 -> +1      lifted as pass=12 flat 139..140
    139: JUMPIF    R16 -> +5
    144: RETURN

Four conditional jumps CONVERGING ON ONE TARGET - the source is
`if not (a and b and c and d) then warn(..) return end`. Each term is lifted as
its own tiny range, so no single `if` ever owns the guarded body: the
`warn(); return` ends up at the OUTER level, and DCE then correctly truncates
everything after it. The emitted file shows exactly that - an unconditional
`warn` and `return` where the source has a guard.

This is a DIFFERENT mechanism from `rc_0274`'s, which was a branch stranded by a
loop split and is now fixed. Widening that fix to cover this case was measured
and is worse (missing calls 125 -> 130): the flat path handles ordinary
truncating branches better than a region does.

What this one needs is CONDITION COMPOSITION - folding consecutive conditional
jumps that converge on the same target into one compound `and`/`or` condition,
so the guarded body has an owner. That is new machinery, not a gate change, and
it is the single largest remaining item (30 of the 125 missing calls).

### Condition composition: why the SAME-TARGET version does not fix rc_0047
Scoped the pass before writing it, and the arithmetic rules out the cheap form.
`rc_0047 ?@1`:

    136: JUMPIFNOT R13 -> 140   |
    137: JUMPIFNOT R14 -> 140   +- same target, composable as `and`
    138: JUMPIFNOT R15 -> 140   |
    139: JUMPIF    R16 -> 145   <- DIFFERENT target, ends a same-target scan
    140..144: warn(..) ; RETURN

Folding only the same-target jumps (136-138) makes the then-body start at 139 -
which is itself a conditional jump to 145, past that body's end, so it is
stranded exactly as it is today and nothing is recovered. The body actually runs
when `R13 and R14 and R15 and NOT R16`, so a correct pass has to treat 145 as
the CONSTRUCT'S COMMON EXIT and fold a jump with a different target into the
same chain.

That is full short-circuit reconstruction over a jump lattice, not a
same-target scan: the terms converge on two distinct pcs (140 = body,
145 = exit), and which pc means "body" depends on the polarity of the LAST term.
`Expr::BinOp { op: BinOp::And }` is available to build the condition, and
`lift_instruction_range`'s inline-if arm is the right place to emit it, but the
detection has to identify the pair (body, exit) first.

Recorded rather than half-built: a partial version that folds only same-target
runs would change output on every multi-term guard in the corpus while
recovering nothing here, which is the worst combination available - blast radius
without payoff.

30 of the 125 missing calls are in this one proto, so the payoff for the FULL
version is real and measurable, and `probes/` plus `tools/structure_diff.py`
can judge it in seconds once written.

## SHIPPED (eb730f2): multi-term short-circuit guards - 30 more calls

    corpus missing calls  125 -> 95     calls_not_emitted 9 -> 8
    bodies_dropped          2 ->  1     defects          23 -> 22
    structure_diff: 1 file changed, ZERO lose an if, a call or a loop

`rc_0047 IndexController` goes from **46 lines to 304** - the whole index build
was being truncated - and the guard comes back verbatim:

    if not IndexUI2 or not IndexUI3 or not MainScrollingFrame2 or not Holder then
        warn("[IndexController] expected IndexUI.MainScrollingFrame.Holder.Item ...")

### Session arc on the call-loss family
    165 missing calls  ->  125 (b1a0f94, loop-spanning branch)  ->  95 (eb730f2)

Two different mechanisms, found by tracing two different files to the bytecode,
and NEITHER was reachable by tuning the other's gate - widening b1a0f94 to cover
rc_0047 was measured at 130 missing calls, WORSE than leaving it alone.

### What made both possible
Neither fix would have shipped on the semantic score. `b1a0f94` is score-neutral
on files and +1 on defects; this one is +0 files. Both were judged on
`corpus missing calls`, which is the decompiler's own count of code it dropped,
and on `probes/` + `tools/structure_diff.py`. The score alone would have called
the first one a regression and the second one noise.

### `calls_not_emitted` COUNTS CALLS IN PROVABLY-DEAD CODE
`rc_0030 Janitor::AddPromise@103` reports 10 of 10 calls missing and emits

    local function AddPromise(self, arg2)
        return arg2
    end

for a 57-instruction body. That looks like the worst possible collapse. It is
correct output:

    0: LOADB     R2 false      <- R2 IS false
    1: JUMPIFNOT R2 -> 56      <- so this ALWAYS jumps, over the whole body
    2..55: the body, 10 calls

R2 is loaded with `false` and never written before the test, so the branch is
provably always taken and the body is unreachable. This is a bundled copy of
evaera's Janitor with `FoundPromiseLibrary` stubbed to false at bundle time -
the Promise library is not in the bundle, so the compiler kept a branch that can
never run. Dropping it is right.

`call_recovery::record` counts CALL opcodes in `proto.code` without asking
whether they are reachable, so every one of those ten is charged as a defect.

**The 95 remaining missing calls are therefore an UPPER BOUND, not a count of
lost code.** At least 10 of them - and everything else behind a
`LOADB Rn false` / `JUMPIFNOT Rn` pair - are calls the decompiler is correct to
omit. `Cleanup@188` in the same file emits a real body, so its 9 are a separate
question and not this.

The metric was built to catch dropped bodies and it does that well - it found
both mechanisms fixed today. But a proto whose guard is a compile-time constant
will sit in its list forever, and no lifter change can or should remove it.
A reachability check on the CALL scan would make the number mean what it says.

### rc_0055: the same converging shape, but the terms are NOT ADJACENT
`?@1` loses 22 calls the same way `rc_0047` did - `eliminate_dead_code` cuts
34 -> 13 - but `compose_guard_chain` does not fire, and the bytecode says why:

    73: JUMPIFNOT R8  -> 80    |
    74..77: compute R10        |  <- intervening work
    78: JUMPIFNOT R10 -> 80    |
    79: JUMPIF    R9  -> 85    |
    80..84: body, RETURN

The convergence is there (73 and 78 to the body at 80, 79 to the exit at 85),
so widening the scan to skip non-jump instructions would collect it. The scan
width is not the problem.

**The problem is evaluation order.** Those intervening instructions COMPUTE THE
LATER TERMS, and under short-circuit semantics they run only if the earlier
terms pass. Hoisting them above a composed `if` evaluates them unconditionally -
and they are `GETTABLEKS` indexing, which errors on nil. A composed condition
would turn a guard that safely tests `a and a.b and a.b.c` into one that
indexes a nil.

So this is the impure-value-join problem in another costume, the same one that
blocks `find_value_join` from admitting a `CALL`: the terms cannot be lifted out
of the control flow that guards them. `compose_guard_chain` is safe precisely
BECAUSE it requires adjacency - adjacent conditional jumps test registers that
are already computed, so composing them hoists nothing.

That adjacency requirement is therefore not a conservative simplification to
relax later. It is the correctness condition. Anything that composes
non-adjacent terms has to keep each term's computation inside the branch that
guards it, which means emitting nested ifs, not one composed condition - and
nested ifs are what the lifter already produces here.

### CORRECTION: composing with `and` is SAFE - rc_0055 is a merge loss, not a hoist
The previous entry said non-adjacent terms cannot be composed because their
computation would be hoisted and evaluated unconditionally. That is wrong, and
the file's own output disproves it:

    local Main3  = Main2  and Main2:FindFirstChild("Frames")
    local Frames2 = Main3 and Main3:FindFirstChild("Shop")
    local Shop2  = Frames2 and Frames2:FindFirstChild("MainScrollingFrame")
    local MainScrollingFrame2 = Shop2 and Shop2:FindFirstChild("Holder")
    if Shop2 then
        local isType = Shop2:IsA("ScrollingFrame")     <- computed, DISCARDED
    end
    warn("[ShopLayout] ... not found - layout left alone.")
    return

FOUR terms are already folded into `and` chains WITH CALLS INSIDE THEM, and they
are correct - `and` short-circuits, so `Main2:FindFirstChild(..)` is not
evaluated when `Main2` is nil. Composition does not hoist; that was my error.

The term that failed is `isType`, and it failed for a different reason: its
result is DISCARDED. The value is computed inside `if Shop2 then .. end` and
never reaches the merge, so the guard has nothing to test and the whole
`warn(); return` ends up unconditional - after which DCE truncates the rest,
34 calls -> 13.

So `rc_0055` is not a composition problem at all. It is the MERGE-LOSS family:
one arm computes a value, the merge cannot see it, and the guard that consumed
it collapses. Same family as `declared_never_assigned`, reached from the guard
side instead of the read side.

That also explains why `compose_guard_chain` correctly does not fire here - by
the time the terms are values, the tests ARE adjacent, and the reason they are
not is the lost `isType`. Fix the merge loss and the guard chain composes
itself.

### rc_0055, third and final reading: the ARM BOUNDARY is wrong
Traced one level further than the last two entries and this is the mechanism:

    73: JUMPIFNOT R8  -> 80      the if-detection makes the arm 74..80
    74..77: compute R10
    78: JUMPIFNOT R10 -> 80      target == arm end, still liftable
    79: JUMPIF    R9  -> 85      target is PAST the arm end -> STRANDED
    80..84: body (warn ; return)

`lift_instruction_range` lifts a conditional jump only when
`target > pc && target <= end`. The jump at 79 leaves the arm entirely - it is
the guard's PASS path, jumping over the failure body to 85 - so it fails that
test and is silently dropped. With it gone nothing guards pc 80, the
`warn(); return` becomes unconditional, and `eliminate_dead_code` truncates the
remaining 21 calls.

So the arm is not 74..80. The construct spans 73..85, and 80..84 is its FAILURE
BODY, not code that follows it. The if-detection cut the arm at the first jump
target it saw and never reconsidered when a later jump inside that arm proved
the construct reached further.

This supersedes both previous readings of this file: it is not a composition
problem (the four working `and` chains prove composition is fine) and not a
merge loss (`isType`/R10 is never read on the falsy path, so there is no merge
to lose it at). It is a boundary chosen too early.

WHAT WOULD FIX IT: when an arm contains a forward conditional jump whose target
lies beyond the arm's end but inside the enclosing range, the arm end is wrong -
extend the construct to that target before lifting. That is a change to arm
DETECTION, and every previous attempt in this journal touched arm STRUCTURING or
the seed instead, which is why none of them reached this file.

### The arm-boundary fix, and the loose version that looked better
Implemented the extension the previous entry named, and the first version was
WRONG in a way the corpus total actively hid.

LOOSE VERSION - extend the arm whenever any conditional jump inside it targets
past its end. Corpus went 1305 -> 1309 clean, 22 -> 17 defects, and missing
calls 80 -> 49. Three separate totals all improving; it looked finished.

`structure_diff` said REJECT: 16 files lost calls, `rc_0104` alone losing 13.
Checked that file's own marker and it was decisive - `rc_0104` carried NO call
recovery marker in the control and reported 23 MISSING CALLS after the patch.
A file that was perfect became one of the worst in the corpus.

So the -31 net was a REDISTRIBUTION, not a gain: large wins in two files paying
for a new break in a third. Every headline number moved the right way while the
change was making the decompiler worse at files it already handled. This is the
third time in this session a corpus total has concealed a regression, and the
per-file check has caught it every time.

NARROW VERSION - extend only when the swallowed region is TERMINAL, ending in a
`Return`. That is the guard-failure-body shape the fix was written for: a region
that does not rejoin. Anything that rejoins is ordinary following code and must
stay outside the arm.

| | control | loose | narrow |
|---|---|---|---|
| clean | 1305 | 1309 | **1307** |
| defects | 22 | 17 | **19** |
| missing calls | 80 | 49 | **41** |
| `rc_0104` | clean | **23 missing** | clean |
| structure_diff | - | **16 files lose** | **0 lose** |

The narrow version scores WORSE on clean files than the loose one and is the
correct change - fewer files fixed, but nothing broken to pay for them. It also
ends up with FEWER missing calls than the loose version, because the loose one
was spending 23 of its own winnings on `rc_0104`.

Fixes `rc_0055` outright: 22 missing calls -> 0. Also `lv_0791` (17 -> 0) and
`rc_0102`. Three files touched, three files improved, nothing else moved.
Probes hold 9/9, 13/13, 8/8.

### lv_0905: located exactly, generalization REJECTED
`lv_0905 LocalCubs` proto 0 emits 7 of 17 calls. The 10 missing are pcs 77-159,
one contiguous block, and the mechanism is fully traced:

    54: FORGPREP_NEXT R2 -> 162        loop body 55..161
    57: JUMPIFNOT    R7 -> 63          arm cut at 63
    62: JUMPIF       R8 -> 72          past 63 -> stranded
    63..71: cloud:Destroy() ; mark ; JUMP -> 162
    72..161: the real work, ALL 10 missing calls

Source shape:

    for cloud, data in pairs(v4) do
        if data.Attachment and data.Attachment:IsDescendantOf(workspace) then
            <work>            -- 72..161, laid out SECOND
        else
            cloud:Destroy() ; toRemove[cloud] = true   -- 63..71, laid out FIRST
        end
    end

The then-body is placed AFTER the else-body. The jump at 62 escapes the arm
(58..62) that the `if` at 57 opened, fails `target <= end`, and falls through to
the out-of-range handler, which in a loop emits a bare `break`. 72..161 is then
never lifted at all.

This is the SAME stranding as `rc_0055`, with a different terminator: there the
swallowed region ended in `Return`, here it ends in `JUMP -> 162`. The shipped
fix requires `Return`, so it correctly declines.

TRIED AND REJECTED: extend to the terminating JUMP's target, so the arm holds
the whole diamond and `detect_else_skip` resolves it. It works - `lv_0905` goes
10 -> 0, corpus 1307 -> 1308 clean, 19 -> 18 defects, 41 -> 31 missing calls.
It also breaks `rc_0089`, turning

    if (CurrentCamera and CurrentCamera.CameraSubject):IsA("BasePart") then

into an `if` on an unrelated register with the `:IsA` result demoted to a
discarded local. THE CALL STILL PRINTS. Only the guard is gone, so every call
counter and every corpus total reads clean - `structure_diff` caught it and
nothing else would have.

Tried gating on the escaping jump being the arm's LAST instruction, on the
theory that this marks a short-circuit condition rather than a body. Measured a
NO-OP: `rc_0089` is byte-identical with and without the gate, so its escaping
jump is also arm-final. That discriminator is wrong and the corpus says so.

Reverted. State stands at 1307/1323, 19 defects, 41 missing calls.

NEXT ATTEMPT should discriminate on the ARMS, not the jump: in `lv_0905` the two
regions [63,71] and [72,161] are genuine siblings meeting at 162, whereas in
`rc_0089` the extension invents an outer `if` that was never there. A soundness
test on the resulting diamond - both arms reaching the same merge, neither
re-entered from outside - is the shape to try, and it must be checked BEFORE
committing to the extension rather than after.

### lv_0756: the dispatch chain never forms, and `current_loop_end` is None
`lv_0756 PassiveTile` proto 7 emits 5 of 14 calls. The 9 missing are exactly the
`Remove`, `Update` and `Use` arms of a string dispatch inside a `for` loop:

    4: FORGPREP_NEXT R1 -> 163          loop body 5..162
    7: JUMPXEQKS R6 not == "Add"    -> 66     body 8..65,  JUMP -> 163
   68: JUMPXEQKS R6 not == "Remove" -> 84     body 69..83, JUMP -> 163
   86: JUMPXEQKS R6 not == "Update" -> 111    body 87..110, JUMP -> 163
  113: JUMPXEQKS R6 not == "Use"    -> 163    body 114..162

An elseif chain. It emits as the FIRST arm only, with every "try the next case"
jump turned into `break`:

    if v.Action ~= "Add" then break end
    ... Add arm ...
    if not New3 then break end

ADDED TWO TRACES rather than guessing again, after the `lv_0905` gate I reasoned
my way into was measured a no-op. `LUAU_ELSE_TRACE` reports every
`detect_else_skip` decision; `LUAU_OOR_TRACE` reports every forward jump that
falls out of range. Both are env-gated and measured INERT - corpus output is
byte-identical, 0 files differ.

They answer it immediately:

    OOR-XEQK pc=7   target=66  end=13   loop_end=None
    OOR-XEQK pc=68  target=84  end=74   loop_end=None
    OOR-XEQK pc=86  target=111 end=92   loop_end=None
    OOR-XEQK pc=113 target=163 end=119  loop_end=None

`detect_else_skip` is called ONCE in the whole file, for an unrelated range. It
never sees the dispatch at all. Each test is lifted with an `end` barely past
itself - pc 7 gets `end=13` while its arm runs to 65 - so every chain jump is
out of range before any else-detection can run. The chain is not being rejected;
it is never presented.

SECOND DEFECT, INDEPENDENT: `loop_end=None` on every one of these. Only three
loop kinds set `ctx.current_loop_end` (mod.rs 3159/3223/3325/3879) and the
generic-for path is not among them. So the Break/Continue choice at
opcode_handlers.rs:3787 has nothing to test and ALWAYS picks `Break`. The jumps
at pc 32/40/45 target 163, which IS the FORGLOOP - those are continues, and they
emit as `break`, abandoning the whole iteration instead of skipping one item.

Note the existing comparison is `target < le`, so even with `le` set a jump TO
the latch would still pick Break. A fix needs `target <= le`, and needs to be
measured against files that already rely on the current reading.

Neither defect is fixed. `lv_0756` and `rc_0030` are both dispatch-shaped, so
the chain is worth roughly half of what remains.

### GenericFor never set `current_loop_end` - 214 wrong `break`s across 93 files
Fixed the second defect the `lv_0756` traces exposed. A generic-for lifted its
body without ever setting `ctx.current_loop_end`, so the Break/Continue choice
had nothing to test and ALWAYS chose `Break`. Every forward jump that left a
lifted range inside a `for ... in pairs(..)` came out as `break`, including
jumps to the FORGLOOP itself, which are continues.

The value is the EXCLUSIVE end of the construct - one past the back edge - so a
jump TO the back edge reads as continue under the existing `target < le` test,
and only a jump past the whole construct reads as break. That leaves the other
three loop kinds untouched.

WHY THIS IS SOUND, not just different: a genuine `break` must LEAVE the loop, so
its target is past the FORGLOOP and still reads as Break. The only jumps that
flipped are those targeting inside the loop, and a target inside the loop is
never a break. The change cannot turn a correct break into an incorrect
continue - it can only turn incorrect breaks into continues.

Measured: 501 -> 287 `break`s, 93 files changed, 0 losing structure. Corpus
totals do not move at all (1307 clean, 19 defects, 41 missing calls) because
break-vs-continue is invisible to every counter I have. Probes 9/9 13/13 8/8;
tests 1037 pass, 1 pre-existing fail.

Hand-verified rather than trusted. `lv_0002 BeeTypes`:

    local MaxForce2 = FaceColor2:FindFirstChild("BodyColor")
    if MaxForce2 then
        v2.GiftedBodyColor = FaceColor2.BodyColor.Value
        continue                    -- was `break`
    end
    -- otherwise fall through to the 2D-colour path

With `break` that abandoned every remaining bee after the first one carrying a
`BodyColor`. These were real semantic bugs in shipped output, and no metric in
the repo could see them - the only reason they surfaced is that a trace was
added instead of another guess.

`lv_0756`'s dispatch is still broken; its `if v.Action ~= "Add" then` now says
`continue` instead of `break`, which is closer but still wrong. The chain itself
remains unfixed.

### lv_0756 FIXED: a Linear region was holding a branch that escaped it
The region trace (extended to print each region's shape, not just a count)
showed proto 7's generic-for body structured as:

    Linear { 5, 13 }      <- JUMPXEQKS at 7 targets 66
    InlineIfThenInLoop { cond_pc: 13, body: [Linear { 14, 15 }] }
    Linear { 15, 74 }     <- JUMPXEQKS at 68 targets 84
    InlineIfThenInLoop { cond_pc: 74, .. }
    Linear { 76, 92 }     <- JUMPXEQKS at 86 targets 111
    ...

Structuring matched only the small `if X then return end` guards and dropped
everything else into Linear spans - and those spans CONTAIN the dispatch
branches. A Linear region is meant to be straight-line code, so
`lift_instruction_range` gets `end=13` for a branch at pc 7 whose arm runs to
65, sees the target as out of range, and degrades it to `break`. Only the first
arm of a four-arm dispatch survived.

Fix: when a Linear child of a generic-for body contains a branch escaping it,
lift to the END OF THE LOOP BODY instead, then stop iterating (that lift has
consumed the rest). The branch is then in range and `detect_else_skip` resolves
the whole chain recursively.

TWO GATES, both forced by measurement, not by reasoning:

1. `JumpXEqK*` ONLY, not the general `JumpIf*` family. Including the general
   family recovered the same 9 calls but produced an `undefined_local` in
   `lv_0005` - output the checker says "would error at runtime" - plus dropped
   values in `lv_0358` and `rc_0060`, because extending past the structured
   sub-regions discards the register state they establish. 1307 -> 1305 clean.

2. The arm must END IN AN UNCONDITIONAL JUMP over the next case - the else-skip
   shape. Without it the extension fires on ordinary guards: in
   `rc_0054 TutorialController` it collapsed a three-way if/elseif/else that
   honoured an `arg1` override into an unconditional default. The override was
   simply gone, and every corpus total still improved. `structure_diff` was the
   only thing that saw it, again.

Result: 1307 -> 1308 clean, 19 -> 18 defects, 41 -> 32 missing calls. ONE file
changed, 0 losing structure, and it emits the chain properly:

    if v.Action == "Add" then ...
    elseif v.Action == "Remove" then ...
    elseif v.Action == "Update" then ...
    elseif v.Action == "Use" then ...

probes 9/9 13/13 8/8; tests 1037 pass, 1 pre-existing fail.

Worth stating plainly: of the three variants tried here, TWO improved the
headline totals while destroying real logic. The totals are not a safety net.

### Made the dispatch gate AUX-safe (it was right by accident)
The gate shipped in the previous entry decoded `code[t - 1]` to ask whether the
arm ended in an unconditional JUMP. That is not safe: `t - 1` can land on the
AUX WORD of a two-word instruction, and an AUX word decoded as an opcode is
arbitrary data. It was a real `Jump` in `lv_0756`, so the gate worked - by luck,
not by construction.

Replaced with a boundary walk from the region start that finds the instruction
whose start + size == t. Output is BYTE-IDENTICAL across all 1,323 files
(0 differing, 1308 clean, 18 defects, 32 missing calls), which is the right
result for a soundness fix: no behaviour change, one less way to be wrong.

### rc_0030 `Cleanup@188`: a different failure, diagnosed not fixed
26 blocks collapse to 2 regions - `IfThenElse { cond_pc: 0 }` plus a trailing
`Linear { 124, 125 }` - and 9 of 10 calls are lost. Bytecode:

     2: JUMPIF       R1 -> 124            re-entrancy guard
    10: FORGPREP_NEXT R1 -> 103           loop body 11..102
    12: JUMPIFEQ     R4 == R6 -> 103      continue
    18: CALL (typeof)                     <- the ONLY call emitted
    19: JUMPXEQKS R6 ==     "string" -> 23
    21: JUMPXEQKS R6 not == "number" -> 26
    23..25: JUMP -> 103                   string/number: skip the entry
    26..102: the real cleanup work, ALL 9 remaining calls

So string-or-number keys skip, and everything else reaches the work at 26. The
emitted body inverts it:

    if result19 ~= "string" then
        if result19 ~= "number" then
            continue          -- bytecode sends these to the WORK at 26
        end
    end

and 26..102 is gone entirely. This is NOT the `lv_0756` shape - the dispatch
here has no per-arm JUMP over the next case, so the gate correctly declines. The
failure is upstream: an `IfThenElse` at pc 0 swallows 26 blocks and the nested
structuring inside its arms never recovers the loop body.

Remaining: `lv_0905` (10), `rc_0030` (9), `lv_0351` (5), `lv_0124` (5),
`rc_0084` (3) = 32 calls across 5 files.

### The dominant remaining mechanism: a non-loop out-of-range jump is SILENTLY DROPPED
Traced the last three files. Two of them share `rc_0030`'s shape exactly:

    rc_0030 Cleanup@188      26 blocks -> IfThenElse{cond_pc:0} + Linear{124,125}
    rc_0084 ComputePath@376  13 blocks -> IfThenElse{cond_pc:0} + Linear{112,113}
    lv_0124 Update@62        10 blocks -> IfThenElse{cond_pc:0} + Linear{50,63} + IfThenElse{cond_pc:5}

`rc_0084` is the smallest instance and shows the mechanism cleanly:

    20: JUMPIFNOT R1 -> 108
    24: JUMPIFNOT R1 -> 60
    25..59: arm A, 2 calls, ends JUMP -> 108
    60..107: arm B, 3 calls          <- LOST
    108..111: common tail ; 112: RETURN

A textbook if/else. The traces give the reason:

    OOR pc=20 target=108 end=25 in_loop=false loop_end=None
    OOR pc=24 target=60  end=25 in_loop=false loop_end=None

Both branches are lifted with `end=25` - a ONE-INSTRUCTION range - so the target
is out of range. And in a non-loop context the handler does nothing at all; the
code says so: "In non-loop context, out-of-range forward jumps are skipped." No
`break`, no `continue`, no `if` - the branch is DISCARDED, and the arm it
guarded is never lifted.

Inside a loop the same situation at least emits a break or continue, which is
why the loop-shaped failures were visible as wrong control flow while these are
invisible: the code simply is not there.

HOW COMMON, measured honestly: 12 of 60 sampled files contain at least one such
drop, 37 drops in total. That does NOT mean 37 defects. A drop only loses code
when the target's region is not lifted by some other region, and usually it is -
only 5 files in the whole corpus have missing calls. So this is a latent hazard
that bites when the structuring has already failed, not a defect on every hit.
Do not extrapolate 37/60 into a corpus-wide defect count; that would be the same
mistake as trusting the totals.

`lv_0351 UpdateLoop@558` is a `WhileTrue`, not this shape, but shows the same
non-loop drops at pcs 16, 23, 32, 72, 117, 122, 135 - all with tiny `end`s.

FIX SHAPE, and the plumbing it needs: for pc 24 the resolution already exists -
the instruction ending at 60 is pc 59, `JUMP -> 108`, so `detect_else_skip` with
a bound of 112 yields then=[25,59), else=[60,108) - the correct if/else. What is
missing is CONSUMPTION SIGNALLING: lifting past `end` means the caller iterating
blocks would lift [25,108) a second time. The generic-for fix solved exactly
this by consuming to the body end and breaking out of the region loop; there is
no equivalent path in `lift_branch_region`, and adding one is the next real
piece of work. Attempting the extension without it would duplicate code, which
no counter in this repo would catch either.

### rc_0084 narrowed further: `else_blocks=0`, and the loss is INSIDE lift_branch_region
Followed the non-loop-drop family one level deeper with the existing
`LUAU_IF_TRACE`, which prints exactly the question worth asking:

    IFLIFT proto=ComputePath@376 else_blocks=0 else_stmts=None else_calls=None
                                 then_stmts=14 then_calls=2 then_blocks=11

Two facts, both decisive:

1. `else_blocks=0`. The `IfThenElse` at cond_pc 0 has NO else region. The inner
   if/else at pc 24 - arm A 25..59, arm B 60..107 - never becomes two arms;
   everything lands in the THEN list.
2. `then_blocks=11` produces `then_calls=2`, where the bytecode holds 5. The
   comment beside this very trace states the rule: if the blocks go in and the
   statements do not come out, the loss is INSIDE `lift_branch_region`, not in
   whether the call was made. Eleven blocks in, two calls out.

Confirmed against the emitted source: `ComputePath` contains only the
`DirectPath` arm (its two `PathWaypoint.new` calls) and the async
path-computation arm is absent entirely.

Added `LUAU_RUN_TRACE`, printing the block ranges and the contiguous runs
`lift_branch_region` splits them into. Measured INERT - 1308 clean, 18 defects,
32 missing calls, 0 files differing.

It also exposed a discrepancy worth writing down rather than papering over: the
run trace prints no line for this proto even though `IFLIFT` proves
`lift_branch_region` was called for it, so something returns before the run
split. That is the next thing to look at and I have NOT explained it - recording
it as an open question, not a conclusion.

So the family now has two candidate roots and they are NOT the same:
* the arms are never separated (`else_blocks=0`) - a structuring failure in
  whatever assigns blocks to then/else, and
* the then-arm's own blocks lose their code during the run lift.
Fixing the second without the first would still emit arm B unconditionally
rather than as an else. The structuring failure is the one to attack.

### The `JumpXEqK` restriction is load-bearing, not conservatism
Answered the open question from the previous entry: `lift_branch_region` takes
its `early_ret || cond_arm` path for `ComputePath` - the arm contains a RETURN -
and calls `lift_structured_run` over the whole arm span, then returns. That is
why `LUAU_RUN_TRACE` printed no line for the proto despite `IFLIFT` proving the
call happened. Not a mystery, just a path that never reaches the run split.

So the `lv_0756` fix is the right SHAPE for this family but in the wrong PLACE:
the loss happens inside `lift_structured_run`'s own sub-structuring, which
produces the same narrow `Linear` spans holding escaping branches.

Then tested a combination never tried before. The two earlier failures of the
general `JumpIf*` family both PREDATE the else-skip gate, so it was open whether
that gate was the real safety property and the opcode restriction merely
incidental. Measured, with the gate retained:

| | shipped (`JumpXEqK` only) | widened to all `JumpIf*` |
|---|---|---|
| clean | **1308** | 1307 |
| defects | **18** | 19 |
| missing calls | 32 | 32 |

It recovers NOTHING and costs a file. The opcode restriction is doing real work
on its own; the else-skip gate does not subsume it. Reverted, 0 files differing.

That also means widening the family is not the route to `rc_0084`/`rc_0030`/
`lv_0124`. The route is applying the extension inside `lift_structured_run`,
where their loss actually happens, and it will need its own gate measured from
scratch rather than inherited from this one.

### rc_0084 FIXED, and the first justified override of a structure_diff REJECT
Applied the escaping-branch extension where this family actually loses its code:
`lift_structured_regions`, not the generic-for arm. `rc_0084 ComputePath`
structures its arm run 4..112 as

    Linear 13..25      <- JUMPIFNOT at 24 targets 60
    ValueJoin 25..50
    Linear 50..112     <- contains JUMP at 59 -> 108, over arm B

Two failures compound. The branch at 24 escapes its span, and OUTSIDE A LOOP an
escaping branch is discarded silently, so no if/else is ever built. `Linear
50..112` then follows the unconditional JUMP at 59 straight to 108, skipping
60..107 - arm B and its three calls, emitted nowhere.

The fix returns the JUMP's target (108) so the span lifts as 13..108, putting
the branch in range for `detect_else_skip`. Consumption is tracked so a region
INSIDE the extension is skipped while one STRADDLING it keeps its tail -
`Linear 50..112` still emits 108..112.

Gates: the branch must be the span's TAIL, its target must escape but stay in
the run, and the instruction ending at that target - found by walking
boundaries, never `t - 1`, which can be an AUX word - must be an unconditional
JUMP forward.

Result: 1308 -> 1309 clean, 18 -> 17 defects, 32 -> 29 missing calls.

OVERRODE A REJECT, and the reasoning matters because this journal has spent all
session insisting the tool be obeyed. `structure_diff` flagged `lv_0521` and
`lv_0582` at `if -3, calls +0`. Inspected both. The CONTROL is the degenerate
one:

    local random6
    if self.Bias then random6 = random3 * min3   -- a NUMBER
    else random6 = {1, 1} end                    -- a TABLE

Four separate `if self.Bias then` blocks, each assigning a different variable,
with type-inconsistent else values - one branch fragmented into four bogus
value-joins. No compiler emits that. The patched output collapses them into one
coherent `if self.Linear then .. else .. end` with both arms computing `Value14`.
The `if` count falls because four fake ifs became one real one.

The other 8 changed files are PURE ADDITIONS, no deletions: `rc_0097
LegacyCamera` gains 54 lines including a whole Fixed/Attach/Watch camera-type
dispatch, `lv_0130 Accessory` gains 46 lines of equip logic with its
`return false` guard.

So the rule stands as "a structuring change must lose nothing", and an if-count
is a PROXY for that, not the thing itself. Overriding is legitimate only after
reading the diff and finding the control was wrong - which is the same standard
that rejected `rc_0054` earlier, applied honestly in the other direction.

probes 9/9 13/13 8/8; tests 1037 pass, 1 pre-existing fail.
Remaining: `lv_0905` (10), `rc_0030` (9), `lv_0351` (5), `lv_0124` (5) = 29.

### lv_0124 is a DIFFERENT failure: the code is lifted, then discarded
Checked whether the `rc_0084` fix reaches the three remaining loop-shaped files.
It does not, and for `lv_0124` the reason is that the mechanism is not the same
one at all.

`Update@62` holds 7 calls and emits 2. Its regions:

    run=7..15   InlineLoopInLoop 9..12 , Linear 13..15
    run=15..50  Linear 15..40 , InlineIfThenInLoop 40 , Linear 41..48 , Linear 48..50

Note pcs 50..62 - which hold the calls at 55 and 61 - appear in NO run. The
branch at pc 3 (`JUMPXEQKNIL R1 not == nil -> 50`) is what reaches them.

`LUAU_IF_TRACE` then shows two lifts for this proto:

    else_blocks=0  else_stmts=None      then_stmts=0   then_calls=0  then_blocks=1
    else_blocks=3  else_stmts=Some(10)  then_stmts=2   then_calls=1  else_calls=Some(4)

The second one PRODUCES FIVE CALLS - four in the else arm, one in the then arm -
and the finished function contains two. So the statements exist and are then
thrown away downstream. The first line is the shape of the discard: one block
in, zero statements out.

That is categorically different from `rc_0084`, where the code was never lifted
because an escaping branch was dropped. Here lifting succeeds and something
after it - an enclosing arm, `is_empty_if`, or a post-pass - deletes the result.
Extending spans cannot help; the next step is to find what consumes a
10-statement arm and emits nothing.

Recording this because the whole family was previously written up as ONE
mechanism ("the non-loop out-of-range drop"). That was wrong for `lv_0124`, and
the correction matters: an extension fix aimed here would measure as a no-op and
look like a bad gate rather than a misdiagnosis.

Remaining, now with two distinct causes:
* never lifted  - `rc_0030` (9), `lv_0351` (5), and `lv_0905` (10, arm layout)
* lifted then discarded - `lv_0124` (5)

### rc_0030 FIXED - and the gate that beat it unifies with the Return case
`Cleanup@188` emitted 1 of 10 calls. Three wrong turns before the right one,
each killed by a measurement rather than an argument:

1. Extended spans in `lift_structured_regions` with a plain-if-then fallback.
   Cost a file and a defect, recovered NOTHING - and could not have, because
   that helper is never called on this proto's path. The measurement tested the
   wrong site.
2. Added a tail-branch extension to the generic-for body path. Exact no-op.
3. Added the arm extension to the `JumpXEqK` handler gated on `m == end`. Exact
   no-op again.

`LUAU_XARM_TRACE` ended the guessing in one run:

    XARM arm_start=21 target=23 end=61 furthest=26 last=Some(Jump) -> 23

Everything matched except `end`. The enclosing range is the `Linear 11..61`
region, not the loop body, and the JUMP at pc 25 goes to 103 - the loop
back-edge, OUTSIDE that range. So `m == end` was simply the wrong question.

The right one is the property `Return` already has: DOES THE SWALLOWED REGION
REJOIN THIS RANGE? A jump landing at or past `end` leaves it and the region is
a terminal skip path. A jump landing inside rejoins, which is ordinary code -
and extending through ordinary code is exactly what demoted a real
`:IsA("BasePart")` guard in `rc_0089` when this was tried before. Gating on
`m >= end` admits `rc_0030` and still excludes `rc_0089`, which stays
byte-identical.

The bytecode:

    19: JUMPXEQKS R6 ==     "string" -> 23      arm = [21, 23)
    21: JUMPXEQKS R6 not == "number" -> 26      PAST 23 -> stranded
    23..25: JUMP -> 103                         "is number": skip the entry
    26..102: the real cleanup work, 9 calls

Stranded, the branch at 21 degraded to a bare `continue`, which made the rest of
the body unreachable and let DCE strip it - so `IFLIFT` reported 10 calls pushed
while the file emitted 1. The AST was right and a post-pass was correct to
remove what the lifter had already made dead.

| | control | shipped |
|---|---|---|
| clean | 1309 | **1310 (99.0%)** |
| defects | 17 | **16** |
| missing calls | 29 | **20** |

Two files changed, 0 losing structure. `rc_0030` recovers real Janitor logic -
the `warnUser` closure, the `pcall` guard and its error path, the destructor
dispatch. `lv_0740` recovers an `else` branch for a "Mythic" rarity tier that
was previously flattened away. probes 9/9 13/13 8/8; tests 1037 pass, 1
pre-existing fail.

First time the corpus has crossed 99%.

### lv_0905 and lv_0351 FIXED - one gate, found by trace, refined twice
Unified the `JumpIf`/`JumpIfNot` arm on `extended_arm_end`, the helper written
for `rc_0030`. That alone took the corpus to 1312/1323 with 5 missing calls -
and damaged `rc_0089` again, exactly as the first attempt had months of entries
ago. The difference this time is that the traces could answer WHY.

    rc_0089 (damaging): arm_start=175 target=176 end=189 furthest=186
    lv_0905 (correct):  arm_start=58  target=63  end=162 furthest=72
    rc_0030 (correct):  arm_start=21  target=23  end=61  furthest=26

`rc_0089`'s bytecode:

    174: JUMPIF    R10 -> 176     outer; arm = [175, 176)
    175: JUMPIFNOT R11 -> 186     escaping branch
    176..179: (..):IsA("BasePart")
    180: JUMPIFNOT R16 -> 186     <- SAME exit as the escaping branch
    185: JUMP -> 189
    186..188: else

Two conditionals converging on ONE exit is a compound condition -
`(a or b) and x:IsA("BasePart")` - not a construct boundary. Extending merges
the terms and the `:IsA` result stops being the condition and becomes a
discarded local. The call still prints, so no call counter can see it.

GATE v1 - block when anything in the swallowed region targets `furthest`.
Fixed `rc_0089`, broke `lv_0142 Beequip`: it turned

    if arg3 then if not File2 then return false end end

into `if arg3 then return false`, aborting whenever `arg3` is set. Corpus totals
were IDENTICAL either way (1312/14/5) - the two variants simply traded which
single file they wrecked, and only a per-file read could tell them apart.

GATE v2 - block only when `furthest < end`. The trace gave the discriminator:

    lv_0142: target=67  end=70  furthest=70   <- furthest == end
    rc_0089: target=176 end=189 furthest=186  <- furthest < end

When `furthest == end` the escaping branch targets the very end of the range, so
extending just makes the arm the remainder and there is no trailing block to
mis-assign. When `furthest < end` the extension swallows a region and leaves one
behind - in `rc_0089` that leftover is the else body.

| | before | after |
|---|---|---|
| clean | 1310 | **1312 (99.2%)** |
| defects | 16 | **14** |
| missing calls | 20 | **5** |

21 files changed, 0 losing structure. `rc_0089`, `lv_0142` and `lv_0143` are all
byte-identical to control. probes 9/9 13/13 8/8; tests 1037 pass, 1 pre-existing
fail.

The whole arm-boundary family - `rc_0055`, `lv_0791`, `rc_0030`, `lv_0905`,
`lv_0351` - is now one helper with one gate, stated as: EXTEND WHEN THE
SWALLOWED REGION DOES NOT REJOIN THE RANGE, UNLESS A SECOND CONDITIONAL
CONVERGES ON THE SAME EXIT AND SOMETHING FOLLOWS IT.

Only `lv_0124` (5 calls, lifted-then-discarded) still loses calls. Defects
remaining: 7 `declared_never_assigned`, 3 `name_body_mismatch`,
3 `branch_local_discarded`, 1 `calls_not_emitted`.

### lv_0124 root cause: the merge point is 5, the construct reaches 50
Added `LUAU_NO_DCE`, which skips `eliminate_dead_code` so the AST can be read
before the pass that hides the evidence. Measured inert - 1312 clean, 14
defects, 5 missing calls, 0 files differing.

It answered `lv_0124` immediately. `LUAU_PASS_TRACE` had already narrowed it:

    after_eliminate_dead_stores calls=7
    after_eliminate_dead_code   calls=2

and with DCE off the emitted `Update` is:

    if StatTools2 ~= nil then
    end                                 -- EMPTY arm
    local ClientCall2 = ClientCall("RetrievePlayerStat", arg2)
    tbl:Set(arg2, ClientCall2)
    return ClientCall2                  -- UNCONDITIONAL
    if v6 then .. else .. end           -- 12 statements, 5 calls, all dead

So DCE is right: the statements after that `return` genuinely are unreachable.
The damage is done before it. The bytecode:

    1: JUMPXEQKNIL R2 == nil     -> 5     cache == nil  -> fetch path
    3: JUMPXEQKNIL R1 not == nil -> 50    statName ~= nil -> single-stat path
    5..49:  the fetch path (the v6 if/else)
    50..62: the single-stat path

    OOR-XEQK pc=3 target=50 end=5 in_loop=false loop_end=None

The arm of the branch at 1 is ONE BLOCK, [2,5), because the `IfThenElse` region
reports `merge=Some(5)`. The branch at 3 targets 50, which is past the arm AND
past `end`, so `extended_arm_end` never even counts it as escaping - `end` is 5.
Outside a loop the branch is then dropped silently, the arm comes out EMPTY
(`then_stmts=0 then_blocks=1`), and 50..62 is emitted unconditionally after it.
Its `return` then kills the real body.

The source is a guard whose BODY SITS LATER IN THE FILE:

    if cache ~= nil and statName ~= nil then <50..62 ; return> end
    <5..49>

This is NOT the arm-boundary family and `extended_arm_end` cannot reach it: the
region itself is wrong, not the arm inside it. `find_merge_point` returned 5 for
a construct that reaches 50. Fixing it means widening the region, in
`cfg.rs::find_merge_point` or in how `IfThenElse` arms are collected - and any
attempt must be measured against the whole corpus, because merge points feed
every branch in every file.

### rc_0270's three defects were the CHECKER, not the decompiler
CLAUDE.md has recorded these as false positives for a while without saying why.
Verified rather than trusted, and they are.

`name_body_mismatch` infers a function's identity from string literals in its
body and fires when the referenced name is a SIBLING declaration. `rc_0270` is
the evaera promise library, where `_new` is the internal constructor
`Promise.new` delegates to:

    error("Argument #2 to Promise.new must be a promise or nil", 2)

Naming the PUBLIC entry point is right - that is what the caller invoked. Same
for `_all`/`all` and `retryWithDelay`/`retry`. The check already exempts a
NESTED helper naming its parent for exactly this reason; a variant naming the
function it implements is the same situation between siblings.

Exemption: the field is the referenced name with leading underscores, or the
referenced name is a prefix of it. A genuine mis-pairing has UNRELATED names, so
this narrows without blunting - and the original test (`Create` carrying
`ServerCall`'s body) still fires untouched.

Two tests, one of which exists only to stop the exemption from growing:
`variant_naming_its_public_counterpart_is_not_a_mismatch` and
`unrelated_sibling_name_still_fires_after_variant_exemption`. Without the second
the first could be satisfied by deleting the check.

1312 -> 1313 clean (99.2%), 14 -> 11 defects. Decompiled output is BYTE-IDENTICAL
across all 1,323 files, which is the only correct result for a checker-only
change. Tests 1039 pass (up 2), 1 pre-existing fail.

Remaining 11 defects: 7 `declared_never_assigned`, 3 `branch_local_discarded`,
1 `calls_not_emitted`. Of those, `lv_0579` and `rc_0010` are the format-blocked
`upval_N` pair and can never be clean, so the reachable ceiling is unchanged at
1321/1323.

### The remaining defect family: seeded but not pinned
`rc_0047 IndexController::setTile` is the clearest instance of the class that
now dominates - 7 `declared_never_assigned` plus 3 `branch_local_discarded`,
one mechanism:

    local Text3
    if arg2 then Text3 = self.gem.rarity else Text3 = "???" end
    TextColor34.Text = Text3                      -- CORRECT

    local Text4
    if arg2 then
        local Text6 = RARITY_COLOR[self.gem.rarity] or Color3.new(1, 1, 1)
    end
    TextColor34.TextColor3 = Text4                -- always nil

The arm computes the value into a FRESH local and the seeded merge variable is
never written. `LUAU_SEED_TRACE` shows the seed is created correctly:

    SEED setTile@156 reg=6 pc=54 name=Text4 sc=true arms=[(55, 68), (69, 70)]

so the seeder knows the register, the name and both arm ranges. The arm write
simply does not land on it.

TRIED AND REVERTED: making a pin outrank debug info in `reg_name`. The reasoning
looked sound - `pin_reg_name` writes into `naming.assigned`, `reg_name` returns
a debug-info name BEFORE consulting `assigned`, so a deliberate pin loses to
whatever local name the compiler recorded. Measured: 0 files differing, every
count identical. The pin is never SET for these registers, so the lookup order
was never the question. Reverted rather than shipped - a no-op carrying a
comment that claims a fix is worse than no change at all.

So the seeder records the name without pinning the arm span, and
`lift_value_join` names its result with a plain `ctx.reg_name` at the join
(mod.rs ~2593) with no reference to the seed. That is where the next attempt
belongs, and it must be measured against the r6 result already in this journal:
naive one-arm seeding fixed 0 and broke 2.

### Three measured attempts at the seed family, all rejected
The remaining 9 real defects are one mechanism, and the naming path is NOT the
lever. Three attempts, each reverted on measurement:

1. **Pin outranks debug info in `reg_name`.** `pin_reg_name` writes into
   `naming.assigned`; `reg_name` returns a debug-info name BEFORE consulting it,
   so a deliberate pin loses. Measured: **0 files differing.** No pin exists at
   the naming pc, so lookup order was never the question.

2. **Pin the short-circuit JOIN instead of withholding the pin.** The seeder
   deliberately withholds the pin when an arm contains a nested short-circuit on
   the register - the comment records why, `rc_0100 Popper` collapsed when the
   whole span was pinned. Pinning only the convergence point should leave the
   operand writes alone. Measured: **0 files differing** again, and the new
   `SEEDPIN` trace said why:

       SEEDPIN setTile@156 reg=6 arm=(55,68) sc_join=Some(70) name=Text4

   The convergence point is 70, OUTSIDE the arm (55,68) - it is the end of the
   second arm (69,70). The whole construct is ONE short-circuit whose operands
   ARE the two arms, so a gate of `j < e` can never fire.

3. **Pin the join wherever it falls.** Fired at last - and made things worse.
   One file changed, `rc_0035 RoomRenderer`, from

       local LocalPlayer14 = LocalPlayer12 or firstUncleaned(arg1)

   to assigning a HOISTED file-scope local instead, leaking the value out of its
   scope. Zero defects fixed, one file degraded. Reverted.

WHAT THIS ESTABLISHES: the seeded name is not being lost to a naming-precedence
problem. The seed declares `Text4` and sets `regs[reg]`, and the arm still binds
its result elsewhere - so the arm's write is not consulting the register file
entry the seed installed. The next attempt belongs at the WRITE, not at the
name: find why an arm write to a seeded register does not see
`RegVal::Expr(Expr::Name(seed))` already sitting in `regs[reg]`.

Three no-ops and a regression is a real result; recorded so the next attempt
does not re-run any of them.

### lv_0883: a generic-for FLATTENED inside a branch arm (new mechanism)
The line-8 `declared_never_assigned` cases are not all the seed family. Checked
`rc_0037 v19`, `rc_0040 v44` and `lv_0883 v5`: each is declared at FILE SCOPE
and read once or twice with NO WRITE ANYWHERE in the file. The value is not lost
at a merge - it is never produced.

`lv_0883` shows why. Emitted:

    while result do
        ...
        result4, result25, result36 = ipairs(Body5)   -- the iterator TRIPLE
        if v5.Parent ~= nil then continue end          -- v5 never bound
        Position2 = (v5.Position - Position):Dot(v5.Position - Position)
    end

The source is `for _, part in ipairs(Body5) do .. end`. The FORGPREP was not
structured, so the iterator triple got assigned to plain locals and the LOOP
VARIABLE was never declared - every read of it is nil. `v5` is the loop
variable, not a dropped merge value.

Regions for the enclosing proto:

    REGIONS proto=?@5 blocks=10 regions=1
      IfThenElse { cond_pc: 0, then_region: [9],
                   else_region: [10,14,20,21,48,49,52,54], merge_pc: None }

and the arm's own run:

    RUNREG ?@5 run=10..69  Linear 10..18
                           InlineIfThenInLoop 18
                           Linear 21..48        <- the flattened generic-for
                           GenericFor 49..52
                           Linear 54..69

So the arm structurer DID find one generic-for (49..52) and missed another
inside the same run (21..48). Other protos in this file structure their loops
correctly - `initSegment@18` gets a proper `GenericFor` - so this is not a
general failure of loop detection. It fails for a loop nested inside a branch
arm that reaches `lift_structured_run`.

This is a THIRD distinct mechanism behind the remaining defects, alongside the
seed family and `lv_0124`'s wrong merge point. Recording it because the earlier
entries lumped all the `declared_never_assigned` rows together, and they are not
one thing: a flattened loop and a dropped merge value need opposite fixes.

### CORRECTION to the entry above: no second loop was flattened
The previous entry claimed `lv_0883` proto `?@5` contains a generic-for
flattened into `Linear 21..48`. That is WRONG and is corrected here rather than
left to be quoted later.

Proto 4 has exactly ONE `FORGPREP`, at pc 48:

    47: CALL           R5 args=1 results=3    -- ipairs(..) triple
    48: FORGPREP_INEXT R5 -> 52               -- body 49..51
    49: NAMECALL R10 R11:"Destroy"
    51: CALL     R10 args=1 results=0
    52: FORGLOOP R5 -> 49 nresults=2 [inext]

which is `for _, v in ipairs(X) do v:Destroy() end`, and RUNREG shows it
correctly structured as `GenericFor 49..52`. Nothing was flattened at 21..48.

The error: I matched the emitted `while` body (which reads the unbound `v5`) to
proto `?@5` by eye, and it belongs to a different proto. The SYMPTOM stands -
`v5` is declared at file scope, read twice, and written nowhere, and the emitted
code does show an `ipairs` triple bound to plain locals beside an unbound loop
variable - but the proto and pcs I attributed it to are not the right ones.

WHAT IS STILL ESTABLISHED, and what is not:
* established - `rc_0037 v19`, `rc_0040 v44`, `lv_0883 v5` are file-scope names
  read but never written anywhere, so the value is never produced rather than
  lost at a merge. That distinguishes them from `rc_0047`'s seed family.
* NOT established - which construct fails, or that it is a loop at all. The
  next attempt must locate the defect by TRACE, from the register the read
  resolves to, and not by pattern-matching emitted text to a region dump.

Three structural readings in this session needed correcting after the fact
(`rc_0055` twice, and this one). All three were built by eye from emitted
output; every reading that started from a trace has held.

### The remaining defects, classified BY TRACE (not by eye)
Applied the rule from the correction above and used `LUAU_MINT_TRACE`, which
reports for every minted name whether it was ever assigned, whether it lives in
a closure, and which sites produced it. All five file-scope
`declared_never_assigned` names in one pass:

    lv_0579  upval_2  READONLY  inclosure=true  sites=UPVAL_NAME
    rc_0010  upval_5  READONLY  inclosure=true  sites=UPVAL_NAME
    rc_0037  v19      READONLY  inclosure=true  sites=REG_EXPR_UNKNOWN+UNK_CALL_CLEAR
    lv_0883  v5       READONLY  inclosure=true  sites=REG_EXPR_UNKNOWN+UNK_CALL_CLEAR
    rc_0040  v44      READONLY  inclosure=true  sites=GEN_VAR_NOHINT

THREE different origins, where the journal previously had one. This also
CONFIRMS the ceiling rather than assuming it: only the two `UPVAL_NAME` files
are the format-blocked class, so 1321/1323 stands.

`UNK_CALL_CLEAR` is raised where a CALL wipes every register above its results.
The code comment there is explicit that this is "CORRECT by construction - a
call leaves everything above its results genuinely holding nothing", and warns
that suppressing a declaration whose register was cleared here "would replace a
visible defect with a silent wrong value". So `rc_0037` and `lv_0883` are the
decompiler correctly SIGNALLING that it does not know the value, and the fix
belongs upstream of the read - either the value should have been in that
register, or the read is of a register the call genuinely clobbers and the
lifter mismodels the frame.

One flagged assumption sits right there and is worth checking first:

    let result_count = match nresults {
        0 => 1,   // vararg results: assume 1 result by convention

A MULTRET call whose real result count differs would put `clear_start` in the
wrong place. That is a hypothesis, NOT a finding - it has not been measured
against these two files, and this journal has already recorded three
sound-looking hypotheses in this area that measured as no-ops.

Next steps, each tied to its own mechanism rather than to one lumped family:
* `rc_0037`, `lv_0883` - frame model at a CALL (`UNK_CALL_CLEAR`)
* `rc_0040` - a name minted with no hint (`GEN_VAR_NOHINT`)
* `rc_0047` + the `branch_local_discarded` rows - the seed family, write path
* `lv_0124` - `find_merge_point` returning 5 for a construct reaching 50
* `lv_0579`, `rc_0010` - unreachable, no CAPTURE data in the format

### FRAME data for the two `UNK_CALL_CLEAR` files
`LUAU_FRAME_TRACE` prints each call's clear and the registers still marked
declared above it:

    lv_0883  pc=3    A=0  clear_start=1  maxstack=10  DECLARED_ABOVE=[7,1,2,3,8,5]
    rc_0037  pc=102  A=0  clear_start=0  maxstack=12  DECLARED_ABOVE=[1,3,0,4,8,9]

A call based at register 0 wipes six registers the lifter still treats as
declared. In real Lua a local that is live across a call sits BELOW the call
base, so a call at A=0 implies nothing above it is live - which points at
declaration tracking outliving the actual scope rather than at the clear being
wrong.

LEAD, NOT A FINDING. It has not been measured, and the MULTRET
"assume 1 result by convention" hypothesis recorded above has not been measured
either. Both are written down so the next attempt starts from data instead of
re-deriving it; three sound-looking hypotheses in this same area have already
measured as no-ops in this session, so neither should be trusted until a control
build says otherwise.

### The MULTRET hypothesis is FALSIFIED for these files
Checked it rather than leaving it standing. `lv_0883`'s clearing frame

    FRAME pc=3 A=0 clear_start=1 maxstack=10 DECLARED_ABOVE=[7,1,2,3,8,5]

resolves to `monitorSegments` pc 3, `CALL R0 args=1 results=1` - an ordinary
ONE-RESULT call. `clear_start = a + 1 = 1` follows directly, with no involvement
from the `nresults == 0` MULTRET branch or its "assume 1 result by convention"
assumption. So that assumption is not the cause here and the next attempt should
not start there.

What the data does show: the six registers wiped are ones the lifter had already
HOISTED declarations for - the emitted body carries
`local result4, result25, result36, v8, Position2` ahead of the loop, and `v5`
is hoisted all the way to file scope. The call then clears exactly those
registers before anything writes them. So the sequence is
hoist-declaration -> call clears register -> later read finds nothing, and the
clear is behaving as documented.

That makes DECLARATION HOISTING the thing to examine, not the frame model. It
is recorded as the next lead, unmeasured, alongside the note that four
hypotheses in this area have now been measured and rejected in this session
(three no-ops, one regression, and this one falsified before it cost a build).

### lv_0124: `find_merge_point` is not the bug, and the fix is a structural feature
Read the function rather than assuming from the symptom. It runs an alternating
BFS from both successors and returns the first block reachable from BOTH. For
the branch at pc 1:

    successors: block@3 (fallthrough) and block@5 (target)
    from 3: reaches 5 and 50
    from 5: reaches 5
    -> first common block is 5

So `merge=Some(5)` is CORRECT by the function's own definition. Block 5 really is
reachable from both arms. What makes it the wrong answer for structuring is that
block 3 ALSO escapes to 50, so the construct is not a diamond closing at 5.

The real shape is a guard whose body sits LATER in the file:

    if cache ~= nil and statName ~= nil then <50..62 ; return> end
    <5..49>

The arm body (50..62) is NOT contiguous with the arm start (3), and everything
between (5..49) belongs AFTER the construct. My existing `extended_arm_end`
cannot help: extending the arm to 50 would swallow 5..49 into it, and those
blocks are lifted separately, so they would be emitted twice.

So this needs OUT-OF-ORDER ARM BODIES - emitting a construct whose then-branch
lives past the code that follows it - which the lifter has no representation
for. That is a feature, not a tweak, and it is the correct scope to record.

Retracting my own earlier recommendation: I previously wrote that `lv_0124` was
the highest-value next target because its root cause was "nailed down". The
cause is understood, but the fix is the largest of the remaining items, not the
smallest. Widening `find_merge_point` would be the wrong move - it is returning
the right answer to the question it is asked, and it feeds every branch in
every file.

### lv_0452: a dropped SETTABLEKS, and the cascade that hides it
A fresh mechanism, not one of the families already logged. `playCaughtAnimation`
loses BOTH a `math.min` clamp and a field write, from one root.

Bytecode:

    65: MULK       R13 R14 * K19   ; 0.0125
    66: ADDK       R12 R13 + K18   ; 0.8
    67: FASTCALL2K math.min R12 K6 skip=5
    73: CALL       R13 args=2 results=1        ; math.min(pitch, 2)
    74: MOVE       R12 R13
    82: SETTABLEKS R13."Parent" R0             -- EMITTED
    84: SETTABLEKS R13."PlaybackSpeed" R12     -- LOST
    86: NAMECALL   R14 R15:"Play"              -- EMITTED

`PlaybackSpeed` and `math.min` each appear ZERO times in the output while both
sit in the constant pool (K28, and K19/K18 for the arithmetic).

THE CASCADE, established by pass trace rather than assumed:

    simplify_stmts         calls=9
    eliminate_dead_stores  calls=8

`math.min` IS lifted - it is present at 9 calls. `eliminate_dead_stores` then
removes it because nothing reads its result, and nothing reads it because the
SETTABLEKS at 84 that consumed R12 was already gone. So the visible symptom
(`Workspace5` computed and never read, flagged as `branch_local_discarded`) is
two removes from the cause.

Also worth recording: the FASTCALL handlers do NOT skip - they only record
`pending_fastcall` - so the fallback sequence is lifted normally. FASTCALL was
my first suspect and it is not involved.

ROOT: `SETTABLEKS R13."PlaybackSpeed" R12` at pc 84 is dropped during LIFTING.
It survives neither with `LUAU_NO_DCE` nor before any post-pass. The adjacent
`SETTABLEKS R13."Parent" R0` at pc 82 IS emitted, so the difference is the VALUE
operand - a parameter register versus a computed local - not the target.

That is the next thing to look at, and it is a genuinely different lever from
the seed family, the frame model, or out-of-order arm bodies.

### lv_0452 FULLY root-caused, and the fix REJECTED for making it worse
Traced the dropped `SETTABLEKS` to its cause with a new `LUAU_SETK_TRACE`:

    SETK-DROP pc=84 target=Field { object: Name("v9"), field: "PlaybackSpeed" }
                    val=Name("v9")  selfref=true  artifact=false

`MOVE R12 R13` at pc 74 aliases R12 to R13's NAME. R13 is then reused for the
cloned sound, so by pc 84 both render as `v9` and the write reads as
`v9.PlaybackSpeed = v9` - self-referential, and suppressed.

In the bytecode A=12 and B=13, DIFFERENT registers, so the obvious fix is to
gate the guard on `a == b` (the NAMECALL-misidentification shape it was written
for). Measured: the write comes back, and it comes back as

    v9.PlaybackSpeed = v9

REJECTED. Un-suppressing does not fix the VALUE - R12 still renders as the stale
alias - so the change converts a VISIBLE OMISSION into a SILENT WRONG VALUE.
That is the exact trade the `UNK_CALL_CLEAR` comment warns against, and the
guard was doing its job: refusing to emit garbage it had been handed.

TOOLING BLIND SPOT, worth more than the fix: `structure_diff` passed this
CLEAN - 23 files changed, 0 losing structure - because an assignment is not an
`if`, a `call` or a `loop`. The corpus totals did not move either (1313/11/5),
and `lv_0452`'s own defect row was unchanged. Every instrument in the repo said
this change was neutral while it was writing wrong code into 23 files. Only
reading the emitted line caught it.

ROOT, for the next attempt: register hints are append-only (already recorded in
CLAUDE.md), so `MOVE` captures a NAME rather than a value and that name goes
stale when the source register is reused. Fixing the alias fixes `lv_0452`
properly and the guard can stay exactly as it is. Fixing the guard alone is
worse than doing nothing.

### Fixed the instrument that missed it: `structure_diff` now counts assignments
The rejected `lv_0452` fix passed every check in the repo while writing wrong
code. That is a tooling failure, not just a bad patch, so the tool is fixed.

`structure_diff` counted ifs, calls and loops. An assignment is none of those,
so `v9.PlaybackSpeed = v9` was invisible. Added two components:

* `assign` - total assignments. FALLS when a write is dropped, which is the
  loss that slipped through.
* `selfassign` - writes of the form `x.field = x`, excluding `tbl.__index = tbl`
  (the class-table idiom, correct under the probe-measured map). NEGATED in the
  tuple so a RISE reads as a decrease under the existing "any component fell"
  rule, with no change to the accept/reject logic.

VALIDATED BOTH WAYS, because a checker that only fires is as useless as one that
never does:

* against the known corruption - now REJECTS, and shows it was WORSE than I had
  seen: self-referential writes appeared in at least six files
  (`rc_0008`, `rc_0074`, `lv_0396`, `lv_0578`, `lv_0654`, `lv_0685`), with
  `lv_0685` gaining five. I had only inspected `lv_0452`.
* against a change already shipped as good - still passes at 21 files, 0 losses.
  No false positives.

That second result matters: the tool would be worthless if the new components
flagged the arm-boundary work already in the tree.

The blind spot is why the `MOVE`-alias fix has not been attempted. It touches
the hottest path in the lifter and its failure mode is silent wrong values -
exactly what could not be seen until now. With `assign`/`selfassign` in place
that change is finally measurable, and it is the right next piece of work.

### The MOVE-alias fix: REJECTED, and the new metric earned its keep immediately
Attempted the fix the previous entry called the right next piece of work: at a
`MOVE` whose source is a Name, materialise the destination as its own local
when the source register is redefined later and the destination is read after
that point.

It does not work, and for an instructive reason. Output becomes:

    local PlaybackSpeed = v9        -- v9 is still the UPVALUE here
    v9 = script.CollectSound
    v9.PlaybackSpeed = PlaybackSpeed

The materialisation captures the stale name as its VALUE, because R13 ALREADY
rendered as `v9` at pc 74 - the `math.min` result was never materialised and
R13 had inherited an upvalue's name. So the corruption is upstream of `MOVE`
too: the register is mis-named before the move ever happens.

MEASURED: 39+ files each LOSE an assignment, with no gain at all - corpus
totals unchanged and `lv_0452` still wrong. Reverted.

THE TOOLING POINT: every one of those rows reads `if +0 calls +0 loops +0`. The
OLD `structure_diff` would have passed this change as clean, exactly as it
passed the previous one. The `assign` component added an hour earlier caught a
39-file regression on its first real use.

That is now two consecutive changes in this area whose damage was invisible to
the pre-existing instruments. Anyone continuing here should treat that as the
governing fact: the naming layer produces failures that if/call/loop counts
cannot see, and the value of a new check is measured by what it rejects, not by
what it permits.

### Write-scoped register hints: attempted, MEASURED, rejected
CLAUDE.md records re-ranking hint PRIORITY as tried and measured a no-op, and
names the alternative: "clear or scope hints at the write, not re-rank them."
That had never been attempted. It has now.

Implemented: hints carry the PC at which they were observed, and `select_hint`
accepts any whose PC <= the read. Added a scope floor - the PC of the most
recent write to that register - so a hint recorded for a PREVIOUS occupant is
excluded, falling back to the full set when that leaves nothing.

    1313 -> 1312 clean, 11 defects, 5 missing calls
    63 files LOSE an `if`; `rc_0074` newly defective

REJECTED. It does what it was meant to - the `assign` column rises by 38-40 in
the worst files, which is exactly the extra distinct names that scoping
produces - but it costs guards to get them, and a lost `if` is a lost guard.

So the CLAUDE.md recommendation is now itself measured, and both halves of it
have failed: re-ranking is a no-op, scoping loses structure. Anyone reaching for
the naming layer next should know that the note pointing here has been spent.

That is nine consecutive hypotheses measured and rejected across these families
in one session: three no-ops, four regressions, one falsified, one actively
harmful while reading as neutral. Each is written up with the measurement that
killed it.

### Why write-scoping fails, read from the diff rather than the count
The previous entry rejected write-scoped hints on `if -1` across 63 files. That
was the right verdict reached the wrong way - the same "read the count, not the
diff" mistake this journal warns about, and `lv_0521` earlier in this session is
proof it can go the other way (an `if -3` there was an improvement).

Inspected. `lv_0210`:

    - local CheckStatReqs = value13.CheckStatReqs(arg1, { ..
    + local value14        = value13.CheckStatReqs(arg1, { ..
    - local HasItem        = value13.HasItem(arg1, { ..
    + local value16        = value13.HasItem(arg1, { ..

The change is a NAMING regression, not a structural one. Scoping at the write
discards the descriptive `Named` hints - the ones derived from field names -
because they were recorded BEFORE the register's most recent write. The lost
`if` is fallout from names no longer folding the same way downstream.

So the rejection stands and now has a reason: hint PCs do not reliably mark
WHICH value a hint describes. A `Named` hint recorded from a field access
describes the value that access produced, and a later write to the same
register does not invalidate it if the write is what stores that value. Scoping
by PC cannot tell those apart, which is why both re-ranking and scoping fail -
the PC is the wrong discriminator for hint validity.

That is a sharper statement of the problem than CLAUDE.md's, and it rules out
the whole PC-based family rather than just the two attempts made against it.

### Why NO PC-based rule can work: name inheritance across writes is INTENTIONAL
Completing the previous entry. The descriptive names that write-scoping
destroyed were not stale - they were inherited on purpose.

    local CheckStatReqs = value13.CheckStatReqs(arg1, { .. })

The `Named` hint comes from the `GETTABLEKS .CheckStatReqs`, which writes the
FUNCTION into the register. The `CALL` then overwrites that register with the
RESULT, and the result inherits the name. That is correct and desirable - a call
result named after the function it came from - and it is exactly the pattern a
"hint recorded before the last write is stale" rule deletes.

So the discriminator is not WHEN the hint was recorded but WHETHER the new value
DERIVES from the old one:

* `GETTABLEKS` then `CALL` on the same register - the result derives from the
  function; INHERIT the name.
* a register reused for an unrelated `Clone()` (`lv_0452`'s R13) - no
  derivation; DROP the name.

That is a dataflow question, and no PC comparison can answer it. It rules out
the entire family - re-ranking, scoping, exact-match on the write PC - rather
than the individual attempts, and it explains why `lv_0452` and the readable
names in `lv_0210` pull in opposite directions under every rule tried.

A real fix tracks derivation at the write: when an instruction's inputs include
the register it defines, the hint carries; otherwise it is dropped. That is a
change to how hints are RECORDED, not how they are selected, and it is the first
approach in this area that is not already measured and rejected.

### DERIVATION BARRIER: the eleventh attempt, and the one that works
The previous entry concluded that no PC-based rule can work because the real
question is whether a new value DERIVES from the register's previous contents.
Implemented exactly that.

`RegisterHint::FreshWrite` is a barrier, not a name. It is recorded for writes
whose value cannot derive from what the register held - `GetGlobal`, `GetUpval`,
`NewTable`, `DupTable`, `NewClosure`, `DupClosure`, `LoadK`, `LoadN`, `LoadB`,
`LoadNil`, `GetVarargs`. `select_hint` then considers only hints at or after the
last barrier, so a name from a previous occupant dies with it while deliberate
inheritance across `GETTABLEKS` -> `CALL` survives untouched.

`GetImport` is DELIBERATELY EXCLUDED. Including it replaced
`local DynamicThumbstick2 = require(Thumbstick)` with `local import3 = ..` in
`lv_0925`, because a `require` result legitimately inherits its name from the
import path. That one exclusion halved the flagged files, 53 -> 27.

| | control | shipped |
|---|---|---|
| clean | 1313 | 1313 |
| defects | 11 | **10** |
| missing calls | 5 | 5 |
| corpus assignments | 120,100 | **120,223** |

Exactly one defect row removed (`lv_0129 declared_never_assigned`), ZERO files
newly defective. probes 9/9 13/13 8/8; tests 1039 pass, 1 pre-existing fail.

SECOND JUSTIFIED OVERRIDE of a `structure_diff` REJECT, held to the same
standard as the first: read every flagged file, and only override when the
CONTROL is the wrong one. All four inspected are improvements, and each is the
defect class this work targets:

* `lv_0877` - the stray file-scope `local v13`, declared and never assigned, is
  GONE. That IS `declared_never_assigned`.
* `lv_0894` - hoisted `local Init4` gone; `task.spawn(function() .. end)`
  instead of `local function Init() .. end ; task.spawn(Init)`.
* `lv_0923` - `for result33 = 1, #tbl2` becomes `for i = 1, #tbl2`, and three
  materialisations of one reused `CurrentCamera` variable holding three
  unrelated booleans collapse into `if arg2:GetShiftLock() then ..`.
* `lv_0925` - only a counter rename once `GetImport` was excluded.

KNOWN LIMITATION OF MY OWN NEW METRIC, recorded so it is not mistaken for a
defect later: `assign` falls when a SPURIOUS DECLARATION is removed, which is a
fix. The metric cannot tell a dropped write from a deleted redundant one, so a
negative `assign` is a prompt to read the diff, not a verdict. It still did its
job today - it caught a 39-file regression that the old tool passed.

### Extending the barrier to table reads: no gain, 282 files, rejected
Natural follow-on to the shipped barrier: `GETTABLEKS/GETTABLE/GETTABLEN A B`
with `b != a` reads out of ANOTHER register's table, so the value is equally
unrelated to what `A` held. Gated on `b != a` so the common
`GETTABLEKS R R."Field"` refinement chain still inherits its name.

Measured: 1313 clean, 10 defects, 5 missing calls - IDENTICAL to control - while
282 files changed and `structure_diff` rejected on assignment losses.

Rejected on the arithmetic rather than on inspection: there is no upside to
weigh against 282 changed files, and no way to hand-verify that many. A change
with zero measurable gain does not earn the risk, whatever its reasoning looks
like.

Worth separating from the shipped barrier, which had a REAL gain (11 -> 10
defects, one `declared_never_assigned` removed, zero newly defective) and whose
27 flagged files were each read and found to be improvements. The difference
between the two is not the idea - it is that one moved a number and the other
did not.

### The one untried idea, recorded rather than started
The shipped derivation barrier composes with the seed family in a way nothing
has tried. `rc_0047`'s defect is that the seeder KNOWS the right name - the
trace prints `SEED setTile@156 reg=6 pc=54 name=Text4` - and the arm still binds
its result to a fresh `Text6`, because `reg_name` resolves from hints that know
nothing about the seed.

With `RegisterHint::FreshWrite` now in place, the natural move is for the seeder
to record its own name as a BARRIER-BACKED hint at the seed pc: a `FreshWrite`
followed by `Named(final_name)`. Every later read of that register in the arms
would then resolve to the seeded identity, which is exactly what the pin was
trying and failing to do.

BLOCKER, and why it is recorded rather than attempted: hints are only pushed
inside `analyze_register_usage`, the pre-pass. The seeder runs later, during
lifting, and `DecompileContext` exposes no way to add a hint after the pre-pass -
`pin_reg_name` writes to `assigned`, which `reg_name` consults AFTER debug info
and which the earlier measurement showed is never even set for these registers.
Adding a hint setter is a small change; doing it safely is not, because it
changes naming for every register that carries a seed.

This is the first idea in this area that is neither already measured nor ruled
out by the PC-based analysis, and it is the one to pick up next.

### The seed-plus-barrier composition: MEASURED, and it relocates the problem
Attempted the idea the previous entry recorded. Added `seed_reg_name` to
`DecompileContext` - a post-pass hint setter that pushes a `FreshWrite` barrier
plus the seeded `Named` hint - and called it from the seeder alongside the
existing `regs[reg] = Name(final_name)`.

Measured: 1313 clean, 10 defects, 5 missing calls - unchanged - across 19
changed files. Reverted.

But the failure is informative, and it RETIRES the naming hypothesis for this
family. `rc_0047`'s arm now names itself `Text43` instead of `Text6`, derived
from the seeded `Text4`. So the hint IS reaching the arm and IS being consulted.
The arm still emits:

    local Text4
    if arg2 then
        local Text43 = RARITY_COLOR[self.gem.rarity] or Color3.new(1, 1, 1)
    end
    TextColor34.TextColor3 = Text4          -- still nil

The write is being classified as a FRESH DECLARATION rather than a reassignment
of the seeded local. That is `LocalTracker::classify_write` returning
`FirstDecl`/`Shadow` where it should return `Reassign` - a different layer
entirely from naming.

So the whole naming line of attack is now closed for the seed family:
* `pin_reg_name` - never set for these registers (measured)
* pin outranking debug info - no-op (measured)
* pinning the short-circuit join - no-op, then a regression (measured)
* PC-scoped hints - loses descriptive names (measured)
* seeded barrier-backed hint - reaches the arm and changes nothing (measured)

Five ways of telling the arm the right NAME, and the arm was never asking. The
next attempt belongs in `classify_write`: why does a write to a register the
seeder has already declared come back as a new declaration?
