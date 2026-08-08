# Where generated names come from

`vN`, `upval_N` and `cap_N` are names the decompiler invents because the real one
was not recovered. They are the largest remaining defect class. Until now the
quality report could only say *what* they were — 471 of them labelled "register
never stored in this proto", 159 "no fallback event observed" — never *which
code path minted them*. That is an absence, not a mechanism, and you cannot fix
an absence.

This measures the mechanism.

## How to reproduce

`decompiler/mint_trace.rs` records, per chunk, which site minted each generated
name. It is gated on `LUAU_MINT_TRACE` and is a no-op when unset — verified
behaviour-neutral against all three gates (below).

```bash
LUAU_MINT_TRACE=1 luau-decompiler batch <corpus> 2> mint.txt
```

Two record types on stderr:

| record | meaning |
|---|---|
| `MINT<TAB>BRANCH<TAB>name<TAB>inclosure=<TAB>sites=` | a name reached `free_var_decls`, with the branch that hoisted it and every site that minted it |
| `REJECT<TAB>SITE<TAB>name<TAB>discarded=` | the lifter **had** an expression and threw it away |

**Measure on a private copy of the corpus.** The `Decompiler.exe` app respawns
`luau-decompiler batch` against
`AppData\Local\Potassium\workspace\bc_extract_*` and rotates the run directory
out from under a reader. That is what produces `SKIP: no .lua files` from the
compile gate on a directory that demonstrably had 630 of them a minute earlier.

## Result — 2,147 generated names over 628 chunks

| branch + site | count | share |
|---|---:|---:|
| `READONLY` `REG_EXPR_UNKNOWN` | **944** | 44.0% |
| `ASSIGNED` (no mint site) | 669 | 31.2% |
| `READONLY` `TABLE_EXPR_REJECT` | 163 | 7.6% |
| `READONLY` `GEN_VAR_NOHINT+REG_EXPR_UNKNOWN` | 115 | 5.4% |
| `ASSIGNED` `GEN_VAR_NOHINT` | 87 | 4.1% |
| `READONLY` `REG_EXPR_UNKNOWN+TABLE_EXPR_REJECT` | 67 | 3.1% |
| everything else (17 combinations) | 102 | 4.7% |

The `ASSIGNED` rows are not the problem — those names are written somewhere in
the body, so hoisting a declaration for them is defensible. The defect is the
`READONLY` population: declared, read, never assigned. Every use evaluates to
`nil` while the output still parses.

**One site dominates: `reg_expr`'s fallback arm on a register holding
`RegVal::Unknown`.** That is the mechanism behind "register never stored in this
proto". It is not a diagnosis on its own — the question is why the register is
`Unknown` — but it is a call site, which the old label was not.

## The reject paths are correct — they are symptoms, not causes

775 rejects, and in every one the lifter had a value and discarded it. That
sounds like the bug until you look at what was discarded:

| discarded | `TABLE_EXPR` | `METHOD_RECV` |
|---|---:|---:|
| `Number` | 283 | 15 |
| `BinOp` | 282 | 1 |
| `Bool` | 78 | 35 |
| `Nil` | 58 | 23 |

Every rejected `BinOp` is arithmetic — `Add` 91, `Sub` 69, `Mod` 57, `Mul` 38,
`Pow` 9, `Div` 5, plus 6 comparisons and 2 `Concat`. Not one `And`/`Or`, which
`is_impossible_as_table` already exempts precisely because `(a or b).field` is a
legal and common idiom.

So indexing these really is impossible, the guard is right to refuse, and the
`vN` it mints is the guard making an upstream mis-decode **visible**. Removing
these rejects would not recover a single name; it would emit `(x % y).Name` and
launder the defect into something that parses. That is the same anti-pattern as
the `free_var_decls` masking bug and the semicolon experiment, both already
reverted on measurement.

## The finding: this is not one defect class, it is two

Splitting the 944 by whether the chunk had any opcode byte left unmapped:

| chunk | files | mints |
|---|---:|---:|
| **has unmapped opcodes** | 165 | **614** (65%) |
| **zero unmapped opcodes** | 165 | **330** (35%) |

Two thirds is the opcode-coverage family this project has been chasing since the
CALL and DUPCLOSURE byte-theft fixes. **A full third is not.** Those 330 names
are minted on chunks where every opcode byte the chunk uses was resolved.

### Weak opcode evidence does not explain the remainder either

The obvious rescue is that those chunks resolved every byte but resolved some of
them *wrongly*, because a chunk exercising few opcodes constrains the shuffle
weakly. The headers carry the coverage, so this is directly testable — and it
comes out backwards:

| opcodes exercised | zero-unmapped files | of those, with mints | mints |
|---|---:|---:|---:|
| `<20` | 241 | 31 (13%) | 59 |
| `20–34` | 155 | 89 (57%) | 154 |
| `35–49` | 52 | 44 (85%) | 116 |
| `50+` | 1 | 1 | 1 |

