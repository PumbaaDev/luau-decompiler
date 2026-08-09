# Defect Census — what the 1040 semantic defects actually are

**Corpus:** 628-file Bee Swarm dump (`target/bc`), decompiled with the current
binary at commit `077bec1` (pooled opmap, batch mode).
**Baseline reproduced:** compile gate **627/628** (only `511_TriangleMaker`
fails, `repeat`/`until` structuring), CoreScript truth **9/9**, lib tests
**903 pass**, semantic **298/628 clean (47.5%), 1040 defects**.

> **Status note (verification pass, 9 Aug 2026):** this census measures the tree
> at `077bec1`. Commit `42fc00a` (measured opcode map replaces the guessed one)
> subsequently moved the same corpus to **491/628 clean, 222 defects** — i.e.
> ~4/5 of the counted defects (mostly the capture-chain class below) were cured
> by measuring the map, which is the silent mis-mapping split described in
> docs/GENERATED_NAME_ORIGINS.md. The remaining 222 have not been re-censused.
> The class analysis, the OURS/GAME-TRUTH method, and the empty-body findings
> remain valid; the counts and the attack-order sizing describe the pre-42fc00a
> world. Nothing below has been edited.

This is a measurement, not a fix. Every OURS / GAME-TRUTH label below is backed
by the bytecode itself — instruction counts from `info --json`, opcode decode
from `disassemble --opmap` (bracket = canonical opcode in hex, e.g. `[0x0D]`=13
GETTABLE), and raw string tables. Where the bytecode carried the data, an empty
or nil output is OURS; where the proto is genuinely one `RETURN`, it is truth.

---

## The two-sentence answer

Almost the entire count is **one defect wearing two check-names**: a value the
lifter dropped, rendered either as a top-level `local vN` that is never assigned
(`declared_never_assigned`, 1001) or as a `vN` with no declaration at all
(`undefined_local`, 31). Together they are **~1032 of 1040 defects across 326 of
the 330 broken files**, and reading the bytecode shows the dropped values are
real — overwhelmingly captured upvalues (the results of `require(...)` that
closures capture) whose binding was never emitted. **No sizeable class in the
1040 is game truth.** The game truth lives *outside* the count, in a class the
checker deliberately cannot see (empty function bodies), and there it is the
majority.

---

## Classes ranked by size (the counted 1040)

| # | Check | Defects | Files | Spread | Verdict |
|---|-------|--------:|------:|--------|---------|
| 1 | `declared_never_assigned` | 1001 (+141 hidden = ~1102 names) | 327 | thin, 2–3/file | **OURS** |
| 2 | `undefined_local` | 31 | 19 | thin | **OURS** |
| 3 | `discarded_table_write` | 7 | 4 | clustered | **OURS** |
| 4 | `name_body_mismatch` | 1 | 1 | single | **OURS** |

`bodies_dropped` and `property_called_as_method` fired **zero** times — the
CameraModule-style `return {}` whole-file loss and `script:Parent()` method/property
confusion are not present in this corpus at the checks' thresholds.

### 1. `declared_never_assigned` — 1001 defects, 327 files — OURS

The dominant class, spread thin (avg ~3 names/file, 2680 read-sites total). A
name is declared by a bare top-level `local vN` and read but never written, so
every read is nil.

**Root cause, confirmed OURS by bytecode.**
`105_..._Donate_To_Wind_Shrine.lua` declares `local v2, v3, v4`; the closures
read them. In the bytecode's main proto:
```
9:  CALL R0  = require(EggTypes)        -- R0 is 'value'
18: CALL R1  = require(NumberCommas)    -- R1 is 'value2'
20: DUPCLOSURE R3 proto0 ; CAPTURE val 0   -> Validate captures R0
28: DUPCLOSURE R3 proto2 ; CAPTURE val 0,1 -> Description captures R0 and R1
```
The values provably exist (they are `require` results); the closures capture
them; the lifter emitted the upvalue *slots* (`v2,v3,v4`) but never bound them
to R0/R1. Same shape in `103_CubSkins` (`v1.Model:Clone()` — v1 is a dropped
captured table; 26 GETUPVAL/DUPCLOSURE/CAPTURE ops in that file). This is the
GETUPVAL / capture-chain gap, not game truth.

The check is sound here: a bare `local` never on any LHS, read ≥1×, is nil at
every read; verified by hand on `CubSkins:v1` (never assigned, read 5×).

### 2. `undefined_local` — 31 defects, 19 files — OURS

The **same** dropped-value bug, rendered differently: where `free_var_decls`
declared the name at chunk top you get class 1; where it did not, the `vN` is
read with no declaration at all. Only decompiler-invented names (`vN`,`upval_N`)
are flagged, so by construction these cannot be game globals.

### 3. `discarded_table_write` — 7 defects, 4 files — OURS

`GlitchFX`, `LocalFlames`, `ParticleTools`, `RoboBearGui`. A table built in a
loop and never read — a SETTABLE whose target register was lost. In
`LocalFlames` the output is literally `local tbl2 = {} - i` inside `for i=1,12`:
a table minus an integer, mangled loop-body arithmetic. Clustered in FX/particle
files with dense loop bodies.

### 4. `name_body_mismatch` — 1 defect, 1 file — OURS

`436_Stickers`: `inspectSF` carries a body identifying itself as
`RepairInvalidStickersInBook`. This is the dangerous Events.lua class (a closure
paired with the wrong SETTABLEKS key) — now down to a single instance.

---

## The game truth is OUTSIDE the 1040

