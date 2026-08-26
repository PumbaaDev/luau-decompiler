#!/usr/bin/env python3
"""Massive regression suite: every trap Qwen3-Coder-30B-A3B has failed here.

Runs BASE (raw abliterated model, no system prompt) against ALIAS
(uncensored-coder, the same weights with the engineering checklist baked in) on
12 tasks and 84 executed cases.

Task provenance, so nothing is decoration:
  A. DOCUMENTED FAILURES - what the bake-off and the audit actually caught.
  B. GENERALISATION - written 23 Aug, base scored 8/12, alias 12/12.
  C. NEW TRAPS - same failure CLASSES, formats the model has never seen and that
     appear nowhere in the system prompt.

Grouped by model so each is loaded once, not once per task.

    python tools/bench_process_regression.py
"""
from __future__ import annotations

import json
import os
import re
import sys
import time
import urllib.error
import urllib.request

URL = "http://localhost:11434/api/generate"
# The control MUST be the same weights the ALIAS is built FROM, minus the system prompt -
# that is the only way the A/B isolates the prompt. This pointed at the Huihui-Coder base,
# which was DELETED to reclaim ~17GB when the model was swapped to Heretic (24 Aug), so every
# BASE task returned "GENERATE FAILED: HTTPError" and BASE scored 0/83 - a dead control that
# silently made every alias number look like an improvement. Repointed at the Heretic weights
# named in Modelfile.uncensored-coder's FROM line, which ARE on disk.
BASE = "hf.co/mradermacher/Qwen3-Coder-30B-A3B-Instruct-Heretic-i1-GGUF:Q4_K_M"
ALIAS = "uncensored-coder"
NUM_CTX = int(os.environ.get("BENCH_NUM_CTX", "16384"))
OPTIONS = {"num_ctx": NUM_CTX, "temperature": 0, "num_predict": 2600}
OUT = "bench_process_regression.json"

import os as _os
sys.path.insert(0, _os.path.dirname(_os.path.abspath(__file__)))
from safe_exec import run_case, define_ok  # noqa: E402
CASE_TIMEOUT = 10.0      # seconds per test case; every task here finishes in <0.01s
RESET_PER_TASK = os.environ.get("BENCH_NO_RESET", "") == ""   # set BENCH_NO_RESET=1 for a fast, NON-reproducible run
_OLLAMA = os.path.join(os.path.expanduser("~"), "AppData/Local/Programs/Ollama/ollama.exe")


def _reset_models():
    import subprocess
    for m in (ALIAS, BASE):
        try:
            subprocess.run([_OLLAMA, "stop", m], capture_output=True, timeout=60,
                           env=dict(os.environ, OLLAMA_MODELS=r"E:\ollama-models"))
        except Exception:
            pass


def _norm(v):
    """JSON-normalise so tuple/list differences from the subprocess pipe vanish."""
    if isinstance(v, (list, tuple)):
        return [_norm(x) for x in v]
    if isinstance(v, dict):
        return {str(k): _norm(x) for k, x in v.items()}
    return v