The mint rate **rises** with opcode coverage — 13% of the weakest-evidence
chunks against 85% of the strongest. If weak evidence were the cause the
gradient would run the other way. It tracks program complexity instead.

### ⚠ What that does NOT license concluding

An earlier revision of this document read that gradient as "the lifter, not the
map", i.e. a defect class finally separated from the opcode family. **That
conclusion was not supported and has been withdrawn.**

"Zero unmapped" means every byte the chunk executes was *assigned* an opcode. It
does not mean it was assigned the *right* one. A byte mapped wrongly is silent:
it produces no unresolved-instruction count, decodes to a plausible instruction,
and the header reports full coverage.

The reproducer is direct evidence of exactly that. Disassembled with the map
applied, `258_ReplicatedStorage_NPCs_SunBear2Init` — 2 protos, 541 constants,
166 `DUPTABLE`, "0 left unmapped" — contains:

```
343  JUMPIFNOT      <- in a data module with no control flow
166  DUPTABLE
128  SETTABLEKS
 74  SETLIST
 72  NEWTABLE
  0  LOADK / LOADN / LOADB / GETIMPORT
```

**A module with 541 constants that executes no constant-load opcode is
impossible.** Something that should load a constant is decoding as `JUMPIFNOT`,
on a chunk reporting full opcode coverage. That is a mapping failure, not a
lifter failure.

So the honest split is **visible mis-mapping (unmapped bytes) versus silent
mis-mapping (wrong byte, full coverage reported)** — not map versus lifter. The
lifter hypothesis is neither established nor excluded; the coverage gradient is
consistent with both, because more complex chunks execute more distinct bytes
and so have more opportunities to be handed a wrong one.

## Two claims withdrawn, and why

Both were made in this document and both came from reading bytes off a view that
could not support them.

**1. "The disassembler disagrees with the decompiler."** It does not.
`disassemble` applies the opcode map only when `--opmap` is passed;
`--opmap-cache` alone takes the raw path and silently produces an unmapped view.
Passing `--opmap --opmap-cache` on the reproducer gives **zero `UNKNOWN`** and a
coherent `DUPTABLE`/`SETLIST`/`SETTABLEKS` stream. The prior claim of
`R2557922`-style garbage was operator error, not a tool defect. The two paths
agree.

**2. "Raw byte `0x6F` is `LOADK` and is being stolen by `JUMPIFNOT`."**
Withdrawn. It came from reading byte values off an *unmapped* disassembly.
Without a map the disassembler cannot compute instruction lengths, so it
mis-steps through AUX words and prints operand bytes as opcodes — 192 distinct
"opcodes" over 40 files, against a Luau instruction set of ~84, is the tell. Raw
byte positions from an unmapped view are not evidence.

The same reasoning invalidates the obvious next test: you cannot collect the set
of genuinely-executed opcode bytes without already holding a correct map.

## The measured permutation is not usable as it stands

`target/release/opmap_ground_truth.json` holds a 76-entry `{hex: NAME}` map, and
`opmap-db` will import a bare map of that shape. It refuses this one:

```
Error: entry has no provenance.method
       every entry must say how it was produced
```

That guard is correct and should not be worked around. The file sits in a build
output directory with no record of whether it came from `probe align` against
Jep's client, a test fixture, or an older client build — and a permutation
adopted on faith would launder every downstream measurement. Establishing its
provenance (or re-running `probe align` against the live client) is the
prerequisite, and would also settle the reproducer outright: a measured map is
the one source that can say which opcode that chunk's constant-loads really are.

## Verified behaviour-neutral

Same binary, instrumentation compiled in, `LUAU_MINT_TRACE` unset, private
corpus copy:

```
compile gate            621/628 (98.9%)     7 do not compile
semantic clean          266/628 (42.4%)     1272 defects
CoreScript ground truth     9/9  (100%)
```

All three match the committed baseline. The compile gate ran in 7.61s — a run
that reports `ok` in ~0.03s has skipped, and has done so twice in this project.

## Not established

- **Why** a register is `Unknown` on a fully-mapped chunk. The call site is
  known; the cause is not.
- Whether the 330 share one cause or several.
- **Which opcode the reproducer's constant-loads are actually being decoded as.**
  The absence of any constant-load in a 541-constant module is established; the
  byte responsible is not, and cannot be identified from an unmapped
  disassembly (see the withdrawn claims above). A measured permutation with
  provenance settles it; nothing else available here does.

## Next step

Run `probe align` against the live Roblox client to obtain a permutation with
recorded provenance, import it with `opmap-db import`, and re-decode the
reproducer. If its constant-loads reappear, silent mis-mapping is confirmed as
the cause of the 330 and the fix is in byte assignment. If they do not, the
lifter hypothesis survives its first real test. Either outcome is decisive, and
no cheaper experiment distinguishes them.
