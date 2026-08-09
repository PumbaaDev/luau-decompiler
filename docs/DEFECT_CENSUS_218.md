# Defect Census 218 — what remains after the measured map

**Corpus:** 628-file Bee Swarm dump (`target/bc`), decompiled at commit `a3713db`
(measured opmap DB entry `roblox_v9_live`, 77 opcodes, batch mode).
**Baseline reproduced in a clean worktree before any analysis:**
compile gate **628/628 (100.0%)**, CoreScript truth **9/9**, lib tests **903 pass**,
semantic **492/628 clean (78.3%), 218 defects**.
**Cross-checked on the 642-file live corpus:** 642/642 compile, **500/642 clean
(77.9%), 230 defects** — same class mix (declared_never_assigned 228/141 files,
plus the same two singleton files), so nothing below is corpus-tuned.

This census supersedes the *counts* in `docs/DEFECT_CENSUS.md` (which measured
the pre-measured-map world of 1040) and reuses its method: every OURS /
GAME-TRUTH verdict below is backed by reading the bytecode, not the output.

## Method note — a trap found while measuring

`disassemble --opmap` consults only the pooled `--opmap-cache` store, **never
the opmap database**. With no store present it silently falls back to solo
detection, printed a 55-opcode partial map for files the batch had decoded with
the 77-opcode measured entry, and produced incoherent listings (back-to-back
CALLs, `MODK R0 R0 % "require"`, jumps to +1539). Every bytecode exhibit below
therefore comes from a census-only dump (`tests/census_disasm.rs`) that applies
the same DB entry verbatim via `OpcodeMap::from_exact_map`, i.e. the decode the
lifter actually saw. Do not cite `disassemble --opmap` output while the store is
empty.

## The two-sentence answer

The 218 count is **one defect family wearing two check names plus one false
positive**: 217 of 218 (135 of 136 flagged files) are values the bytecode
provably computes whose **binding is lost on the way out** — either a register
written across if/else arms whose arms the structurer deleted or demoted, or a
captured upvalue whose child-slot name was never linked to the parent binding.
The single remaining `name_body_mismatch` (436_Stickers) is **GAME TRUTH**: the
raw bytecode's proto 21 is debug-named `inspectSF` and its own constant table
carries the `"Stickers.RepairInvalidStickersInBook: …"` warn strings, so the
game itself ships a nested helper that logs its parent's name — the check
tripped on a convention, not a mispairing.

## Classes ranked by files moved

| # | Check | Defects | Files | Verdict |
|---|-------|--------:|------:|---------|
| 1 | `declared_never_assigned` | 215 findings (= **259 names** after the 6-per-file reporting cap; two files hide 43 and 3 more) | 134 | **OURS** |
| 2 | `undefined_local` | 2 | 2 (427_PlaceInfo, 180_NPC — 180 also carries class 1) | **OURS**, same family |
| 3 | `name_body_mismatch` | 1 | 1 (436_Stickers) | **GAME TRUTH — checker false positive** |

`bodies_dropped`, `discarded_table_write`, `property_called_as_method` fired
zero times (discarded_table_write was 7/4 files pre-map; the map cured it).

Unique flagged files: 136. Ours: **135**. False positive: **1**.

## Class 1+2 — lost bindings (217 defects, 135 files, OURS)

One umbrella root cause — *a value the bytecode computes is read in the output
under a name that is never bound* — with **two proven mechanisms**. The names
are decompiler-minted (`vN` 179/213, the rest renamed by the naming pass:
`result3`, `Obj`, `SwayAnimId3`…), declared by `free_var_decls` at chunk top
(the checker sees the bare `local` and the reads).

### Mechanism 1a — branch-arm writes deleted or demoted (in-proto)

A register is written in several if/else arms and read after the merge; the
arm bodies are deleted outright or their writes demoted to fresh dead locals.
Bytecode exhibits (all from the exact-map dump):

* **95_TaskTypes_Make_Honey `v2`** — GetStat proto 3: `LOADNIL R2` then a
  three-way diamond writes R2 (`ORK R2 …` at 11, 17, 23 off
  `Totals.ChallengeHoney/FarmedHoney/Honey`), consumed by `SUB R2 R2-R3` at 32.
  Output deletes the entire diamond (instrs 0–23 have no trace) and reads R2 as
  `v2`. The surviving arm write is demoted to `local Honey = …` inside the if,
  then `return Honey` reads it out of scope — two defects, one deletion.