The suspects in the brief are about **dropped bodies**, and the semantic checker
is blind to them (an empty function is syntactically valid). Scanning the output
for empty named-function bodies and looking each proto up in the bytecode:

| Empty `function X() … end` bodies | 165 |
|---|---:|
| **GENUINE no-op** (proto = 1–2 instrs, just RETURN) → **GAME TRUTH** | **124** |
| **DROPPED** (proto 4–94 instrs, body emptied) → **OURS** | **27** |
| name not matched to a named proto | 14 |

**This is the monster-Loot lesson again.** 124 of the 165 empty bodies are
genuinely empty in the client — `StatModifiers/*:Effect()`, `DigitalBeeQuests:
ReloadServer()` etc. are one-instruction `RETURN` protos. Flagging them would
manufacture 124 false positives, which is exactly why the checker is right not
to. The remaining **27 are ours**: real bodies discarded, proven by instruction
count — e.g. `DigitalBeeQuests:InitServer` (94 instrs, references ServerStorage,
body empty) and the GetStat class below.

Distinct files carrying ≥1 proven dropped body: **23** (17 already caught by a
class-1/3 defect, **6 currently counted false-clean**). So the honest clean
figure is ~6 below 298, not wildly inflated — the checker's conservatism holds.

### Brief's three named suspects, checked against bytecode

- **~10 TaskType `GetStat` bodies emptied → CONFIRMED, OURS (9).** Nine
  `TaskTypes/*` files emit `function GetStat(self, arg2) end`. `Match_Pairs`
  GetStat is 6 instrs and the bytecode carries `Totals`, `MemoryMatchPairs`; a
  mid-body instruction decodes as an out-of-range `JUMPX -> 770` in a
  6-instruction proto, the lifter can't place it, and drops the statements.
  `Playtime` GetStat is 20 instrs, same empty output. Data present → ours.
- **`CollectibleListener` tag-merge loop emptied → CONFIRMED loop dropped,
  OURS — but thinner than assumed.** The bytecode loop body between
  `FORGPREP`/`FORGLOOP` is a single `SUB R7 R2-R6`, not a rich merge; the output
  drops it (`for k,v in pairs(arg1) do end`). Real, ours, one instruction.
- **`Inc[n]` where `Inc*n` meant → HYPOTHESIS NOT SUPPORTED.** In `Honey_Bee`
  the canonical opcode is genuinely `GETTABLE` (13, well-supported in the pool),
  and `Inc`'s constant `K23` is a `{2 keys}` table, so `self.Inc[count]` is the
  *correct* operation — not a mangled multiply. There is a separate real
  inconsistency (output renders `Inc = 500` scalar yet indexes it as a table),
  but it is small (4 RepeatQuests/StatModifiers files) and sits in
  constant/table-constructor emission; its mechanism is **UNDETERMINED** without
  decoding the nested constant, and I did not chase it into the emitter.

### Two more checker-invisible classes (measured, not in the 1040)

- **Empty `for`-loop bodies: 53 across 30 files.** A source-level empty loop is
  a near-nonexistent construct; `CollectibleListener` proves at least one drops
  a real instruction. Very likely OURS; I did not verify all 53 proto ranges, so
  the class total is **OURS (sampled), remainder UNDETERMINED**.
- **Renamed undeclared reads ("escapees"): ~176 names / 93 files (heuristic).**
  Same dropped-value family as class 1 but the temp was renamed (`Item2`,
  `Combo6`) so `undefined_local` (which only knows `vN`) misses it. E.g.
  `Donate:Description` reads `Item2` (a dropped `self.Amount` temp) never bound.
  **Likely OURS, lower confidence** — heuristic may include a few false hits
  (`arg2` in `ControlScript` is probably a real param it failed to bind).

---

## Recommended attack order (justified by files moved)

1. **The capture-chain / dropped-value binding (classes 1+2).** One root cause,
   **326 of 330 broken files.** 306 files carry *only* `declared_never_assigned`,
   so binding captured upvalues to their source registers would, on the checker's
   view, move up to ~325 files clean — 47.5% → potentially ~99%. This is the
   whole ballgame; everything else is a rounding error by comparison. Caveat:
   some of those files also hide a dropped-body defect the checker can't see, so
   the *true* clean gain is smaller than the checker will report — watch the
   compile gate and the empty-body count, not just the clean %.
2. **Dropped real bodies (27 functions / 23 files, incl. 9 GetStat).** The
   checker-invisible OURS core. A `JUMPX`-to-nowhere misdecode empties short
   protos; worth a targeted look at the opcode that produces the bogus long jump.
   Fixing it also removes ~6 false-cleans and improves files already counted
   broken. Second because it is fewer files and partly opcode-map work.
3. **`discarded_table_write` (4 files) and `name_body_mismatch` (1 file).**
   Small, but class 4 is the highest-severity shape (wrong body on a real name);
   fix when touching the SETTABLEKS/closure pairing that also underlies class 1.

**Do NOT** attempt the `Inc = 500` constant question or the empty-`for` remainder
as decompiler fixes yet — both are UNDETERMINED here and the first overlaps the
constant/BinOp emission path another agent is working.

## Classes I could not fully classify

- The `Inc`-scalar-vs-table inconsistency (4 files) — real defect, mechanism
  undetermined without nested-constant decode.
- The empty-`for`-loop remainder beyond the one proven case (up to 52 loops).
- The escapee heuristic's exact OURS count (~176 names) — high-likelihood ours,
  but not individually bytecode-verified.
