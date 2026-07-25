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

**Current: 46 of 47 programs decompile to semantically equivalent source.** The one
outstanding failure is a nested short-circuit expression (`t and t.n or -1`) whose value is
produced across a control-flow join; recovering it correctly needs phi-node materialisation
rather than another pattern match.

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