TASKS = [
    # ---------- A. DOCUMENTED FAILURES ----------
    {"name": "parse_protos", "fn": "parse_protos", "group": "A-documented",
     "why": "bench failure: wrote line[12:] when len is 11, and assumed total=2 "
            "when format is '2 total'. Returned (None, 1).",
     "prompt": ("Write a Python function parse_protos(line) that parses a line of the form\n"
                "  -- Protos: 2 total, main=1\nand returns a tuple (total, main) of ints. "
                "If the line does not match, return (None, None). Return only the function code."),
     "cases": [("-- Protos: 2 total, main=1", (2, 1)),
               ("-- Protos: 17 total, main=12", (17, 12)),
               ("-- Protos: 0 total, main=0", (0, 0)),
               ("  -- Protos: 5 total, main=3  ", (5, 3)),
               ("-- something else", (None, None)),
               ("-- Protos: x total, main=1", (None, None)),
               ("-- Protos: 2 total, main=1 EXTRA", (None, None)),
               ("", (None, None))]},

    {"name": "find_undefined_registers", "fn": "find_undefined_registers", "group": "A-documented",
     "why": "PASSED the bench then produced 6 false positives on ordinary Lua: "
            "commas without spaces, local function, comments, strings.",
     "prompt": ("Write a Python function find_undefined_registers(text) for auditing decompiler "
                "output. It scans Lua source text for identifiers matching v followed by one or "
                "more digits (e.g. v3, v7). It returns a sorted list (by numeric part ascending) "
                "of those USED somewhere but never declared. A declaration is a 'local' statement, "
                "including 'local v1, v2', 'local v1,v2' with no spaces, and 'local function v5'. "
                "Identifiers appearing inside Lua comments (-- to end of line) or inside string "
                "literals must NOT count as uses. Return only the function code."),
     "cases": [("local v3\nlocal x = v3 + v7\nreturn v7 .. v10", ["v7", "v10"]),
               ("local v1\nreturn v1", []),
               ("local v1,v2,v3\nreturn v1 + v2 + v3", []),
               ("local v1, v2, v3\nreturn v1 + v2 + v3", []),
               ("local function v5()\n  return 1\nend\nreturn v5()", []),
               ("local v1 = 1\n-- v99 was the old register\nreturn v1", []),
               ("local v1 = 1\nlocal s = \"v42 not a register\"\nreturn v1 .. s", []),
               ("local v1 = 1\nreturn v1 + v8", ["v8"]),
               ("return v2 + v11 + v1", ["v1", "v2", "v11"]),
               ("", [])]},

    {"name": "strip_header", "fn": "strip_header", "group": "A-documented",
     "why": "passed, but silently normalised CRLF to LF and dropped the trailing newline.",
     "prompt": ("Write a Python function strip_header(text) that removes the leading block of "
                "comment lines (stripped form starts with '--') from a Lua source string, along "
                "with any blank lines immediately after that block, and returns the rest. Only the "
                "LEADING block is removed; later comments are kept. Return only the function code."),
     "cases": [("-- a\n-- b\n\nlocal x = 1\n-- keep me\nreturn x", "local x = 1\n-- keep me\nreturn x"),
               ("local x = 1\nreturn x", "local x = 1\nreturn x"),
               ("-- only comments", ""),
               ("", ""),
               ("-- h\n\n\n\nlocal y = 2", "local y = 2"),
               ("  -- indented header\nlocal z = 3", "local z = 3")]},

    # ---------- B. GENERALISATION (already measured) ----------
    {"name": "parse_upval", "fn": "parse_upval", "group": "B-generalisation",
     "why": "base did index arithmetic; an optional trailing comment broke it.",
     "prompt": ("Write a Python function parse_upval(line) for a Luau bytecode listing.\n"
                "A line looks like:   UPVAL[3] = R7   -- captured from parent\n"
                "Return a tuple (upval_index, register) of ints, here (3, 7).\n"
                "The trailing '--' comment is optional and must be ignored.\n"
                "Leading and trailing whitespace is possible.\n"
                "If the line does not match this form, return (None, None).\n"
                "Never return a tuple where only one element is an int.\n"
                "Return only the function code."),
     "cases": [("UPVAL[3] = R7   -- captured from parent", (3, 7)),
               ("  UPVAL[12] = R0  ", (12, 0)),
               ("UPVAL[0] = R5", (0, 5)),
               ("GETUPVAL R1 U2", (None, None)),
               ("", (None, None)),
               ("UPVAL[x] = R7", (None, None))]},

    {"name": "count_real_calls", "fn": "count_real_calls", "group": "B-generalisation",
     "why": "base declared in_string/in_comment then returned 2 for every input, "
            "including one whose answer was 0.",
     "prompt": ("Write a Python function count_real_calls(src) that counts how many times\n"
                "the function name 'spawn' is actually CALLED in a Lua source string,\n"
                "meaning the identifier spawn followed by an opening parenthesis.\n"
                "Occurrences inside Lua comments (-- to end of line) and inside string\n"
                "literals (single or double quoted) must NOT be counted.\n"
                "Return an int. Return only the function code."),
     "cases": [("spawn(f)\nspawn(g)", 2),
               ("spawn(f)\n-- spawn(g) old code\nreturn 1", 1),
               ("local s = \"spawn(x)\"\nspawn(f)", 1),
               ("-- spawn(a)\n-- spawn(b)", 0),
               ("spawner(f)", 0),
               ("respawn(f)", 0),
               ("", 0)]},

    # ---------- C. NEW TRAPS, same classes ----------
    {"name": "parse_stack_hint", "fn": "parse_stack_hint", "group": "C-new-offbyone",
     "why": "OFF-BY-ONE trap: prefix '-- Stack: ' is 10 chars, tempting a miscount.",
     "prompt": ("Write a Python function parse_stack_hint(line) that parses lines of the form\n"
                "  -- Stack: 7 slots, top=4\n"
                "and returns a tuple (slots, top) of ints. If the line does not match, return\n"
                "(None, None). Never return a tuple with only one int. Return only the function code."),
     "cases": [("-- Stack: 7 slots, top=4", (7, 4)),
               ("-- Stack: 0 slots, top=0", (0, 0)),
               ("  -- Stack: 128 slots, top=99  ", (128, 99)),
               ("-- Stack: seven slots, top=4", (None, None)),
               ("-- Registers: 7 slots, top=4", (None, None)),
               ("", (None, None))]},

    {"name": "parse_jump", "fn": "parse_jump", "group": "C-new-asymmetry",
     "why": "FORMAT ASYMMETRY: one field positional, one key=value, one bracketed.",
     "prompt": ("Write a Python function parse_jump(line) parsing a Luau disassembly line:\n"
                "  JUMPIF R3 -> [42] offset=-7\n"
                "Return a dict {'reg': 3, 'target': 42, 'offset': -7} with int values.\n"
                "The offset may be negative. If the line does not match, return None.\n"
                "Return only the function code."),
     "cases": [("JUMPIF R3 -> [42] offset=-7", {"reg": 3, "target": 42, "offset": -7}),
               ("JUMPIF R0 -> [0] offset=0", {"reg": 0, "target": 0, "offset": 0}),
               ("JUMPIF R15 -> [200] offset=12", {"reg": 15, "target": 200, "offset": 12}),
               ("JUMPIF R3 -> 42 offset=-7", None),
               ("NOP", None),
               ("", None)]},

    {"name": "split_locals", "fn": "split_locals", "group": "C-new-realsyntax",
     "why": "REAL SYNTAX: must handle spaces/no-spaces, 'local function', and '=' cutoff.",
     "prompt": ("Write a Python function split_locals(line) that takes ONE line of Lua and returns\n"
                "the list of local variable names it declares, in order. Handle:\n"
                "  'local a'            -> ['a']\n"
                "  'local a, b, c'      -> ['a','b','c']\n"
                "  'local a,b,c'        -> ['a','b','c']\n"
                "  'local x = 1'        -> ['x']\n"
                "  'local function f'   -> ['f']\n"
                "  anything else        -> []\n"
                "Names on the right-hand side of '=' are NOT declarations.\n"
                "Return only the function code."),
     "cases": [("local a", ["a"]),
               ("local a, b, c", ["a", "b", "c"]),
               ("local a,b,c", ["a", "b", "c"]),
               ("local x = 1", ["x"]),
               ("local function f", ["f"]),
               ("  local p, q = 1, 2  ", ["p", "q"]),
               ("return a", []),
               ("", [])]},

    {"name": "strip_block_comments", "fn": "strip_block_comments", "group": "C-new-comments",
     "why": "COMMENTS/STRINGS: Lua long comments --[[ ]] and levelled --[==[ ]==].",
     "prompt": ("Write a Python function strip_block_comments(src) that removes Lua block comments\n"
                "from a source string and returns the rest. A block comment starts with --[[ and\n"
                "ends with ]], and may also use any number of equals signs: --[==[ ... ]==].\n"
                "Line comments starting with -- must be left alone. Return only the function code."),
     "cases": [("local x = 1 --[[ hidden ]] + 2", "local x = 1  + 2"),
               ("--[==[ a ]==]local y = 2", "local y = 2"),
               ("local z = 3 -- keep this", "local z = 3 -- keep this"),
               ("no comments here", "no comments here"),
               ("", "")]},

    {"name": "safe_div_report", "fn": "safe_div_report", "group": "C-new-failclosed",
     "why": "FAIL CLOSED: an unknown must not fall through to a permissive answer. "
            "This is the recurring defect shape across the whole project.",
     "prompt": ("Write a Python function safe_div_report(passed, total) that returns a percentage\n"
                "string like '85.0%'. Rules, follow exactly:\n"
                "  - if total is 0, return 'n/a' (never 0% and never a crash)\n"
                "  - if either argument is None, return 'n/a'\n"
                "  - if either argument is negative, return 'n/a'\n"
                "  - if passed > total, return 'n/a'\n"
                "  - otherwise return the percentage to one decimal place with a % sign\n"
                "Return only the function code."),
     "cases": [((17, 20), "85.0%"),
               ((0, 20), "0.0%"),
               ((20, 20), "100.0%"),
               ((5, 0), "n/a"),
               ((0, 0), "n/a"),
               ((None, 20), "n/a"),
               ((17, None), "n/a"),
               ((-1, 20), "n/a"),
               ((21, 20), "n/a")]},

    {"name": "dedupe_defects", "fn": "dedupe_defects", "group": "C-new-falsepositive",
     "why": "FALSE POSITIVE direction: dedupe must not drop distinct defects, and "
            "must not keep duplicates. Over-reporting is the expensive direction.",
     "prompt": ("Write a Python function dedupe_defects(rows) taking a list of dicts, each with\n"
                "keys 'file', 'line' and 'kind'. Return a new list with duplicates removed, where\n"
                "two rows are duplicates only if all three of file, line and kind are equal.\n"
                "Preserve the order of first appearance. Do not mutate the input.\n"
                "Return only the function code."),
     "cases": [(([{"file": "a", "line": 1, "kind": "x"}, {"file": "a", "line": 1, "kind": "x"}],),
                [{"file": "a", "line": 1, "kind": "x"}]),
               (([{"file": "a", "line": 1, "kind": "x"}, {"file": "a", "line": 2, "kind": "x"}],),
                [{"file": "a", "line": 1, "kind": "x"}, {"file": "a", "line": 2, "kind": "x"}]),
               (([{"file": "a", "line": 1, "kind": "x"}, {"file": "a", "line": 1, "kind": "y"}],),
                [{"file": "a", "line": 1, "kind": "x"}, {"file": "a", "line": 1, "kind": "y"}]),
               (([],), [])]},

    {"name": "parse_version", "fn": "parse_version", "group": "C-new-zero",
     "why": "ZERO/FALSY trap: version 0 is legal, so `if not major` is wrong.",
     "prompt": ("Write a Python function parse_version(s) that parses 'v9.0.3' into a tuple of\n"
                "three ints (9, 0, 3). The leading 'v' is required. Every component is a\n"
                "non-negative integer and zero is legal. If it does not match exactly this shape,\n"
                "return None. Return only the function code."),
     "cases": [("v9.0.3", (9, 0, 3)),
               ("v0.0.0", (0, 0, 0)),
               ("v10.20.30", (10, 20, 30)),
               ("9.0.3", None),
               ("v9.0", None),
               ("v9.0.3.1", None),
               ("vx.0.3", None),
               ("", None)]},
]


