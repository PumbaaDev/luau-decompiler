# The probe set

Roblox renumbers Luau's opcodes in every client build. The rest of this
decompiler *infers* that renumbering from the structure of the bytecode, which
works well enough to be useful and will never be exact.

It does not have to be inferred. A client that can compile source can be asked
to compile source you already have, and the renumbering falls straight out of
comparing its output with a compilation whose numbering is documented.

These files are that source.

## Why these particular programs

No ordinary script exercises the whole instruction set. Measured over a
47-program corpus of normal Luau, the richest single file uses 28 of the 83
opcodes and the whole corpus pooled reaches 66. The rest never turn up by
accident:

| opcode | what forces it |
|---|---|
| `SUBRK`, `DIVRK` | a constant on the **left**: `3 - x`, `3 / x` |
| `ANDK`, `ORK` | `x and 5`, `x or 5` |
| `GETGLOBAL`, `SETGLOBAL` | a global the same chunk also **writes** (otherwise the import optimiser turns it into `GETIMPORT`) |
| `FORGPREP_NEXT` | `pairs(t)` or `next, t` specifically |
| `JUMPXEQKB` | `x == true` |
| `JUMP` | an if/else with enough code after it that duplicating the tail is not cheaper |
| `DUPTABLE` | a table literal whose keys **and** values are all constants |
| `LOADKX` | more than 32 767 distinct constants in one function |
| `JUMPX` | a forward jump over more than 32 767 instructions |

The set writes each of these on purpose and reaches **79 of the 83** canonical
opcodes.

## The four it cannot reach

`NOP`, `BREAK`, `NATIVECALL` and `COVERAGE` are not missing by oversight: no
compiler emits them from source. `NOP` and `BREAK` are inserted at runtime by
the debugger, `NATIVECALL` is patched in by the native code generator at load
time, and `COVERAGE` needs a compiler option no shipping configuration sets.
A fifth internal slot — a generic-for variant removed from upstream Luau — has
no canonical counterpart at all.

Those five stay on the inference path. That is still most of the value: the
residual ambiguity drops from 84-way to 5-way.

## Layout

```
probe/
  sources/    the programs, one construct family per function
  fixtures/   three of them precompiled, used by the Rust test-suite
```

`p*` files each target one family. `m*` files are **mirrors**: the same opcodes
expressed through different source text. Every opcode in the core tier appears
in at least two files, so a client that lowers one construct differently loses
coverage rather than losing the opcode.

The two heavy-tier programs (`h01_loadkx`, `h02_jumpx`) are generated on demand
rather than stored — they are hundreds of kilobytes of machine-written text and
buy two rare opcodes between them.

## Rules these sources obey

- **No `getfenv` / `setfenv`.** Either one disables import and builtin
  optimisation for the whole chunk, which would silently delete `GETIMPORT` and
  every `FASTCALL*` from the set.
- **No vector constructors.** A client configured with a vector library compiles
  `SomeVector.new(a, b, c)` to a fastcall where upstream emits `GETIMPORT` +
  `CALL`, and the mismatch would reject the prototype.
- **No `bit32`.** Some clients implement bitwise operations as native opcodes
  upstream Luau does not have, so one side would emit a single instruction where
  the other emits three. `FASTCALL2K` and `FASTCALL3` come from `string.sub` and
  `math.clamp` instead.
- **Small functions.** A prototype that fails to align loses every opcode in it,
  so each one carries as few as possible.

## Using them

```sh
# 1. write the sources out
luau-decompiler probe emit --out ./probe-src --tier core

# 2. compile them with upstream luau-compile (numbering we know)
for f in ./probe-src/*.luau; do
  luau-compile --binary -O1 -g1 "$f" > "./canonical/$(basename "$f" .luau).luac"
done

# 3. compile the SAME sources with the client whose numbering you want, and
#    dump each chunk into ./client/ under the same names

# 4. read the permutation off the pair
luau-decompiler probe align --canonical ./canonical --client ./client \
    --id my-build --out my-build.json

# 5. store it, and every later decompile of that build is exact
luau-decompiler opmap-db import my-build.json --opmap-db opmap_db.json
luau-decompiler script.luac --opmap-db opmap_db.json
```

Step 3 is the only part this tool cannot do for you, because it depends on how
the client exposes compilation.

## What alignment does, and what it refuses to do

It walks the canonical stream (which is self-decoding) and reads the client's
instruction words at the same offsets. At every position it requires bits 8..31
to be identical, because a permutation relabels the opcode byte and nothing
else. Any divergence rejects that prototype and reports it.

Measured failure behaviour:

| situation | result |
|---|---|
| clean pair | 79 pinned, 0 wrong |
| client compiled at a different optimisation level | 72 pinned, 0 wrong |
| files deliberately mispaired | 6 pinned, 0 wrong |

Coverage degrades. Correctness does not.
