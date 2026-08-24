#!/usr/bin/env python3
"""Decompiler-output audit helpers, corrected.

These three functions came out of the model bake-off in `bench_all_uncensored.py`.
Qwen3-Coder-30B-A3B-abliterated's versions are recorded here with what was wrong,
because the failure shapes are the useful part.

  parse_protos              FAILED the bench. Two bugs, one line apart.
  find_undefined_registers  PASSED the bench and is the most dangerous of the
                            three: four latent bugs, every one producing a FALSE
                            POSITIVE. For a tool whose whole job is auditing, a
                            false positive is worse than a crash, and it is the
                            exact failure three separate models already produced
                            on this project.
  strip_header              Essentially correct. Two small fidelity issues.
"""
from __future__ import annotations

import re

# -- parse_protos -----------------------------------------------------------
#
# Qwen wrote:
#     rest = line[12:]                    # len("-- Protos: ")   <- it is 11
#     if part.startswith("total="):       # format is "2 total", not "total=2"
#
# Bug 1: an off-by-one it documented as correct. line[12:] drops the digit
#        entirely, so the value is gone before parsing starts.
# Bug 2: it assumed both fields were key=value. Only `main=1` is; `2 total`
#        puts the value first. So main parsed and total silently stayed None,
#        returning (None, 1) - half an answer a caller would happily accept.
#
# One anchored regex removes the arithmetic that made both bugs possible.
_PROTOS_RE = re.compile(
    r"^\s*--\s*Protos:\s*(\d+)\s+total\s*,\s*main\s*=\s*(\d+)\s*$"
)


def parse_protos(line: str) -> tuple[int | None, int | None]:
    """Parse `-- Protos: 2 total, main=1` into (total, main).

    Returns (None, None) if the line does not match. Never returns a partial
    tuple: either both values are ints or both are None.
    """
    if not isinstance(line, str):
        return (None, None)
    m = _PROTOS_RE.match(line)
    if not m:
        return (None, None)
    return (int(m.group(1)), int(m.group(2)))


# -- Lua noise stripping ----------------------------------------------------

_LONG_BRACKET_OPEN = re.compile(r"(?<!-)-{0,2}\[(=*)\[")


def _blank_out(src: str, start: int, end: int) -> str:
    """Replace src[start:end] with spaces, preserving newlines and length.

    Length and line structure are preserved so that anything computed on the
    stripped text still lines up with the original.
    """
    chunk = "".join("\n" if c == "\n" else " " for c in src[start:end])
    return src[:start] + chunk + src[end:]


def strip_lua_noise(src: str) -> str:
    """Blank out Lua comments and string literals, keeping length and lines.

    This is what Qwen's `find_undefined_registers` was missing. Scanning raw
    text means `-- v99 is broken` and `local s = "v42"` both register v99/v42
    as USED, so a register mentioned only in a comment gets reported as an
    undefined defect. That is an invented defect, not a found one.

    Not a full Lua lexer. It handles line comments, long comments/strings with
    any `=` level, and quoted strings with backslash escapes, which covers
    decompiler output.
    """
    out = src
    i = 0
    n = len(out)
    while i < n:
        c = out[i]

        # long bracket, optionally preceded by -- making it a long comment
        if c == "[" or (c == "-" and out.startswith("--", i)):
            m = _LONG_BRACKET_OPEN.match(out, i)
            if m:
                level = m.group(1)
                close = "]" + level + "]"
                end = out.find(close, m.end())
                end = n if end == -1 else end + len(close)
                out = _blank_out(out, i, end)
                i = end
                continue

        # line comment
        if out.startswith("--", i):
            end = out.find("\n", i)
            end = n if end == -1 else end
            out = _blank_out(out, i, end)
            i = end
            continue

        # quoted string
        if c in ("'", '"'):
            j = i + 1
            while j < n:
                if out[j] == "\\":
                    j += 2
                    continue
                if out[j] == c or out[j] == "\n":
                    break
                j += 1
            end = min(j + 1, n)
            out = _blank_out(out, i, end)
            i = end
            continue

        i += 1
    return out