def generate(model, prompt):
    payload = {"model": model, "prompt": prompt, "stream": False, "options": OPTIONS}
    for think in (False, None):          # qwen3-coder crashes if `think` is sent
        body = dict(payload)
        if think is not None:
            body["think"] = think
        req = urllib.request.Request(URL, data=json.dumps(body).encode(),
                                     headers={"Content-Type": "application/json"})
        try:
            with urllib.request.urlopen(req, timeout=2400) as r:
                return json.load(r)
        except urllib.error.HTTPError as e:
            if e.code == 400 and think is False:
                continue
            raise
    raise RuntimeError("unreachable")


def extract_code(text, fn_name=None):
    """Pick the fenced block that actually DEFINES the wanted function.

    `blocks[-1]` was the old rule and it silently penalised verbose models. A
    model that shows its working emits several fences (trace, code, then a worked
    example), so the LAST fence is often sample data, not the function. MEASURED
    23 Aug 2026: dedupe_defects scored 0/4 purely because the extractor grabbed a
    trailing list-of-dicts example. The model's actual function was never run.
    """
    text = re.sub(r"<think>.*?</think>", "", text, flags=re.S)
    blocks = re.findall(r"```(?:python)?\s*\n(.*?)```", text, re.S)
    # A captured block can itself START with a fence marker when the model nests
    # or leaves a fence unbalanced: the regex spans two fences and line 1 of the
    # "code" is ```python, an instant SyntaxError scored as "function not
    # defined". MEASURED 23 Aug 2026 (ITER 6): strip_block_comments 0/5 with a
    # perfectly good function inside. Strip any stray fence line from every block.
    blocks = [_defence(b) for b in blocks]
    if not blocks:
        return _defence(text)
    if fn_name:
        named = [b for b in blocks if re.search(r"^\s*def\s+%s\s*\(" % re.escape(fn_name),
                                                b, re.M)]
        if named:
            return named[-1]
    anydef = [b for b in blocks if re.search(r"^\s*def\s+\w+\s*\(", b, re.M)]
    if anydef:
        return anydef[-1]
    return blocks[-1]


