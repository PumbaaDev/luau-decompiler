#!/usr/bin/env python3
"""Oracle extensions: the OTHER defect families, same contract as the existing
oracles (take decompiled Luau source text, return deterministic findings).

Ports the measured rules from crates/luau-core/src/decompiler/semantic_check.rs
so the Python audit works on single files, old runs, and server/wasm output
that never got a SEMANTIC_REPORT.tsv. The Rust checker remains ground truth;
divergence between the two is a bug here, not there.

Corpus-measured 24 Aug 2026 on run_20260822_102631 (1,323 files), reproduced by
the __main__ self-test below:
  truncated_bodies             2 files   (rc_0047 10 protos/0 fns, rc_0055 7/0)
  dropped_calls_header        11 files   (lifter-computed, zero-FP)
  dead_store_discarded_branch  2 files   (lv_0452 Workspace5:54, lv_0919 new11:667 - both hand-verified TP)
  dead_store_constructor_stub  1 file    (lv_0026 setmetatable drop)

NOT here on purpose: name_collision. 4,193 suffixed-sibling assignments across
581 files (44% of the corpus) - the pattern IS the emitter's house style, and
"unrelated values" is not decidable from text. A deterministic detector for it
would be a false-positive machine. See name_suffix_density() for the metric.

IMPORTANT: run these BEFORE strip_header(); two detectors read the comment
headers the decompiler writes into the file.
"""
from __future__ import annotations

import os
import re
import sys
from pathlib import Path
from typing import NamedTuple

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from decompiler_audit import strip_lua_noise, parse_protos


class Finding(NamedTuple):
    kind: str          # "truncated_body" | "dropped_call" | "dead_store"
    anchor: str        # identifier to build model context around ("" = file-level)
    line: int | None   # 1-based, None = file-level
    detail: str        # human-readable evidence


# -- truncated_body ---------------------------------------------------------
# Port of check_body_recovered (semantic_check.rs:862). The "-- Protos:" header
# is the lifter's own count; if the text declares under a quarter of them, most
# bodies were discarded. Counts the `function` KEYWORD on the ORIGINAL source
# (word-boundary), exactly like the Rust - the thresholds were verified against
# that counting, and blanked-source counting can only lower the count, i.e.
# push toward MORE findings, which is the wrong direction to drift.

_FUNC_KW = re.compile(r"\bfunction\b")


def truncated_bodies(src: str) -> list[Finding]:
    protos = None
    for ln in src.splitlines()[:20]:
        total, _main = parse_protos(ln.strip())
        if total is not None:
            protos = total
            break
    if protos is None or protos < 5:          # expected = protos-1 must be >= 4
        return []
    emitted = len(_FUNC_KW.findall(src))
    expected = protos - 1                     # main proto is not a declaration
    if emitted * 4 < expected:
        return [Finding(
            "truncated_body", "", None,
            "chunk has %d protos but the output contains only %d `function` "
            "keyword(s) - most function bodies were discarded" % (protos, emitted))]
    return []


# -- dropped_call -----------------------------------------------------------
# The lifter compares bytecode call count against the emitted AST and writes
# the deficit into the header ONLY when there is one (lib.rs:936). Header
# presence = defect; absence = clean. This is the one family whose ground
# truth cannot be derived from the text, so the header IS the oracle.

_CALLREC = re.compile(
    r"^--\s*call recovery:\s*(\d+)\s+proto\(s\)\s+emitted\s+(\d+)\s+fewer call\(s\)")


def dropped_calls_header(src: str) -> list[Finding]:
    for ln in src.splitlines()[:20]:
        m = _CALLREC.match(ln.strip())
        if m:
            protos, calls = int(m.group(1)), int(m.group(2))
            if protos == 0:
                return []
            return [Finding(
                "dropped_call", "", None,
                "%d function(s) emitted %d fewer call(s) than their bytecode "
                "contains" % (protos, calls))]
    return []


