# luau-decompiler — working notes

A Luau bytecode decompiler, disassembler and obfuscator in Rust. Fully offline.
This file is read by Claude Code and by the local model, so work can continue on
either. Keep it short — it is loaded into every session, and the local model only
has an 8192-token window.

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

## Local model tier

For the days the Claude quota is exhausted. Free, uncapped, runs on the RTX 5060 Ti.

```sh
ollama run local-coder                              # chat
ollama launch claude --model local-coder            # Claude Code harness, local model
python tools/sweep_local.py bc --out sweep_local.jsonl   # corpus triage
```

`local-coder` is `gemma4:12b-it-q4_K_M` with `num_ctx 8192` baked in
(`tools/Modelfile.local-coder`). It won a matched benchmark against
`gemma4:26b`, `qwen3-coder:30b-a3b` and `nemotron-3.5-lightning`: most correct
code, ~1478 tok/s prefill, the only one with zero hallucinations when auditing,
and the only one that fits entirely on the fast GPU.

### Hard-won gotchas

- **Always set `num_ctx`.** The default is 262144, which builds a KV cache large
  enough to push even a 17GB model half into system RAM. Capping it at 8192 took
  `gemma4:26b` from 19.3 to 42.1 tok/s. This one setting invalidated several
  early benchmark runs.
- **`think: false` is required for gemma4** — without it the response comes back
  empty, with the whole token budget spent on reasoning traces.
- **`think` crashes `qwen3-coder`.** Not a 400 — it kills llama-server with
  `0xc0000409` (stack buffer overrun), taking down every loaded model. Send the
  field only to gemma-family models.
- **PowerShell 5.1 `ConvertTo-Json` mangles source text**, and
  `[Text.Encoding]::UTF8` writes a BOM the API rejects. Build Ollama requests in
  Python, not PowerShell.
- Two GPUs: the GTX 1650 (4GB) only activates for models over ~15GB. A model that
  fits on the 5060 Ti staying there is correct, not a fault.

### Ollama Pro cloud tier

18 cloud models, driven through the local daemon with the signed key in
`~/.ollama` — no API key, no separate endpoint, no local VRAM.

```sh
python tools/ollama_fanout.py tasks.jsonl --out results.jsonl \
    --model gpt-oss:120b-cloud -j 14        # 12 concurrent calls in 6.3s
```

`tools/ollama_fanout.py` takes a JSONL of `{id, prompt, schema?}` and fans it
out; resumes by id, retries with backoff, caps `num_ctx` for local models only
(capping a cloud model just truncates the prompt).

- **`gpt-oss:120b-cloud` is the workhorse.** Measured on a 52-task structured
  merge: 34/35 ok. Same lesson as the local tier — the biggest model is not the
  best one.
- **`qwen3.5:397b-cloud` returned an empty body on all 17 tasks it was given.**
  It spends the whole `num_predict` budget on reasoning before emitting
  anything. `think: false` is not the fix — sending `think` to qwen crashes
  llama-server. Raise `num_predict` hard or do not use it for JSON.
- **`deepseek-v4-flash` truncates schema-constrained output** at
  `num_predict 1200`; 3000 was enough. Same failure `sweep_local.py` hit at 800.
- A schema-constrained model can still return an **array where an object was
  asked for** — 13 of 52 did. Coerce, do not crash.
- **The cloud tier has a concurrency ceiling around 16.** `-j 14` and `-j 16`
  ran 0 failures at ~35s median. Raising to `-j 40` produced **HTTP 429 Too
  Many Requests on 41 tasks and pushed median latency 35s → 110s** — slower AND
  lossier. Concurrency is not the speed lever.
- **The speed lever is FEWER, LARGER tasks.** Same bytes, fewer round-trips:
  raising the chunk from 20K to 52K chars took one job from 562 tasks to 221
  with no extra failures. Latency is per-request, not per-byte.
- Do not lower `num_predict` to go faster. Dropping 3500 → 1600 on a
  schema-constrained job returned **52 empty bodies**; the budget is consumed
  before the JSON is emitted.

### The operating rule

**Execute everything the model writes.** Across four models and four coding
tasks, every single one produced plausible, well-formatted, confidently-presented
code containing a real bug — a `ValueError`, a truncated string, `None` where an
int belonged. Fluency carried no signal at all. The strictness of the test was
the only thing that determined whether the bug was visible: loosening one
assertion from 5 fields to 4 turned a genuine failure into a "pass".

The same applies to defect audits. Every model except `local-coder` invented
defects — most often claiming Roblox child-instance lookups like
`game.ReplicatedFirst.PlaceType` are invalid property accesses. They are valid.

## Corpus triage findings

`tools/sweep_local.py` over `bc/` (605 files, 85 min, free):

- 306 broken, 63 suspect, 171 clean, 788 defects with line citations → `sweep_local.jsonl`
- **406 of 788 defects are the `local`-declared-inside-a-branch shape** — over
  half. A value is assigned in every arm, each to a fresh `local`, and all are
  discarded at the merge. Same family as commit 26b3b9f; either that fix does not
  cover this shape or it regressed.
- Clearest example: `110_..._Collect_Tokens.lua` builds `Amount2`…`Amount8` in
  branches, discards them all, then returns a never-assigned `v3`.
- Densest file: `427_ReplicatedFirst_PlaceInfo.lua`, 11 defects.
- Trust `broken` verdicts, **never trust `clean`** — recall is roughly half, so
  use the sweep to add candidates, never to rule a file out.

## Open

- 65 files of the sweep never ran (server was OOM-killed at file 549). The script
  retries and resumes: re-run the same command.
- `nemotron-3.5-lightning` (25GB), `qwen3-coder` (18GB), `gemma4:26b` (17GB) are
  on disk and all measured worse than the 7.6GB `local-coder`. `ollama rm` when
  the disk is wanted. Keep `gemma4:12b-it-q4_K_M` — `local-coder` layers on it.