def _defence(block):
    """Remove any line that is itself a markdown fence marker."""
    return "\n".join(ln for ln in block.splitlines() if not ln.strip().startswith("```"))


# REPEAT-AND-MEDIAN. MEASURED 26 Aug 2026: G1 is NOT reproducible run-to-run. Two runs with
# NOTHING changed gave BASE 69/83 and 70/83, three tasks differing; BASE produced five distinct
# signatures in one day. temperature 0, a fixed seed, a pinned GPU split and the per-task reset
# all failed to remove it (G6 shows determinism only WITHIN one model load, and this bench
# reloads per task by design). A single run carries about +/-3-4 points of noise, WIDER than
# most changes being tested, so single-run A/B was reading noise as signal. Scoring each task
# as the MEDIAN of REPEATS runs collapses that noise.
REPEATS = int(os.environ.get("BENCH_REPEATS", "3"))


def median_run(runs):
    if not runs:
        raise ValueError("runs must not be empty")

    if len(runs) == 1:
        return runs[0]

    # Sort by 'passed' value ascending
    sorted_runs = sorted(runs, key=lambda x: x[0])

    n = len(sorted_runs)
    # For even number of elements, we want the upper median
    # which is at index n // 2
    median_index = n // 2

    return sorted_runs[median_index]