def zero_call_functions(src: str) -> list[str]:
    """CONTEXT HELPER, not a detector: names of declared functions whose body
    contains no call expression. When dropped_calls_header fires, these are the
    candidate short protos to hand the model as focused context. Never report
    these as defects on their own - a getter with no calls is normal code."""
    clean = strip_lua_noise(src)
    lines = clean.splitlines()
    out: list[str] = []
    decl = re.compile(
        r"^(?:local\s+)?function\s+([\w.:]+)\s*\(|^(?:local\s+)?([\w.]+)\s*=\s*function\s*\(")
    call = re.compile(r"[\w\)\]]\s*\(|:\s*\w+\s*\(|\{")   # f( / obj:m( / f{
    i = 0
    while i < len(lines):
        m = decl.match(lines[i].strip())
        if not m:
            i += 1
            continue
        name = m.group(1) or m.group(2)
        depth, j, has_call = 1, i + 1, False
        while j < len(lines) and depth > 0:
            t = lines[j].strip()
            depth += len(re.findall(r"\b(?:function|if|for|while|do)\b", t))
            depth -= len(re.findall(r"\bend\b", t))
            if call.search(t):
                has_call = True
            j += 1
        if not has_call:
            out.append(name)
        i += 1
    return out


# -- dead_store -------------------------------------------------------------
# Port of check_branch_local_discarded (semantic_check.rs:228). NOT a generic
# "assigned, never read" rule - that was measured in Rust at 1,161 candidates
# vs 162 with the extends-a-base condition, and a generic dead store whose RHS
# is a call still EXECUTES (side effects), so it is readability, not loss.
# All four conditions are needed:
#   (1) the local is mentioned exactly once in the whole file (its declaration)
#   (2) its initialiser EXTENDS a base identifier (base.. / base+ / base* / base/)
#   (3) that base is still read AFTER this line
#   (4) the identical RHS text is not recomputed anywhere else (exact-text on
#       purpose: a looser match suppresses real losses)

_IDENT = re.compile(r"\b[A-Za-z_]\w*\b")
_LUA_KW = frozenset(
    "and break do else elseif end false for function goto if in local nil not "
    "or repeat return then true until while".split())


def dead_store_discarded_branch(src: str) -> list[Finding]:
    clean = strip_lua_noise(src)
    lines = clean.splitlines()

    occ: dict[str, list[int]] = {}
    for i, raw in enumerate(lines):
        t = raw.strip()
        if t.startswith("--"):
            continue
        for tok in _IDENT.findall(t):
            if tok not in _LUA_KW:
                occ.setdefault(tok, []).append(i)

    found: list[Finding] = []
    for i, raw in enumerate(lines):
        t = raw.strip()
        if not t.startswith("local "):
            continue
        rest = t[len("local "):]
        eq = rest.find("=")
        if eq < 0:
            continue
        if rest[eq:eq + 2] == "==" or (eq > 0 and rest[eq - 1] in "~<>="):
            continue                                     # comparison, not assignment
        name = rest[:eq].strip()
        if not name or not all(c.isalnum() or c == "_" for c in name):
            continue                                     # multi-target lines excluded
        if len(occ.get(name, ())) != 1:                  # (1) sole mention
            continue
        rhs = rest[eq + 1:].lstrip().lstrip("(")
        m = re.match(r"[A-Za-z_]\w*", rhs)
        if not m:
            continue
        base = m.group(0)
        after = rhs[m.end():].lstrip()
        # NOTE tuple membership, not `in "+*/"`: an empty `after` is a
        # substring of every string and would silently pass the check.
        if not (after.startswith("..") or after[:1] in ("+", "*", "/")):
            continue                                     # (2) extends the base
        if not any(j > i for j in occ.get(base, ())):    # (3) base outlives line
            continue
        rhs_norm = " ".join(rhs.split())
        if rhs_norm and any(                             # (4) not recomputed
                j != i and not other.strip().startswith("--")
                and rhs_norm in " ".join(other.split())
                for j, other in enumerate(lines)):
            continue
        found.append(Finding(
            "dead_store", name, i + 1,
            "`%s` extends `%s` and is never read, while `%s` is used again "
            "below - the branch arm's value was written to a fresh local "
            "instead of back to `%s`, so it is silently dropped" % (name, base, base, base)))
    return found


# Port of check_discarded_table_writes (semantic_check.rs:916): a generated-name
# table literal built inside a `for` and never read - a SETTABLE whose target
# register was lost, every iteration's write discarded.

def dead_store_discarded_table(src: str) -> list[Finding]:
    lines = src.splitlines()
    depth_for = 0
    found: list[Finding] = []
    gen = re.compile(r"^(?:tbl|upval_|cap_|field_)")
    for i, raw in enumerate(lines):
        t = raw.strip()
        if t.startswith("for ") and t.endswith(" do"):
            depth_for += 1
        elif t == "end" and depth_for > 0:
            depth_for -= 1
        if depth_for == 0 or not t.startswith("local "):
            continue
        rest = t[len("local "):]
        if "=" not in rest:
            continue
        name, tail = rest.split("=", 1)
        name = name.strip()
        if not tail.strip().startswith("{") or not gen.match(name):
            continue
        uses = sum(1 for j, l in enumerate(lines) if j != i and name in l)
        if uses == 0:
            found.append(Finding(
                "dead_store", name, i + 1,
                "`%s` is built inside a loop and never read - a table "
                "assignment whose target was lost, every write discarded" % name))
    return found