* **180_NPC `v18`** (undefined_local) — main proto: `LOADNIL R18`, three arm
  writes `LOADK R18 "Press …/Tap …/Click to continue"` (instrs 123/142/144),
  consumed by `SETTABLEKS R19."Text" R18` at 147. Output deletes two arms and
  the surviving write, emits `NPC.CommandTag.Text = v18`. Same file, same
  verdict on the live corpus (309_NPC).
* **427_PlaceInfo `v7`** (undefined_local) — the compiler *inlined*
  `getServerType()` at the call site (DUPCLOSURE at 121 followed by the body
  copy, instrs 122–138, writing R7 in all three paths; `JUMPXEQKS R7 ==
  "StandardServer"` at 139). Output keeps the first inlined statement, drops
  the rest, reads R7 as `v7`.

This is the same family as commit `f7cc859` ("if/else arms no longer delete
the for-loops they own") — arm content marked handled by
`structure_control_flow` (`crates/luau-core/src/analysis/structuring.rs:154`)
and then not re-examined by the lifter's arm walk
(`crates/luau-core/src/decompiler/lifter/mod.rs`); the for-loop case was fixed,
the plain-statement/diamond case was not. The demotion half (assignment turned
into a fresh `local`) is the lifter's declare-vs-assign decision losing the
live-out register identity across arms.

### Mechanism 1b — capture chain not linked (upvalues)

The parent provably computes and even *emits* the value; the child closure
reads it via GETUPVAL and the lifter mints a fresh unbound name instead of the
parent's binding. Bytecode exhibits:

* **538_RingPulse `v1,v3,v4,v5`** — `NEWCLOSURE R24 P0` + `CAPTURE val
  20,16,15,12,10,18,6,17` (U0–U7); OnUpdate reads U1–U6. The captured parent
  registers are all named, emitted locals in the output
  (`Transparency`, `GoalOpacity`, `Size`, `Pos`, `Position`…), yet the child
  reads arrive as `v3,v4,v1,v5`. Two more escapees in the same closure
  (`Radius22`, `Pos2`) have the identical defect but dodge both checks — the
  count understates this file.
* **279_JellyBeanToss `result3`** — ref-capture: `LOADNIL R12` … `CAPTURE ref
  12` (U2); child does `GETUPVAL R3 U2; NAMECALL Disconnect`. The parent-side
  store of the `RenderStepped:Connect(...)` result into R12 is emitted as a
  *fresh* local `RenderStepped` instead of the captured `result3` — the classic
  connection-variable split. (Same file's `v3` is a 1a case: `SUB R3 R2-R1`
  feeding `.Magnitude`, SUB dropped — the two mechanisms co-occur.)
* **313_Beesmas2025Quests** (49 names, worst file) — repeated per-quest
  cutscene closures ref-capturing config slots (`v12 = nil` parent-side
  re-inits visible in output); capture family at scale in one file.

Code: child-slot naming happens inside the NEWCLOSURE/DUPCLOSURE handler off
the live register file (`crates/luau-core/src/decompiler/lifter/opcode_handlers.rs`
~1799–2050, GETUPVAL at 733–780, `upval_parent_links` two-phase at ~1920–1935).
The untracked `lifter/capture_chain.rs` (unlanded Phase C10Y) is a static
pre-pass targeting exactly these residuals; its header enumerates the same
failure conditions this census measured. Caveat before landing it: RingPulse
shows the child must adopt the parent's *final emitted* local name (post
naming-pass), and JellyBeanToss shows the parent-side write must target the
captured slot — name seeding alone fixes neither half unless it wins those
races.

### How the 259 names split (automated + sampled)

Per-name resolution of each read to its enclosing proto (`_classify2.tsv`
in the worktree; enclosing-function → proto debug-name match):

