# luau-decompiler — working notes

A Luau bytecode decompiler, disassembler and obfuscator in Rust. Fully offline.
Read by Claude Code and by the local model, so work can continue on either.
Keep it short: it loads into every session.

## Layout

| Crate | Purpose |
|---|---|
| `crates/luau-core` | Parser, lifter, emitter, opmap. Nearly all real logic. |
| `crates/luau-cli` | The `luau-decompiler` binary |
| `crates/luau-compiler` | `luau-protect` obfuscator |
| `crates/luau-server` | HTTP server |
| `crates/luau-wasm` | WASM build |

67 test files under `crates/**/tests/`.

## Build and test

```sh
cargo build --release          # binary at target/release/luau-decompiler
cargo test                     # full suite
cargo test -p luau-core        # core only
```

Kill any running `luau-decompiler.exe` / `Decompiler.exe` before rebuilding on
Windows, or the link step fails on a locked file.

## Rules that matter here

- **Force a clean decompiler run when measuring quality.** Empty the output dir
  first and confirm `resumed=0`. Otherwise the numbers mix old-opmap files and
  are meaningless.
- **`disassemble` needs `--opmap` AND `--opmap-db`** or its output is wrong.
  `--opmap` alone runs the detectors and prints a map the decompile never used;
  it misassigns the AUX-carrying opcodes, so the listing eats AUX words as
  instructions and desynchronises. It looks plausible and is not. Fixed in
  4ca4286, but pass both flags — the DB is what makes it exact.
- **Never read operands off the disassembler's printer** — it invents names like
  `R{a+1}`. Go to the bytecode.
- Tag a backup (`git tag backup/pre-<phase>`) before lifter/opmap/emitter edits.

## THE fix list (this is the actual work)

### 1 SEP 2026 — 10→8 DEFECTS (99.4%). EVERYTHING BELOW THIS IS OLDER

Fresh clean `batch target/union1324` with the current binary: **1315/1323 clean (99.4%), 8
defects** — the 22-Aug "22 defects" block below was already stale (real number was 10).
Two lifter fixes landed (commit `e531307`), measured with a strict ratchet:

- **DUPTABLE cross-branch write** (fixed lv_0129): DupTable now calls `table_needs_binding`
  like NewTable, so a cloned `{...}` template crossing a branch materialises instead of
  parking pending and losing a later cross-branch field write at the merge.
- **Upvalue-alias name collision** (fixed lv_0452): `LocalTracker` now tracks `alias_regs`
  (a GETUPVAL read of upvalue `vN`); `classify_write` mints a fresh name for a real write to
  an alias register instead of donating `vN` and colliding with the real upvalue.

**BOTH the unit suite AND the corpus are mandatory gates — neither alone is enough.** Three
fixes were REVERTED, each failing exactly one gate: `find_merge_point` zero-successor guard
(fixed lv_0919 in the corpus, **broke 7 merge unit tests** — two arms that both `return`
share a terminal block as their real merge); `collect_loop_body` dead-end inclusion (**951/0
tests but regressed lv_0943 + rc_0274**, net +1); ternary leaf-cap widening (**no-op**). Gate
every change on `cargo test -p luau-core --release --lib` == 951/0 AND a full `batch` diff
(defects down, NO previously-clean file regressed). Backup tag `backup/pre-defect-fixes-0901`.

**Remaining 8 are at the ceiling:** 2 unfixable `upval_N` (lv_0579, rc_0010 — no CAPTURE data)
+ 6 in the merge/phi/arm/loop core (lv_0124, lv_0883, lv_0919, rc_0037, rc_0047, rc_0040) that
the three reverts show resists safe change. Per-defect root-cause traces exist (8-agent workflow).



### MEASURE ON `target/union1324` — 22 Aug 2026, EVERYTHING BELOW IS OLDER

**The corpus is 1,324 unique files, not the ~15,000 the directory listing
suggests.** Hash-deduped every `.bin` across 20 dirs: `bc/`, `target/full`,
`vz`, `rf`, `fresh`, `live`, `r*lv`, `r*bc`, `p1run`..`p4run` add **ZERO** new
files — every one is a strict SUBSET of `target/live_20260820`. Only
`target/roomclean_139191806727638` adds anything: **179 files never measured
before** (Roblox CoreScripts, PlayerModule, topbarplus/Janitor/evaera-promise).
Built as `target/union1324` with `rc_`/`lv_`/`fz_` provenance prefixes.