def score_task(model, task):
    """Return (passed, total, code, notes, tok_s)."""
    n = len(task["cases"])
    try:
        if RESET_PER_TASK:
            # MEASURED 23 Aug 2026: a prompt's output depends on what the server
            # computed BEFORE it (KV-cache/prefix-reuse state): same prompt, three
            # different outputs after three different histories, but a fixed history
            # reproduces exactly. So each task starts from an identical state: unload,
            # reload, discard the (non-reproducible) cold first call, then measure.
            # Proven 3/3 identical with different history injected before each reset.
            # Costs ~53s/task; it is the price of scores that mean something.
            _reset_models()
            generate(model, task["prompt"])            # cold call, discarded
        r = generate(model, task["prompt"])
    except Exception as e:
        return 0, n, "", ["GENERATE FAILED: %s" % type(e).__name__], 0.0
    raw = r.get("response", "")
    ed = r.get("eval_duration", 1) or 1
    tok_s = round(r.get("eval_count", 0) / (ed / 1e9), 1)
    code = extract_code(raw, task["fn"])

    # Model code runs in a KILLABLE subprocess with a hard timeout. MEASURED 23 Aug
    # 2026: a generated `while i < len(s)` scanner that never advanced `i` hung the
    # direct `fn(*args)` call forever and ate a 3600s gate run. An infinite loop is
    # wrong code and scores as a failure; it must never freeze the harness.
    defined, derr = define_ok(code, task["fn"], timeout=CASE_TIMEOUT)
    if not defined:
        return 0, n, code, ["EXEC/DEFINE FAILED: %s" % (derr or "function %s not defined" % task["fn"])], tok_s

    ok, notes = 0, []
    for arg, want in task["cases"]:
        args = arg if isinstance(arg, tuple) else (arg,)
        ran, got, err = run_case(code, task["fn"], args, timeout=CASE_TIMEOUT)
        if not ran:
            tag = "HANG" if err == "TIMEOUT" else "ERR "
            notes.append("  [%s] %-44s %s" % (tag, repr(arg)[:44], err[:40]))
            continue
        # JSON round-trip turns tuples into lists; compare on a normalised form so
        # (2, 1) == [2, 1]. Dict-returning tasks are unaffected.
        good = _norm(got) == _norm(want)
        ok += good
        if not good:
            notes.append("  [FAIL] %-44s -> %s  want %s"
                         % (repr(arg)[:44], repr(got)[:34], repr(want)[:34]))
    return ok, n, code, notes, tok_s