| Signal | Names | Files | Reading |
|---|---:|---:|---|
| Enclosing proto has **zero** GETUPVAL (capture story impossible) | 58 | 53 | mechanism 1a certain |
| Reads at declaration scope (flat) | 4 | 4 | 1a |
| Enclosing proto reads upvalues (1b *possible*) | 128 | 81 | mixed — sampling found real 1a inside this bucket too (411_CentralTime's `v2,v3` are conditional year/month diamonds) |
| Unresolved (anonymous/renamed enclosing fn) | 27 | 19 | unknown split |

Pure-composition by file: 40 files carry only impossible-capture names,
67 only possible-capture names, 11 only unresolved, 16 mixed. So **neither
mechanism alone covers the class**: a branch-merge fix moves ≥40 files
(likely ~55–70 with its share of the mixed/possible buckets), a capture-link
fix moves ≤67 files on its own. Every one of the 9 names hand-traced to
bytecode (5 files) had its defining writes present — none was game truth — and
the structural argument generalises: these names are minted by *our* lifter
for registers *it* read; the game stripped its names, so no flagged name can be
a source-carried variable. The theoretical game-truth shape (a source local
that is genuinely read-while-always-nil, i.e. LOADNIL with no later store
before the reads) was not found in any sampled file; I did not automate a
corpus-wide per-name LOADNIL proof, so per-name residual risk is stated, not
zero — but it cannot be large, because free_var_decls only declares names whose
binding *we* failed to emit.

## Class 3 — `name_body_mismatch` (1 defect, 1 file, GAME TRUTH)

`436_Stickers` (and `145_Stickers` on live): `inspectSF` is a `local function`
**nested inside** `tbl.RepairInvalidStickersInBook`, and warns with the
enclosing API function's name — the ordinary helper-logging convention.
Bytecode proof: proto 21 debug_name `inspectSF`, own constants K3/K7 = the two
`"Stickers.RepairInvalidStickersInBook: …"` strings. There is no SETTABLEKS
mispairing; the output is faithful.

**Narrowing that fixes the FP without blinding the check:** skip the finding
when the referenced sibling declaration's line range **contains** the flagging
declaration (nested helper naming an ancestor). The Events.lua defect this
check exists for pairs *disjoint sibling* closures (`Create` carrying
`ServerCall`'s body at the same table level), which containment cannot mask.
I could not construct a mis-pairing that containment would hide: for the
ancestor to contain the helper, the closure must genuinely be lexically inside
it, which is the honest case by definition.

## Reachability and ceiling — the honest answer

* **Checker-visible ceiling:** 100% is structurally reachable: 135 files are
  ours (one root-cause family, two mechanisms, both in the lifter/structurer),
  and the 136th is a one-line-narrowable checker FP. Nothing in the 218 is
  unfixable game truth.
* **Realistically:** these are the two hardest remaining lifter problems —
  cross-arm live-out register merging (needs the arm walk to emit arm bodies
  and keep one identity for the merged register instead of declare-demoting),
  and capture linking (needs child slots bound to the parent's final emitted
  names, plus parent-side writes to ref-captured slots redirected). Expect the
  branch-arm fix to land first (it is the f7cc859 family finished properly) at
  roughly +40–70 files (85–89%); capture linking is larger surface (C10Y
  pre-pass exists unlanded) for most of the rest. Both must hold the compile
  gate at 628/628 — the GETUPVAL handler comment records how the last guard
  here broke 7 files.
* **The true ceiling is below what the checker will report.** RingPulse alone
  carries two escapee names (`Radius22`, `Pos2`) with the identical defect and
  no flag, and the old census measured ~176 escapees corpus-wide. When the
  visible 218 reach 0, the output is not yet honest — the checker's `vN`-only
  scope and the 6-per-file cap both undercount. Treat 100%-clean as a gate to
  re-run this census with a widened escapee scan, not as done.

## Not established

* The exact 1a/1b split inside the 128-name "possible-capture" bucket (needs
  per-name GETUPVAL-site tracing; sampled only).
* A corpus-wide per-name LOADNIL-liveness proof that no flagged name is a
  genuine always-nil source local (sampled 9/9 ours; structural argument
  above).
* Which exact code path deletes vs demotes an arm body (both symptoms observed;
  single stepping the structurer per exemplar was out of scope for a census).

## Reproduction

```powershell
$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
cargo build --release -p luau-cli
.\target\release\luau-decompiler.exe batch <corpus>   # prints the semantic line
$env:OUR_CORPUS = "<corpus>\decompiled\run_<ts>"
cargo test -p luau-core --release --test census_dump -- --nocapture    # TSV of all 218
$env:CENSUS_BINS="<corpus>"; $env:CENSUS_OUT="<dir>"; $env:CENSUS_DB="opmap_db\roblox_v9_live.json"
cargo test -p luau-core --release --test census_disasm -- --nocapture  # exact-map disasm
```
