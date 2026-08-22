#!/usr/bin/env python3
"""Count STRUCTURE, not defects, between two decompiler run directories.

The semantic checker has no check for a deleted `if`. A region-structuring
change can therefore delete guards and SCORE BETTER: measured 22 Aug 2026, one
such change read "fixed 1, broke 0, 33 -> 32 defects" while five files lost an
`if` and `lv_0910_VRCamera` lost three guards outright, so a body that was
gated on `arg1 == "offtrack"` ran unconditionally.

A second change read -28 files by the semantic score while 146 files lost
structure, the worst dropping 208 calls, 63 ifs and 27 loops.

So: never accept a structuring change on the semantic score alone. Run this,
and require ZERO losses.

    python tools/structure_diff.py <control_run_dir> <patched_run_dir>
"""
import pathlib
import re
import sys


def counts(text: str):
    """(ifs, calls, loops) ignoring header comments."""
    src = "\n".join(l for l in text.splitlines() if not l.strip().startswith("--"))
    return (
        len(re.findall(r"^\s*if .+ then\s*$", src, re.M)),
        len(re.findall(r"\w\s*\(", src)),
        len(re.findall(r"^\s*(?:for|while) .+ do\s*$", src, re.M)),
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
        print(f"  {name[:64]:<66} if {d[0]:+4}  calls {d[1]:+5}  loops {d[2]:+4}")
    if len(losers) > 25:
        print(f"  ... and {len(losers) - 25} more")
    if losers:
        print("\nREJECT: a structuring change must lose nothing.")
        return 1
    print("\nOK: no file lost an if, a call or a loop.")
    return 0


if __name__ == "__main__":
    if len(sys.argv) != 3:
        print(__doc__)
        sys.exit(2)
    sys.exit(main(sys.argv[1], sys.argv[2]))