def main():
    results = {}
    for tag, model in (("BASE", BASE), ("ALIAS", ALIAS)):
        print("\n" + "#" * 78)
        print("# %s   %s   num_ctx=%d" % (tag, model.split("/")[-1][:46], NUM_CTX))
        print("#" * 78, flush=True)
        rows, tot_ok, tot_n, t0 = [], 0, 0, time.time()
        for task in TASKS:
            _runs = [score_task(model, task) for _ in range(REPEATS)]
            ok, n, code, notes, tok_s = median_run(_runs)
            _spread = sorted(r[0] for r in _runs)
            _sp = "" if len(set(_spread)) == 1 else "  spread=%s" % _spread
            tot_ok += ok
            tot_n += n
            mark = "OK  " if ok == n else "FAIL"
            print("\n[%s] %-26s %2d/%-2d  %5.1f tok/s   (%s)%s"
                  % (mark, task["name"], ok, n, tok_s, task["group"], _sp), flush=True)
            for ln in notes[:6]:
                print(ln, flush=True)
            rows.append({"task": task["name"], "group": task["group"], "passed": ok,
                         "total": n, "tok_s": tok_s, "code": code, "failures": notes})
        wall = round(time.time() - t0)
        results[tag] = {"model": model, "num_ctx": NUM_CTX, "passed": tot_ok,
                        "total": tot_n, "wall_s": wall, "tasks": rows}
        print("\n  %s TOTAL: %d/%d in %ds" % (tag, tot_ok, tot_n, wall), flush=True)

    with open(OUT, "w", encoding="utf-8") as f:
        json.dump(results, f, indent=2)

    print("\n" + "=" * 78)
    print("%-28s %10s %10s" % ("TASK", "BASE", "ALIAS"))
    print("-" * 78)
    for i, task in enumerate(TASKS):
        b, a = results["BASE"]["tasks"][i], results["ALIAS"]["tasks"][i]
        delta = a["passed"] - b["passed"]
        flag = "  <-- improved" if delta > 0 else ("  <-- REGRESSED" if delta < 0 else "")
        print("%-28s %6d/%-3d %6d/%-3d%s"
              % (task["name"], b["passed"], b["total"], a["passed"], a["total"], flag))
    print("-" * 78)
    print("%-28s %6d/%-3d %6d/%-3d"
          % ("TOTAL", results["BASE"]["passed"], results["BASE"]["total"],
             results["ALIAS"]["passed"], results["ALIAS"]["total"]))
    print("=" * 78)
    print("saved -> %s" % OUT)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
