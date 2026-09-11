# luau-decompiler

[![CI](https://github.com/PumbaaDev/luau-decompiler/actions/workflows/ci.yml/badge.svg)](https://github.com/PumbaaDev/luau-decompiler/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-stable-orange.svg?logo=rust&logoColor=white)](https://www.rust-lang.org)

A fast, fully-offline **Luau bytecode decompiler**, disassembler, and obfuscator, written in Rust.

Feed it compiled Luau bytecode and get readable Luau source back. Everything runs 100% locally — no network, no external service, no telemetry.

As far as we know it is the only **open, auditable, offline** Luau decompiler whose quality is measured by *executing the recovered code* rather than by how plausible the output looks. Most Luau decompilation lives inside closed, executor-bundled tools; this is the missing piece for security researchers, malware analysts, and Luau tooling authors who need a reproducible, inspectable one.

## Highlights

- **99.4% semantically clean on a real game.** 1,315 of 1,323 scripts pulled from a live Roblox client decompile to source a strict semantic checker passes — and **100% parse as valid Luau**, cross-checked against the official `luau-compile`, not just our own parser.
- **Verified by round-trip, not by eye.** The synthetic suite runs the original, decompiles it, **runs the recovered source**, and requires byte-identical output — 47/47 pass. "Looks right" is treated as a failure, because it usually is.
- **Roblox bytecode v3 through v9 (tver3).** Every opcode has a handler, including Roblox's extensions and AUX-carrying instructions, decoded against an opcode map calibrated to the client's own compiler.
- **Handles opcode-shuffled bytecode.** It infers the opcode permutation before lifting, and stamps every remapped decode with an honest evidence header (what was pinned by detectors vs completed by inference) so you always know what the output leans on.
- **Proven in real security research.** Used to reverse and deobfuscate VM-obfuscated Roblox scripts (Synapse Xen / IronBrew2-family) down to their exact server calls.
- **A companion obfuscator** (`luau-protect`) — the same project that takes obfuscation apart can also apply it: control-flow flattening, constant encryption, operand encoding, identifier renaming.
- **One offline static binary** — no runtime, no network, no services.

## Features

- **Decompiler** — lifts Luau bytecode back into readable Luau source: register-based VM state, control-flow reconstruction (if/while/repeat/for), tables, upvalues and constant recovery. Every opcode in the instruction set has a handler, including Roblox's bytecode extensions.
- **Bytecode versions 3–9** supported, including Roblox v9 / tver3.
- **Disassembler** — human-readable opcode listing, with optional debug info (line numbers, local names) and an opmap-remap diagnostic view.
- **Info** — dump bytecode metadata (protos, strings, params, stack sizes) as text or JSON.
- **Obfuscator** (`luau-compiler`) — a companion protector that compiles/obfuscates Luau (control-flow flattening, constant encryption, operand encoding, identifier renaming, junk insertion).
- **Batch & watch** modes — decompile a whole folder, or auto-decompile files as they appear.
- **Validate & compare** — syntax-check decompiled output and diff two sources with a similarity score.

## Install

Build from source (requires a [Rust toolchain](https://rustup.rs)):

```sh
git clone https://github.com/PumbaaDev/luau-decompiler
cd luau-decompiler
cargo build --release
```

The binary lands at `target/release/luau-decompiler`.

## Usage

```sh
# Decompile a single bytecode file to Luau source (prints to stdout)
luau-decompiler script.bin

# Write to a file
luau-decompiler script.bin -o script.lua

# Disassemble instead
luau-decompiler disassemble script.bin --debug-info

# Bytecode metadata as JSON
luau-decompiler info script.bin --json

# Batch-decompile every bytecode file in a folder
luau-decompiler batch ./bytecode --out-dir ./out

# Watch a folder and decompile files as they land
luau-decompiler watch ./drop --out-dir ./out

# Syntax-check a Luau file
luau-decompiler validate script.lua

# Diff two Luau sources with a similarity score
luau-decompiler compare original.lua decompiled.lua
```

Run `luau-decompiler --help` (or `--help` on any subcommand) for the full option list.

## Try it end-to-end

Both demos below run against files already in this repo — no external Luau toolchain required.

### 1. Watch the decompiler recover source

The decompiler consumes **standard Luau bytecode** (v3–9), such as the output of the official `luau-compile --binary` or a Luau runtime's `string.dump`. To stay fully self-contained, the bundled example assembles a small bytecode chunk in memory and lifts it back to source using the same engine the CLI uses:

```sh
cargo run -p luau-core --example decompile_demo
```

Output:

```lua
for i = 1, 10 do
    local function closure()
        return i
    end
    local tbl = {
        run = closure
    }
    tbl.run()
end
```

Once you have a real bytecode file, decompile it directly with `luau-decompiler script.luac` (or pipe it in: `luau-decompiler decompile - < script.luac`).

### 2. Protect a script with the companion compiler

`luau-compiler` (the `luau-protect` binary) compiles a `.lua` file into a self-contained, obfuscated Luau script — constants encrypted, control flow hidden behind an interpreter loop:

```sh
cargo run --release -p luau-compiler -- crates/luau-compiler/tests/fixtures/hello.lua -o hello.protected.lua
```

It prints `luau-protect: wrote <N> bytes to hello.protected.lua`, and the result is valid Luau that runs anywhere the original did. Add `--max` to enable every protection phase.

## Correctness

Decompilation is lossy by nature: variable names, comments and some structure are discarded
at compile time, so no decompiler can reproduce the original source exactly. "Looks
plausible" is therefore a weak standard, and it is the one most decompilers are measured by.

This project uses a stricter one: **semantic round-trip testing**. For each program in the
test corpus the suite

1. runs the original with the Luau interpreter and records its output,
2. compiles it to bytecode,
3. decompiles that bytecode back to source,
4. **runs the recovered source**, and
5. requires its output to match the original exactly.

Anything less than an exact match is a failure, including output that merely looks right.
This catches whole classes of bug that a visual inspection sails past: dropped loop
increments, upvalues that silently stop being captured, off-by-one table construction,
branches that collapse into the wrong arm.

The corpus covers arithmetic and operators, strings, tables, control flow, and closures /
varargs / metatables / OOP patterns. It is deliberately adversarial rather than a happy path,
and the pass rate is treated as the project's real quality metric.

**Current: 47 of 47 programs decompile to semantically equivalent source.**

This figure is measured out of tree: the round trip needs a Luau interpreter binary to
execute both the original and the recovered source, and neither that binary nor the corpus
ships in this repository, so a fresh clone cannot reproduce the number directly. What CI and
`cargo test` do exercise is the in-repo suite (parser, lifter, disassembler and obfuscator
tests across the workspace, over 1,000 in all and 951 in the `luau-core` library alone, all
green); the interpreter round-trip test skips cleanly when the binary is absent.

A known limitation: when the operands of a nested short-circuit are themselves function
calls, the value-join reconstruction declines to fold them. Doing so would risk emitting a
call twice, which would change behaviour silently rather than visibly, so the analysis
deliberately gives up instead. Values joined across control flow are otherwise recovered
generally, including the idiomatic `t and t.field or default` accessor.

### What that number does and does not cover

**It covers standard Luau bytecode**, where opcodes carry their canonical numbering, such as
the output of `luau-compile --binary` or a Luau runtime's `string.dump`. If that is your
input, this is the figure that matters.

**It does not cover opcode-shuffled bytecode.** Some hosts permute the opcode numbering, and
this decompiler infers the permutation before lifting. That inference is a separate stage,
and the corpus above does not exercise it at all.

Measuring the shuffled path separately, by permuting the same corpus and running the same
round trip, gives a much weaker result: on short programs the detector recovers under half
of the opcode bytes, and no file in the corpus survives the round trip intact. Real scripts
are far larger and give the inference considerably more structural evidence to work from, so
this is a worst case rather than a typical one, but it is not currently a measured claim
either way.

**A poor inference is dangerous precisely because the output still looks clean.** Bytes the
detectors cannot pin are completed by bijection to finish the permutation, so a substantially
wrong map can still produce well-formed, plausible source. To make that visible rather than
silent, every remapped decode now carries an evidence header: how many opcode bytes this
chunk uses were pinned by detectors, how many were filled by completion, how many were left
unmapped, and a list of any unresolved instructions. That header reports **provenance, not
correctness** — measured against ground truth, the detector-backed share predicts per-byte
accuracy only weakly (r ≈ 0.26), so even a high pinned share can be confidently wrong. Read it
as a map of what the output leans on, not a guarantee. Treat results on shuffled input as
unverified and prefer checking recovered behaviour over reading the recovered source. The one
path that carries no such caveat is a database-backed decode, where the map was measured
against the client's own compiler rather than inferred; the header says so when that applies.

### Semantic checking — a second measurement, for input we cannot round-trip

Round-tripping needs the original source to compare against. For real-world
bytecode there is none, so quality there used to be judged by counting marker
strings in the output — how many files contained `upval_N`, a bare `return nil`,
and so on.

**That measure is close to worthless, and it produced a confidently wrong report.**
Three Roblox modules scored "0 defects in 3 of 4 categories" by marker count.
Reading them showed:

| module | what marker counting missed |
|---|---|
| `CameraModule` | 32 protos in, `return {}` out — the whole body gone |
| `ClickToMoveController` | `game[1] = v8`, `Players.LocalPlayer = Enum.KeyCode.Down`, undefined `v9`–`v27` throughout |
| `Events` | every `tbl.X` function carried a *different* function's body |

None contained a single marker string. The `Events` case is the dangerous one:
correct-looking names on correct-looking bodies, wired to each other wrongly, so
calling `Events.Create` actually ran `ServerCall`. A marker count can only find
defects someone already thought to name.

`decompiler::semantic_check` asserts **properties of meaning** instead:

| check | catches |
|---|---|
| `name_body_mismatch` | a body that identifies itself as a sibling function |
| `undefined_local` | a name read or written but never bound |
| `bodies_dropped` | proto count far exceeding emitted functions |
| `discarded_table_write` | a table built in a loop and never read |
| `property_called_as_method` | `script:Parent()`, which errors at runtime |

Every check is *sound*: it fires only when the output really is wrong. A check
that produces false positives trains people to ignore the report, which is worse
than having no check.

### Measured state on real shuffled bytecode

Real bytecode, not a sample: **1,324 unique files** — a hash-deduped union of a
whole live Roblox game (Bee Swarm Simulator, `PlaceId 1537690962`) plus Roblox
CoreScripts, the PlayerModule, and widely-used community modules (topbarplus,
Janitor, evaera-promise) — decompiled in one clean batch run against a measured
opcode map.

| | |
|---|---|
| Decoded | **1,323 / 1,323** (the 1 exclusion ships a compiler error, not a program) |
| Parses as valid Luau | **1,323 / 1,323 (100%)** |
| Semantically clean | **1,315 / 1,323 (99.4%)**, 8 defects |

This corpus is a live-client capture and, like the round-trip corpus above, is
not shipped in the repository, so the 99.4% figure cannot be reproduced from a
fresh clone; it is recorded here as the project's measured result. Syntax
validity is cross-checked against the real `luau-compile`, not just our own
checker — see below for why that mattered.

The 8 remaining defects are adjudicated by hand — none is a checker false
positive, and the number is honestly at the tool's ceiling:

- **2 are not fixable from the input.** Roblox bytecode carries no `CAPTURE`
  records, so the parent register behind a child closure's upvalue is simply
  absent — a bare `upval_N` is the most faithful thing that can be emitted.
- **6 sit in the merge / phi / branch-arm / loop core.** Every candidate fix
  tried so far either regresses a previously-clean file or fails the unit suite;
  three such fixes were written, measured, and reverted precisely because they
  traded one file for another. The numbers are recorded rather than the attempts
  quietly dropped.

That gate — **both** the full `luau-core` unit suite (951/0) **and** a full corpus
diff with no previously-clean file regressing — is what every change has to
clear. It is why the clean rate is trustworthy rather than tuned.

**The measure that decides a fix is not the defect count.** Six changes during
this work scored *better* on the semantic checker while quietly deleting
output — among them a purchase guard and an auto-jump guard, each removed
because a constant kept in a dead register let the branch fold away. All six
were reverted. The arbiter that caught them is total emitted calls across the
corpus: a fix that raises the clean count while the corpus emits fewer calls is
buying a number with invisible damage.

Recent fixes, each verified by that harness rather than by eye:

- **Captured upvalues were never declared** — a closure capturing a parent local
  kept the use and lost the declaration, so output assigned to a global instead.
  It still parsed, which is why markers never saw it. `undefined_local`: 48 → 0.
- **Opcode bytes were claimed by the first detector to guess.** Detectors ran in
  a fixed order and would not take a byte already mapped, so a single coincidental
  match could permanently claim one. `CALL` lost its byte to `CAPTURE` this way and
  was then never assigned at all — every call in the chunk decoded as a no-op.
  Strong evidence can now displace weak evidence. `CameraModule`: 1 → 34 call sites.

**Known open defect: `bodies_dropped`.** `CameraModule` still emits 0 function
declarations from 32 protos. The instruction stream now decodes correctly; the
remaining fault is in the `DUPCLOSURE` → `SETTABLEKS` → declaration path, which
attaches methods to a module table. The test for this is committed and
deliberately failing — it documents a real defect and stays red until fixed.

### A tool that lies is worse than no tool

`validate` used to reject **106 files from an earlier 1,309-file run**,
reporting a `function`
left unclosed hundreds of lines from anything wrong.

Every one of them was valid. The official `luau-compile` — the real Luau
compiler — accepted all 106. The bug was ours: the block tracker pushed an entry for
every `then` token, including the one after `elseif`, but `if/elseif/else`
closes with a single `end`. Each `elseif` leaked one entry, and because the
stack is LIFO the survivor reported at EOF was the outermost opener.

The correlation was exact — 106 files contain `elseif` and 106 failed; the
1,203 without `elseif` all passed, with no exception either way.

Two things are worth taking from that. A false failure costs more than a missed
one, because it sends you hunting a bug that does not exist — this nearly
became a hunt for an emitter fault across 100 files. And **when a checker and a
reference implementation disagree, find out which is wrong before believing
either.** The official `luau-compile` is the reference this project checks
against, so that question is always one command away.

## Project layout

| Crate | What it is |
|-------|------------|
| `luau-core` | The decompiler engine — parser, opcode mapping, AST, lifter, emitter. |
| `luau-cli`  | The command-line front-end (`luau-decompiler`). |
| `luau-compiler` | The companion Luau obfuscator/protector. |

## How it works

`luau-core` parses the bytecode chunk (32-bit instruction words, ABC/AD/E encodings, the constant and proto tables), reconstructs each proto's control-flow graph, and runs a register-tracking lifter that turns the flat instruction stream back into structured Luau statements and expressions, which the emitter pretty-prints.

## License

MIT — see [LICENSE](LICENSE).

## Contributing

Issues and pull requests are welcome. If you hit a bytecode file that decompiles incorrectly, a minimal repro (the bytecode plus the expected source) is the most useful thing you can open.
