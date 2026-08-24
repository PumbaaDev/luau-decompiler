#!/usr/bin/env python3
"""Corpus-scale audit: does the tested find_undefined_registers hold on 1,324
REAL decompiled files, and how does it relate to the decompiler's own count of 9?

TWO DIFFERENT DEFECTS, kept separate on purpose:

  A. USED-NEVER-DECLARED  - what tools/decompiler_audit.find_undefined_registers
     measures: a vN referenced with no `local` for it anywhere. This is the
     function that was unit-tested to 8/8 on split_locals etc.

  B. DECLARED-NEVER-ASSIGNED - the decompiler's own `undefined_register` metric
     (CLAUDE.md: "a `local vN` declared, used, never assigned", measured at 9 in
     8 files). A vN that gets a `local vN` slot but is never written, so it reads
     nil. Reproduced here to check the ground-truth number, NOT to claim it is the
     same thing as A.

Both run over comment/string-blanked source (strip_lua_noise), so neither counts a
register named only in a comment or string. Deterministic, no model.

    python tools/corpus_undefined_audit.py [run_dir]
"""
from __future__ import annotations

import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from decompiler_audit import find_undefined_registers, strip_lua_noise

DEFAULT_DIR = "target/union1324_out/run_20260822_102631"

_REG = re.compile(r"\bv(\d+)\b")
# a bare `local vN` or `local vN, vM` declaration line (no `=` assigning them)
_LOCAL_NAMES = re.compile(r"^local\s+(?:function\s+)?([A-Za-z_]\w*(?:\s*,\s*[A-Za-z_]\w*)*)")
_ASSIGN = re.compile(r"^([A-Za-z_][\w.\[\]]*(?:\s*,\s*[A-Za-z_][\w.\[\]]*)*)\s*=")


def declared_never_assigned(text: str) -> list[str]:
    """Reproduce the decompiler's `undefined_register` shape.

    A vN that is (1) declared by a `local`, (2) used somewhere, and (3) never
    appears on the left-hand side of an assignment nor is given a value on its
    own `local vN = ...` line.
    """
    clean = strip_lua_noise(text)
    declared_bare: set[str] = set()      # `local vN` with no value on that line
    assigned: set[str] = set()           # got a value via `local vN =` or `vN =`
    used: set[str] = set()

    for line in clean.splitlines():
        s = line.strip()

        m = _LOCAL_NAMES.match(s)
        if m and not s.startswith("local function"):
            names = [n.strip() for n in m.group(1).split(",")]
            reg_names = [n for n in names if _REG.fullmatch(n)]
            # does this local line ALSO assign (has `=`)?
            if "=" in s:
                # names left of `=` that get a value
                assigned.update(reg_names)
            else:
                declared_bare.update(reg_names)

        # plain assignment target:  vN = ...   or  a, vN = ...
        am = _ASSIGN.match(s)
        if am:
            for tgt in am.group(1).split(","):
                tgt = tgt.strip()
                if _REG.fullmatch(tgt):
                    assigned.add(tgt)

        used.update("v" + d for d in _REG.findall(line))

    # declared bare, used, and never assigned anywhere
    bad = [r for r in declared_bare if r in used and r not in assigned]
    return sorted(bad, key=lambda r: int(r[1:]))


def main():
    run_dir = sys.argv[1] if len(sys.argv) > 1 else DEFAULT_DIR
    files = []
    for root, _, fs in os.walk(run_dir):
        for fn in fs:
            if fn.endswith(".lua"):
                files.append(os.path.join(root, fn))
    files.sort()
    print("corpus: %d .lua files under %s\n" % (len(files), run_dir))

    a_files, a_total = [], 0     # used-never-declared
    b_files, b_total = [], 0     # declared-never-assigned (the "9")
    errors = 0

    for p in files:
        try:
            with open(p, "r", encoding="utf-8", errors="replace") as f:
                src = f.read()
        except Exception:
            errors += 1
            continue
        a = find_undefined_registers(src)
        b = declared_never_assigned(src)
        if a:
            a_files.append((os.path.basename(p), a))
            a_total += len(a)
        if b:
            b_files.append((os.path.basename(p), b))
            b_total += len(b)

    print("=" * 74)
    print("B. DECLARED-NEVER-ASSIGNED  (the decompiler's own metric, expected ~9)")
    print("=" * 74)
    print("  %d occurrences across %d files\n" % (b_total, len(b_files)))
    for name, regs in b_files:
        print("    %-52s %s" % (name[:52], regs))

    print("\n" + "=" * 74)
    print("A. USED-NEVER-DECLARED  (find_undefined_registers, the tested function)")
    print("=" * 74)
    print("  %d occurrences across %d files" % (a_total, len(a_files)))
    if len(a_files) <= 40:
        for name, regs in a_files:
            print("    %-52s %s" % (name[:52], regs[:12]))
    else:
        for name, regs in a_files[:40]:
            print("    %-52s %s" % (name[:52], regs[:12]))
        print("    ... and %d more files" % (len(a_files) - 40))

    print("\n" + "-" * 74)
    print("errors reading files: %d" % errors)
    print("SUMMARY:  declared-never-assigned=%d/%d files   used-never-declared=%d/%d files"
          % (b_total, len(b_files), a_total, len(a_files)))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
