# Full-machine semantic scan

**Run:** 2026-08-07 · binary at commit `c110ab9` · `cargo test -p luau-core --release --test scan_everything -- --nocapture`

Every Lua/Luau file reachable on this PC, checked for defects **by consequence**
rather than by marker string. Re-running takes about a minute, so this document
exists to avoid re-deriving the conclusions, not the numbers.

---

## Coverage

```
files found      32,276
files opened     32,276      ← every one, none skipped
unreadable            0
lines read    6,024,150
```

Coverage is 100% of files found. `files opened` is reported precisely so the
claim can be checked rather than trusted — an earlier report in this project
said "0 defects" after reading ~50 lines of 3 files out of 1,273, and that must
not happen silently again.

**The first run missed 1,522 files.** It walked five roots; a drive-wide search
afterwards found more, including `C:\Users\jep\.luau_tools\corpus` — the
round-trip corpus the README's correctness claim rests on. Scanning everything
*except* the reference corpus would have been a poor joke. Nine roots are walked
now; `.git`, `node_modules` and `target` are skipped.

---

## Results

```
clean            18,843  (58.4%)
defects          58,485
```

| check | defects | what it means |
|---|---:|---|
| `undefined_local` | 57,676 | name read or written but never bound — errors at runtime |
| `bodies_dropped` | 294 | proto count far exceeds emitted functions |
| `discarded_table_write` | 268 | table built in a loop and never read; the write target was lost |
| `name_body_mismatch` | 208 | a function's body identifies itself as a *different* function |
| `property_called_as_method` | 39 | `script:Parent()` — a property read emitted as a call |

### By root

| root | files | clean | defects |
|---|---:|---:|---:|
| Potassium | 26,172 | 16,235 | 42,634 |
| temp-claude | 3,318 | 1,090 | 9,546 |
| luau_tools | 1,501 | 721 | 3,380 |
| Downloads | 1,141 | 672 | 2,829 |
| claude | 61 | 61 | 0 |
| Desktop | 34 | 29 | 26 |
| Documents | 35 | 29 | 6 |
| tmp | 11 | 3 | 64 |
| Projects | 3 | 3 | 0 |

**`luau_tools` at 3,380 defects is worth a look.** That tree holds the reference
corpus. Its hand-written originals should be spotless; if the defects are in the
originals rather than in decompiled output stored alongside them, either the
corpus is not what it claims to be or a check is still unsound. Not yet
investigated.

### A reporting bug found mid-run

The first expanded run reported `Projects: 4,834 files` when `C:\Projects` holds
three. `root_label`'s fallback arm named a real bucket, so every root added
later was silently absorbed into it. A catch-all that names a real destination
produces a *wrong* number instead of an obviously missing one; the fallback is
now `"other"`.

---

## What these numbers are NOT

**This is a snapshot of the past, not of current capability.**

All 27,445 files are output from binaries *predating* today's fixes. The
dominant defect — `undefined_local` at 44,877, i.e. 98.6% of everything — is
one root cause repeated: a closure capturing a parent local kept the use and
lost the declaration. That was fixed today, measured 48 → 0 on freshly
decompiled bytecode.

So the honest prediction: **re-decompiling this corpus with the current binary
should remove most of those 44,877 defects.** That is a prediction, not a
measurement, and it stays that way until raw bytecode is available to re-run.
Blocked on Potassium.

---

## Corrections found by doing this

### 1. The checker itself was unsound

The first pass reported **1,890** `property_called_as_method`. Reading a single
flagged file killed it:

```lua
local builder = ChangelogBuilder.new(...)
builder:Section("...", function(p) p:Text("...") end)
```

`:Text()` is a real method on a user builder object. Nothing is wrong with that
code. The check was firing on *any* receiver whose method name collided with a
Roblox property name.

Fixed by requiring the receiver to be provably an Instance (`script`, `game`,
`workspace`). Result: **1,890 → 6**, i.e. **99.7% of those findings were false
positives.**

This module's own header says a check must be sound because "a check that
produces false positives trains people to ignore the report" — and it shipped
violating exactly that. Worth remembering that writing the rule down is not the
same as following it.

### 2. The hand-written / generated split is wrong

The scan reports `decompiler output 19,119` and `hand-written 8,326`, but the
classifier only recognises **our own** header. Decompiled output from other
tools — most of `bgs_decomp_fresh`, for instance — is counted as hand-written.

**The real hand-written figure is far below 8,326.** Do not cite that number.
Fixing it needs a better generated-code heuristic than a header match.

---

## What to look at next

`name_body_mismatch` (208) is the one worth reading, despite being small.

Every other check finds code that is *obviously* broken — it errors, or it is
visibly a stub. `name_body_mismatch` finds code that is **confidently wrong**:
correct-looking names on correct-looking bodies, wired to each other wrongly.
The original instance was `Events.lua`, where calling `Events.Create` actually
ran `ServerCall`, and nothing in the output looked amiss.

That is the class most likely to be trusted and acted on while being false,
which makes it more dangerous per instance than the 44,877.

---

## Reproducing

```bash
cargo test -p luau-core --release --test scan_everything -- --nocapture
```

Roots are the `ROOTS` constant in `crates/luau-core/tests/scan_everything.rs`.
Runtime ~1 minute in release mode.
