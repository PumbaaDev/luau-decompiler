#!/usr/bin/env python3
"""Count STRUCTURE, not defects, between two decompiler run directories.

The semantic checker has no check for a deleted `if`. A region-structuring
change can therefore delete guards and SCORE BETTER: measured 22 Aug 2026, one
such change read "fixed 1, broke 0, 33 -> 32 defects" while five files lost an
`if` and `lv_0910_VRCamera` lost three guards outright, so a body that was
gated on `arg1 == "offtrack"` ran unconditionally.

A second change read -28 files by the semantic score while 146 files lost
structure, the worst dropping 208 calls, 63 ifs and 27 loops.

23 Aug 2026: this tool was itself BLIND to a real corruption. A change rewrote
23 files, turning a dropped field write into `v9.PlaybackSpeed = v9`, and
passed clean at "0 LOSE structure" because an assignment is neither an `if`, a
call nor a loop. The corpus totals did not move either, and the affected file's
own defect row was unchanged. Only reading the emitted line caught it. Hence
`assign` and `selfassign` below.

So: never accept a structuring change on the semantic score alone. Run this,
and require ZERO losses.

    python tools/structure_diff.py <control_run_dir> <patched_run_dir>
"""
import pathlib
import re
import sys


# `x.field = x` - a write whose value is its own target. Always a defect
# EXCEPT `tbl.__index = tbl`, the standard class-table idiom, which is correct
# under the probe-measured opmap (see CLAUDE.md).
SELF_ASSIGN = re.compile(r"^[ \t]*([A-Za-z_]\w*)\.(\w+)[ \t]*=[ \t]*\1[ \t]*$", re.M)

IF_RE = re.compile(r"^\s*if .+ then\s*$", re.M)
CALL_RE = re.compile(r"\w\s*\(")
LOOP_RE = re.compile(r"^\s*(?:for|while) .+ do\s*$", re.M)
ASSIGN_RE = re.compile(r"^\s*[\w.\[\]\"']+\s*=\s*\S", re.M)


def counts(text: str):
    """(ifs, calls, loops, assigns, -selfassigns) ignoring header comments.

    `assigns` falls when a write is dropped - the loss that slipped through on
    23 Aug. `selfassigns` is NEGATED so a RISE in self-referential writes reads
    as a decrease, under the same "any component fell" rule as everything else.
    """
    src = "\n".join(l for l in text.splitlines() if not l.strip().startswith("--"))
    selfassign = sum(1 for m in SELF_ASSIGN.finditer(src) if m.group(2) != "__index")
    return (
        len(IF_RE.findall(src)),
        len(CALL_RE.findall(src)),
        len(LOOP_RE.findall(src)),
        len(ASSIGN_RE.findall(src)),
        -selfassign,
    )


def main(control: str, patched: str) -> int:
    a_dir, b_dir = pathlib.Path(control), pathlib.Path(patched)
    losers, changed = [], 0
    for f in sorted(b_dir.glob("*.lua")):
        a = a_dir / f.name
        if not a.exists():
            continue
        ta = a.read_text(encoding="utf-8", errors="replace")
        tb = f.read_text(encoding="utf-8", errors="replace")
        if ta == tb:
            continue
        changed += 1
        ca, cb = counts(ta), counts(tb)
        delta = tuple(y - x for x, y in zip(ca, cb))
        if any(d < 0 for d in delta):
            losers.append((f.name, delta))

    losers.sort(key=lambda r: r[1][1])
    print(f"{changed} files changed, {len(losers)} LOSE structure")
    for name, d in losers[:25]:
        print(
            f"  {name[:50]:<52} if {d[0]:+4}  calls {d[1]:+5}"
            f"  loops {d[2]:+4}  assign {d[3]:+5}  selfassign {-d[4]:+3}"
        )
    if len(losers) > 25:
        print(f"  ... and {len(losers) - 25} more")
    if losers:
        print("\nREJECT: a structuring change must lose nothing.")
        return 1
    print("\nOK: no file lost an if, a call, a loop or an assignment,")
    print("    and none gained a self-referential write.")
    return 0


if __name__ == "__main__":
    if len(sys.argv) != 3:
        print(__doc__)
        sys.exit(2)
    sys.exit(main(sys.argv[1], sys.argv[2]))