# The one crisp truncated-body/dropped-result hybrid worth its own signature:
# a constructor that builds the metatable object, throws it away, and returns
# a fresh empty table. Hand-verified on lv_0026 BeequipCaseEntry.

_SETMETA_STMT = re.compile(r"^setmetatable\(\s*\{\s*\}\s*,\s*[A-Za-z_]\w*\s*\)$")


def dead_store_constructor_stub(src: str) -> list[Finding]:
    lines = [l.strip() for l in strip_lua_noise(src).splitlines()]
    found: list[Finding] = []
    for i in range(len(lines) - 1):
        if _SETMETA_STMT.match(lines[i]) and lines[i + 1] == "return {}":
            found.append(Finding(
                "dead_store", "setmetatable", i + 1,
                "the `setmetatable({}, ...)` result is discarded and a fresh "
                "empty table returned - the constructor returns an object with "
                "no metatable and no fields"))
    return found


# -- name_collision: metric only, deliberately NOT a detector ---------------

def name_suffix_density(src: str) -> tuple[int, int]:
    """(suffixed-sibling assignments, total locals). A per-run trend number for
    the readability defect - NEVER a per-instance defect list. Measured: the
    pattern appears 4,193 times across 581/1,323 files because it is the
    emitter's standard idiom for method-call sequences; flagging it flags the
    house style, and 'two UNRELATED values' is not decidable from text."""
    clean = strip_lua_noise(src)
    sib = re.compile(r"^\s*local\s+([A-Za-z_]\w*?)\d+\s*=\s*([A-Za-z_]\w*?)\d+\b")
    n_sib = n_loc = 0
    for ln in clean.splitlines():
        if ln.strip().startswith("local "):
            n_loc += 1
            m = sib.match(ln)
            if m and m.group(1) == m.group(2):
                n_sib += 1
    return n_sib, n_loc


# -- combined entry point ---------------------------------------------------

def oracle_extra(src: str) -> list[Finding]:
    """All extended-family findings, file-level first, then by line."""
    out = (truncated_bodies(src) + dropped_calls_header(src)
           + dead_store_discarded_branch(src) + dead_store_discarded_table(src)
           + dead_store_constructor_stub(src))
    return sorted(out, key=lambda f: (f.line is not None, f.line or 0, f.kind))


# -- self-test: reproduce the measured corpus counts ------------------------
# `python tools/audit_bench/oracle_extra.py` runs the detectors over the frozen
# run dir and asserts the hand-verified counts. A drift here means a detector
# changed behaviour and must be re-verified against the Rust ground truth.

_EXPECT = {"truncated_body": 2, "dropped_call": 11, "dead_store": 3}
_RUN = (Path(__file__).resolve().parent.parent.parent
        / "target/union1324_out/run_20260822_102631")


def _selftest() -> int:
    if not _RUN.exists():
        print("run dir missing, cannot self-test: %s" % _RUN)
        return 2
    counts = {"truncated_body": 0, "dropped_call": 0, "dead_store": 0}
    hits: dict[str, list[str]] = {k: [] for k in counts}
    ncollide = ncollide_files = 0
    for p in sorted(_RUN.glob("*.lua")):
        src = p.read_text(encoding="utf-8", errors="replace")
        for f in oracle_extra(src):
            counts[f.kind] += 1
            hits[f.kind].append("%s%s" % (p.name[:44], "" if f.line is None else ":%d" % f.line))
        sib, _ = name_suffix_density(src)
        if sib:
            ncollide += sib
            ncollide_files += 1
    ok = True
    for k, want in _EXPECT.items():
        got = counts[k]
        flag = "OK" if got == want else "DRIFT"
        if got != want:
            ok = False
        print("  %-15s %2d  (want %d)  %s" % (k, got, want, flag))
        for h in hits[k]:
            print("       ", h)
    print("  name_collision  %d pairs / %d files  (metric only, not a detector)"
          % (ncollide, ncollide_files))
    print("\n%s" % ("ALL COUNTS MATCH" if ok else "COUNT DRIFT - re-verify against Rust ground truth"))
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(_selftest())