# -- find_undefined_registers ----------------------------------------------
#
# Qwen's version PASSED the bench, which is why it is worth reading closely.
# It split each line on whitespace and only accepted a declaration when a
# whitespace-separated token was exactly `vN`. Four consequences, all FALSE
# POSITIVES (a register reported as undefined when it is fine):
#
#   1. `local v1,v2,v3`      no spaces after the commas, so split() yields the
#                            single token "v1,v2,v3", nothing is declared, and
#                            all three are reported undefined.
#   2. `local function v3()` the token is "v3()", never matches, so v3 is
#                            reported undefined.
#   3. `-- v99 is broken`    a comment counts as a use.
#   4. `local s = "v42"`     a string literal counts as a use.
#
# It also did `import re` inside the per-line loop.

_REGISTER_RE = re.compile(r"\bv(\d+)\b")
# `local a, b, c` / `local a,b,c` / `local function f` / `local a = ...`
_LOCAL_DECL_RE = re.compile(r"^local\s+(?:function\s+)?([A-Za-z_][\w]*(?:\s*,\s*[A-Za-z_][\w]*)*)")
# `for k, v in pairs(t)` / `for v in next, t` - the control vars are bindings
_FOR_IN_RE = re.compile(r"^for\s+([A-Za-z_][\w]*(?:\s*,\s*[A-Za-z_][\w]*)*)\s+in\b")
# `for i = 1, n do` / `for i = 1, n, 2 do` - numeric-for control var is a binding
_FOR_NUM_RE = re.compile(r"^for\s+([A-Za-z_][\w]*)\s*=")
# `function foo(a, b, v3)` / `function(v2)` - parameters are bindings
_FUNC_PARAMS_RE = re.compile(r"\bfunction\b[^(]*\(([^)]*)\)")


def find_undefined_registers(text: str) -> list[str]:
    """Return vN identifiers used in `text` but never bound anywhere.

    A binding is a `local`, a for-loop control variable (`for k, v in ...` or
    `for v = 1, n`), or a function parameter. Comments and string literals are
    ignored for both uses and bindings. Sorted by numeric part ascending.

    CORPUS-MEASURED 23 Aug 2026: without the for-loop and parameter cases this
    reported 172 defects on target/union1324, of which 169 (98%) were valid
    `for k, v in pairs(t)` loop variables. An audit tool that invents 169 defects
    is worse than useless, so those binding forms are not optional.
    """
    if not text:
        return []

    clean = strip_lua_noise(text)

    declared: set[str] = set()
    used: set[str] = set()

    def add_names(blob: str) -> None:
        for name in blob.split(","):
            name = name.strip()
            if _REGISTER_RE.fullmatch(name):
                declared.add(name)

    for line in clean.splitlines():
        stripped = line.strip()

        m = _LOCAL_DECL_RE.match(stripped)
        if m:
            add_names(m.group(1))

        fm = _FOR_IN_RE.match(stripped)
        if fm:
            add_names(fm.group(1))

        fn = _FOR_NUM_RE.match(stripped)
        if fn:
            add_names(fn.group(1))

        for params in _FUNC_PARAMS_RE.findall(line):
            add_names(params)

        used.update("v" + d for d in _REGISTER_RE.findall(line))

    return sorted(used - declared, key=lambda r: int(r[1:]))


# -- strip_header -----------------------------------------------------------
#
# Qwen's version was essentially correct. Two small things:
#   1. `'\n'.join(...) if i < len(lines) else ''` - the guard is dead code,
#      joining an empty slice already yields ''.
#   2. splitlines() + '\n'.join() silently normalises CRLF to LF and drops a
#      trailing newline. Harmless in a bench, not harmless in a file rewriter.


def strip_header(text: str, keep_line_endings: bool = False) -> str:
    """Remove the leading block of `--` comment lines and the blanks after it.

    Only the LEADING block goes; later comments are kept. With
    keep_line_endings=True the original CRLF/LF endings and any trailing
    newline survive the round trip.
    """
    if not text:
        return text

    lines = text.splitlines(keepends=True) if keep_line_endings else text.splitlines()

    i = 0
    while i < len(lines) and lines[i].strip().startswith("--"):
        i += 1
    while i < len(lines) and lines[i].strip() == "":
        i += 1

    return "".join(lines[i:]) if keep_line_endings else "\n".join(lines[i:])