Those 179 are **10x worse** than the tuned corpus (10.1% of files defective vs
1.05%), which is what overfitting to one corpus looks like when it is finally
measured.

```sh
./target/release/luau-decompiler batch target/union1324   --out-dir target/<fresh> --opmap-db opmap_db/roblox_v9_live.json
```

| measured 22 Aug on union1324 | |
|---|---|
| decodable | 1,323 / 1,323, 0 failed, 1 not a program |
| semantic | **1,305 / 1,323 clean (98.6%), 22 defects** — corpus missing calls 95 (was 165) |
| of which FALSE POSITIVES | 3 defects, all in ONE file (`rc_0270`) → true **1,298/1,323 = 98.1%** |
| **CEILING** | **1,321/1,323 = 99.85%** — `lv_0579` and `rc_0010` fail only on `upval_N`, and Roblox bytecode carries NO CAPTURE data, so those two can never be clean. **100% is not reachable.** The real gap is **23 files**. |
| outputs that parse | 1,323 / 1,323 |
| tests | 1,035 pass |

**`batch` now writes `SEMANTIC_REPORT.tsv`** into the run dir: one row per
defect with FILE and LINE. Use it. Every defect ledger in this project that
lacked file-level evidence went stale and was then quoted as current.

**DO NOT enable `LUAU_BRANCH_ARMS`.** Its own comment records "clean 1,100 →
1,104, defects 48 → 39, files losing clean status: 0". That measurement is
STALE. Measured today: **1,044/1,323 (78.9%), 639 defects** — it destroys a
fifth of the corpus. Other gated flags: `LUAU_ARM_COND`/`LUAU_ARM_IF` +2 files
but REGRESS one (net +1, not shipped); `LUAU_RET_RESUME` neutral;
`LUAU_DECL_HOIST` worse. A comment recording a measurement is not a
measurement; re-run it against a control build.

**The CLI and the library produce DIFFERENT output.** `bundled_db_path()` makes
the CLI silently load `opmap_db/roblox_v9_live.json`; `luau_core::decompile()`
does NOT, so `luau-wasm` and `luau-server` get the weaker detector-only path.
Measured on `16_StickerPlacer`: CLI correct, library emits the artifact.

**A red test can be asserting a FALSE invariant.** `settablekS_self_assign`
claimed "a real SETTABLEKS never has A == B" and was red forever.
`SETTABLEKS R0."__index" R0` under the PROBE-MEASURED map is A == B and is
correct — `tbl.__index = tbl`. "Fixing" the detector to satisfy it would have
stopped every class table decoding. Check the premise against ground truth
before treating red as a defect.

### REMEASURED 22 Aug 2026 — THE NUMBERS BELOW THIS BLOCK ARE STALE

The list further down is dated 19 Aug and was swept against an older opmap
source. Measured today against a clean `batch` of all 1,143 inputs with the
CURRENT binary (`1142 ok, 0 failed, 1 not a program, 82s — 1142/1142
decodable`), using deterministic counters rather than a model sweep:

| Defect | 19 Aug sweep | measured 22 Aug | how measured |
|---|---|---|---|
| `undefined_register` | 171 | **9** (in 8 files) | a `local vN` declared, used, never assigned |
| `name_collision` | 388 | 26,919 suffixed locals of 44,804 (60.1%) | `local Name<digits> =` |

