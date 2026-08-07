# luau-decompiler

[![CI](https://github.com/PumbaaDev/luau-decompiler/actions/workflows/ci.yml/badge.svg)](https://github.com/PumbaaDev/luau-decompiler/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-stable-orange.svg?logo=rust&logoColor=white)](https://www.rust-lang.org)

A fast, fully-offline **Luau bytecode decompiler**, disassembler, and obfuscator, written in Rust.

Feed it compiled Luau bytecode and get readable Luau source back. Everything runs 100% locally, no network, no external service.

## Features

- **Decompiler** — lifts Luau bytecode back into readable Luau source: register-based VM state, control-flow reconstruction (if/while/repeat/for), tables, upvalues and constant recovery. Every opcode in the instruction set has a handler, including Roblox's bytecode extensions.
- **Bytecode versions 3–8** supported.
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
luau-decompiler batch ./bytecode -o ./out

# Watch a folder and decompile files as they land
luau-decompiler watch ./drop -o ./out

# Syntax-check a Luau file
luau-decompiler validate script.lua

# Diff two Luau sources with a similarity score
luau-decompiler compare original.lua decompiled.lua
```

Run `luau-decompiler --help` (or `--help` on any subcommand) for the full option list.

## Try it end-to-end

Both demos below run against files already in this repo — no external Luau toolchain required.

### 1. Watch the decompiler recover source

The decompiler consumes **standard Luau bytecode** (v3–8), such as the output of the official `luau-compile --binary` or a Luau runtime's `string.dump`. To stay fully self-contained, the bundled example assembles a small bytecode chunk in memory and lifts it back to source using the same engine the CLI uses:

```sh
cargo run -p luau-core --example decompile_demo
```

Output:

```lua
for i = 1, 10 do
    local function run()
        return i
    end
    local tbl = {
        run = run
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
unit tests, 900+ of them); the interpreter round-trip test skips cleanly when the binary is
absent.

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

Seven Roblox client modules, decompiled and checked:

**3 of 7 fully clean, 4 remaining defects, 0 undefined locals.**

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