**`undefined_register` is ~91% gone and nobody recorded it.** The 18 Aug run
had 104 unassigned vars across 25 files; today it is 9 across 8. The cause is
the probe-aligned opmap DB (`roblox_v9_live`, 77 opcodes measured against the
client's own compiler), not any lifter change. `Utils_RefCounter` — the file
`tests/opmap_dupclosure_detection.rs` documents as emitting seven methods all
assigned to an unbound `v1` — now decompiles to seven real functions with zero
unresolved markers.

**So the two red tests are not tracking a live defect.** `cargo test` shows
965 pass / 2 fail; both failures are in `opmap_dupclosure_detection`, which
exercises the DETECTOR-FALLBACK path. The shipping binary reports `opmap
evidence: not applicable; no detector or bijection completion contributed to
this decode` — it resolves from the DB and never runs those detectors. The
tests still guard the fallback; they just do not describe normal operation.

**`name_collision` is the remaining real class, and it is READABILITY, not
correctness** — `local Exists4 = tbl.Exists; local Exists5 = Exists4(self)` is
valid Lua that reads badly. Root cause located: register hints in
`decompiler/mod.rs` are APPEND-ONLY and never cleared when a register is
overwritten, so a dead name outlives its value. **Reordering hint PRIORITY is
not the fix** — tried on 22 Aug in `select_hint` (prefer the newest hint over a
stale `Named`) and measured a NO-OP: patched and unpatched binaries produce
byte-identical output on the sample file and identical corpus totals
(26,919 / 9 both ways). Reverted; `backup/pre-naming-fix` holds it. The next
attempt should clear or scope hints at the write, not re-rank them.

**Measuring rules learned the hard way, 22 Aug:**
* Compare against an UNPATCHED CONTROL BUILD, never against an old run dir. A
  stale baseline made one no-op change look first like a win, then like a 3.2pt
  regression. Both readings were wrong.
* `cargo build` leaves the OLD binary in place until it finishes — a
  measurement taken right after a revert can still be running the patched code.
* `batch` DELETES old run directories (83 in one run). A baseline stored under
  the default run folder is destroyed by the run measuring against it; the
  19 Aug reference survived only because it sits under `run_verify/`.
* `OPMAP_TRACE` is inert for integration tests — `#[cfg(test)]` in the library
  does not activate for anything under `tests/`.
* `pgrep` cannot see Windows processes from Git Bash, so `until ! pgrep ...`
  exits instantly and yields a confident "0 files" measurement. Use `tasklist`.



REBASED 19 Aug 2026 ONTO THE REAL CORPUS. The list below used to be built from
`bc/`, which holds **628** files - roughly half of `target/full1143`'s 1,143,
and skewed: 575 ReplicatedStorage against **1 Workspace script out of 224**.
`617_PromoCodes` is not in `bc/` at all. Every proportion it implied was wrong.

COMPLETED 19 Aug 2026 - the first sweep covered 1,111 of 1,142 because a 60KB
cap skipped the 31 LARGEST files. That is a biased 31, not a random one, and it
moved the answer. `sweep_corpus_merged.jsonl` is the whole corpus, with the 8
files too big for one context pass swept in 110KB chunks and their line numbers
mapped back to the originals.

| Kind | whole corpus | ordinary files | the 31 biggest |
|---|---|---|---|
| `name_collision` | **388 (47%)** | 388 (54%) | 0 |
| `undefined_register` | **289 (35%)** | 171 (24%) | **118 (97%)** |
| `dead_store` | 71 | 68 | 3 |
| `dropped_call` | 39 | 39 | 0 |
| `truncated_body` | 38 | 38 | 0 |
| `duplicate_call` | 9 | 8 | 1 |

**THE TWO FAMILIES SORT BY FILE SIZE.** `name_collision` is absent from the big
files entirely; `undefined_register` is 97% of what is in them and 24% of
everything else. So "start with X" is the wrong shape of advice: pick the family
by the file you are holding. 834 defects total, and the densest single file is
`623_Beesmas2024BQuests` at 35 - which the 60KB cap had hidden completely.
`sweep_full1143.jsonl` — 1,111 files, **712 defects with exact line
citations**: 246 broken, 79 suspect, 781 clean, 5 error.

| Kind | Count | was, on `bc/` |
|---|---|---|
| `name_collision` — one debug name reused for unrelated values | **388 (54%)** | 177 |
| `undefined_register` — bare `vN` used, never assigned | 171 (24%) | 368 (59%) |
| `dead_store` — assigned, never read | 68 | 21 |
| `dropped_call` — result referenced, call missing | 39 | 50 |
| `truncated_body` — body missing or `return {}` stub | 38 | 8 |
| `duplicate_call` | 8 | - |

**THE DOMINANT FAMILY FLIPPED.** `undefined_register` was 59% of the old list
and is 24% of the real one; `name_collision` is now the majority at 54%. The
old advice to start with `undefined_register` was an artefact of the half
corpus, not a fact about the decompiler.

The broken rate fell with it: 246 of 1,111 (22%) against 271 of 605 (45%). That
is consistent with `bc/` being a decompiler TEST corpus - deliberately hard
files - rather than a sample of what the game actually ships.

Densest now: `350_..._BuffTile` (18), `373_ItemPackages_Accessory` (17),
`438_LocalFX_LocalFlames` (15), `282_Gui_GuiGrid` (13).

The semantic checker disagrees with the sweep by design and both are kept: it
reports 97 defects across 1,142 files (96.1% clean) because it reports only what
is PROVABLY wrong, where the sweep reports what looks wrong. Trust `broken`,
never trust `clean`.

PREVIOUS LIST, kept for comparison — densest files were: `220_..._RootCamera_Follow` (22), `352_MonsterTypes` (15),
`10_PlayerAbilities` (13), `597_..._MasterControl` (10).

Verified by hand, all real: `return tbl[math.min(arg1, tbl)]` (the `#` was
dropped), `return v3 .. "."` where `v3` is declared line 7 and never assigned,
`local value2` declared inside an `if` then used after it, `if 0 < tbl2` in
`368_Orbiter.lua`, `table.remove(tbl4, tbl4)` in `456_TradeRequestGui.lua`.

SUPERSEDED 19 Aug 2026: `undefined_register` at 59% was true of `bc/` only. On
the full corpus it is 24% and `name_collision` dominates at 54%. The link to
commit 26b3b9f (a value assigned in every branch arm, each to a fresh `local`,
all discarded at the merge) is still worth checking, but it is no longer the
biggest lever - it is the second.

**Trust `broken`, never trust `clean`** — recall is roughly half. Use the sweep
to add candidates, never to rule a file out.

## Local tier — free, unlimited

```sh
.\tools\local-claude.ps1                                 # Claude Code, local model
ollama run local-coder                                   # chat
python tools/sweep_local.py target/full1143/decompiled/<run> --out sweep_full1143.jsonl   # corpus triage (NOT bc/ - it is half the corpus)
python tools/index_all.py --out index_all.jsonl           # read corpus + vault
```

Desktop shortcuts exist: **AI - Local (free)**, **AI - Cloud Fast**,
**AI - Cloud Heavy**.

`local-coder` = `gemma4:12b-it-q4_K_M` + `num_ctx 65536`
(`tools/Modelfile.local-coder`). It beat **nine** local models on executed
coding tasks (7/7) with zero invented defects: `gemma4:26b`, `qwen3-coder:30b`,
`nemotron-3.5-lightning`, `gpt-oss:20b`, `qwen3.5:9b`, `ornith:9b`,
`qwen3.6:27b`, `qwen3.8:27b`, `gemma4:e4b`.

Persisted config (User scope): `OLLAMA_NUM_PARALLEL=4`, `OLLAMA_KEEP_ALIVE=30m`,
`OLLAMA_FLASH_ATTENTION=1`, `OLLAMA_MODELS=C:\Projects\ML\_model-cache\...`.

### Speed is maxed; stop tuning it

~43 tok/s single-request = **84% of the card's theoretical ceiling**
(448 GB/s ÷ 8.5GB ≈ 52 tok/s). Nothing in software moves this.

Measured and rejected: driver update (flat), `NUM_PARALLEL=8` (**worse** on real
work — 10.7 vs 11.6 files/min, despite winning a synthetic benchmark), NVFP4
(macOS-gated by Ollama), `q8_0` (spills, 3x slower), higher effort settings
(dramatically slower). Only win was 4-worker parallelism: **5.9 → 11.6
files/min** on batch jobs. Real speedup now needs more memory bandwidth.

### Hard-won gotchas

- **Always set `num_ctx`.** Default 262144 builds a KV cache big enough to push
  a 17GB model half into system RAM. This one setting invalidated several early
  benchmarks. 65536 costs under 1GB more than 8192 and is no slower.
- **Anything over ~14.2GB spills and becomes unusable.** That is the 5060 Ti's
  16GB minus Windows. Measured: `qwen3.8` 5 min/task, `qwen3.6` 6.5 tok/s,
  `nemotron` 4 hallucinations. The GTX 1650's 4GB only activates above ~15GB and
  never rescued anything.
- **`think` is per-model and getting it wrong is not a soft failure.** gemma4
  returns an EMPTY response without `think:false`; `qwen3-coder` **crashes
  llama-server** with `0xc0000409` if you send the field at all, killing every
  loaded model.
- **Constrain `kind` fields to a schema enum.** Free-string kinds produced 35
  categories including `Badge Awarding` and `RemoteEvent_Execution` — the model
  describing the *game* rather than the decompiler. The enum cut 892 findings to
  624 and 35 kinds to 5. **~30% of the old defect map was category noise.**
- **`num_predict` starvation looks like incapability.** Four cloud models scored
  0/3 at 3000 and 3/3 at 16000. Never lower it to go faster.
- **PowerShell 5.1**: `ConvertTo-Json` mangles source text, `[Text.Encoding]::UTF8`
  writes a BOM the API rejects, `Start-Process` has no `-Environment`. Build
  Ollama requests in Python.
- Do not leave scripts named after stdlib modules in `%TEMP%` — a stray
  `inspect.py` shadowed the standard library and broke every later script.

## Cloud tier — Ollama Pro

**22 models verified working.** Every one needs the `:cloud` suffix; bare names
404. `kimi-k3:cloud` is 402 payment-gated.

```sh
.\tools\cloud-claude.ps1                                 # gemma4:cloud (default)
.\tools\cloud-claude.ps1 -Model deepseek-v4-pro:cloud    # heavy jobs only
python tools/ask_cloud.py --list                         # benchmarked models
python tools/ask_cloud.py --file some.rs "explain this"
```

15 of 22 score 3/3 on executed coding tasks. Best points:

| Model | Score | Wall | tok/s | Note |
|---|---|---|---|---|
| `gemma4:cloud` | 3/3 | **21.5s** | 43.1 | fastest, low quota — default |
| `deepseek-v4-flash:cloud` | 3/3 | 97.1s | **181.4** | needs `think=low` |
| `deepseek-v4-pro:cloud` | 3/3 | 60.3s | 132.6 | 1.65T, 1M ctx, **extra high** quota |
| `glm-5.2:cloud` | 2/3 | 100.8s | **223.9** | fastest tokens, fails `find_undefined` |

- **Cloud cannot do structured outputs.** Ollama's docs say so and it was
  confirmed: a schema-constrained request came back as markdown. So
  `sweep_local.py` and `index_all.py` are permanently local-only.
- **Size predicted nothing.** `gemma4:cloud` beat 675B Mistral, 756B GLM and
  397B Qwen on wall clock. `glm-5.1` (756B) managed 1/3 even at max settings.
- Watch the **usage tier**: `deepseek-v4-pro` is rated "extra high" and scored
  the same 3/3 as `gemma4:cloud` in 3x the time. Quota is metered on GPU time.

## The operating rule

**Execute everything the model writes.** Across every model tested, each one
produced plausible, well-formatted, confidently-presented code containing a real
bug — a `ValueError`, a truncated string, `None` where an int belonged. Fluency
carried no signal. Only test strictness determined whether the bug was visible:
loosening one assertion from 5 fields to 4 turned a genuine failure into a
"pass", and `gemma4:e4b` looked 77% faster while calling **37 of 40 known-broken
files clean**.

For defect audits: Roblox child-instance lookups like
`game.ReplicatedFirst.PlaceType` or `script.IsHiveHub` are **valid** and are not
bugs. Three separate models invented that defect.

## Tooling

`sweep_local.py` (defect triage) · `index_all.py` (corpus + vault knowledge map)
· `bench_models.py` · `bench_cloud.py` · `bench_parallel.ps1` ·
`ollama_inventory.py` · `ollama_model_stats.py` · `ask_cloud.py` ·
`style_profile.py` · `local-claude.ps1` · `cloud-claude.ps1`

Data: `sweep_full1143.jsonl` (1,111 files, the real corpus), `sweep_local.jsonl` (+ `.prev`, bc/ only - half the corpus), `index_all.jsonl` (+ `.prev`, 728 files),
`cloud_bench_combined.json`, `ollama_inventory.json`, `ollama_model_stats.json`.

## Open

- **The 712 defects are unfixed.** Start with `name_collision` (388, 54%) - it
  is the majority on the real corpus. `undefined_register` (171, 24%) is second
  and is the one related to commit 26b3b9f (a value assigned in every branch
  arm, each to a fresh `local`, all discarded at the merge).
- 5 files error in the full-corpus sweep - content-specific.
- `index_all.jsonl` under-reports defects by design (its prompt asks what files
  *do*). Use `sweep_full1143.jsonl` for defects, `index_all.jsonl` for knowledge.
- Vault notes are largely Claude-written, so they are not a source for
  `style_profile.py` if the goal is Jep's own voice.
