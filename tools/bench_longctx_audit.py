#!/usr/bin/env python3
"""The gap every other test left open: does the checklist SURVIVE a full context?

Every measurement so far used a short prompt. The real job is auditing decompiler
output, which means pasting hundreds of lines of Lua. The system prompt sits at the
FRONT of the window; with ~800 lines of code after it, does the model still apply
the checklist, or does the instruction get buried?

Method: generate a realistic Lua file with a KNOWN set of planted undefined
registers, plus deliberate DECOYS that must NOT be reported (registers named only
in comments or strings, and ones properly declared via `local a,b,c` /
`local function`). Ground truth is exact, so both recall and false positives are
measurable.

    python tools/bench_longctx_audit.py
"""
from __future__ import annotations

import json
import os
import re
import urllib.error
import urllib.request

URL = "http://localhost:11434/api/generate"
# BASE REPOINTED 26 Aug 2026. This pointed at the Huihui coder GGUF, which was DELETED to
# reclaim disk when the coder was swapped to Heretic on 24 Aug. Every call 404'd, so G3 has
# been dead ever since and only surfaced when the full gate set was finally run end to end.
# This is the SECOND harness found pointing at that deleted model (see bench_process_regression,
# fixed in c14ac82, where a dead BASE scored 0/83 while printing "improved" beside every task).
# BASE must be the weights the ALIAS is built FROM, so the only variable is the system prompt.
BASE = "hf.co/mradermacher/Qwen3-Coder-30B-A3B-Instruct-Heretic-i1-GGUF:Q4_K_M"
ALIAS = "uncensored-coder"
NUM_CTX = int(os.environ.get("BENCH_NUM_CTX", "16384"))
OUT = "bench_longctx_audit.json"

# Registers genuinely used but never declared. These MUST sit far above the
# generator's sequential v{n} counter (which climbs past v340) or they collide
# with a real declaration and the ground truth becomes a lie. The first version
# used v77/v104/v233, all of which the counter declared, so both models scored a
# false 3/3. Validate with the oracle before trusting any run.
PLANTED = ["v9001", "v9014", "v9033"]
# Must NOT be reported: comment-only, string-only, and properly declared.
DECOY_COMMENT = "v9901"
DECOY_STRING = "v9902"


def build_lua(target_lines: int = 780) -> str:
    """Realistic decompiler-style Lua with exactly PLANTED undefined registers."""
    out = [
        "-- Decompiled with luau-decompiler",
        "-- Protos: 14 total, main=1",
        "",
        "local Players = game:GetService('Players')",
        "local RunService = game:GetService('RunService')",
        "",
    ]
    n = 1
    fi = 0
    while len(out) < target_lines:
        fi += 1
        a, b, c = n, n + 1, n + 2
        n += 3
        out += [
            "local function handler%d(arg1, arg2)" % fi,
            "    local v%d, v%d, v%d = arg1, arg2, 0" % (a, b, c),   # comma decls
            "    local v%d = v%d + v%d" % (n, a, b),
            "    if v%d > 0 then" % c,
            "        v%d = v%d * 2" % (n, n),
            "    end",
            "    return v%d" % n,
            "end",
            "",
        ]
        n += 1
        if fi == 4:
            # decoy: named ONLY in a comment
            out += ["-- register %s was removed in an earlier pass" % DECOY_COMMENT, ""]
        if fi == 8:
            # decoy: named ONLY in a string
            out += ['local msg = "%s is not a register"' % DECOY_STRING, ""]
        if fi == 12:
            # decoy: declared via local function, then used (v9500 is clear of the sequence)
            out += ["local function v9500() return 1 end", "local r = v9500()", ""]
        if fi == 16:
            # PLANTED #1 - used, never declared
            out += ["local acc = %s + 1" % PLANTED[0], ""]
        if fi == 24:
            # PLANTED #2
            out += ["if %s ~= nil then acc = acc + %s end" % (PLANTED[1], PLANTED[1]), ""]
        if fi == 32:
            # PLANTED #3
            out += ["return acc .. tostring(%s)" % PLANTED[2], ""]
    return "\n".join(out[:target_lines])


PROMPT_TMPL = (
    "Below is a decompiled Luau file. Audit it.\n\n"
    "List every identifier matching v followed by digits that is USED but NEVER "
    "declared by a `local` statement. Declarations include `local v1`, "
    "`local v1, v2`, `local v1,v2` and `local function v5`. Identifiers appearing "
    "only inside a Lua comment (-- to end of line) or inside a string literal are "
    "NOT uses and must not be reported.\n\n"
    "Answer with ONLY a JSON array of the undefined names, for example "
    '["v7","v10"]. No prose.\n\n'
    "```lua\n%s\n```\n"
)


def gen(model, prompt):
    payload = {"model": model, "prompt": prompt, "stream": False,
               "options": {"num_ctx": NUM_CTX, "temperature": 0, "num_predict": 700}}
    for think in (False, None):
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


def parse_names(text):
    text = re.sub(r"<think>.*?</think>", "", text, flags=re.S)
    m = re.findall(r"\[[^\[\]]*\]", text, re.S)
    for blob in reversed(m):
        try:
            got = json.loads(blob)
            if isinstance(got, list):
                return [str(x) for x in got]
        except Exception:
            pass
    return sorted(set(re.findall(r"\bv\d+\b", text)))


def _validate_ground_truth(src):
    """Refuse to run against a broken answer key.

    Uses the independently unit-tested reference (decompiler_audit) as an oracle:
    the file must contain EXACTLY the planted undefined registers and none of the
    decoys. The first version of this bench planted v77/v104/v233, which collided
    with the generator's own v{n} sequence, so both models scored a false 3/3.
    """
    import os
    import sys
    sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
    from decompiler_audit import find_undefined_registers
    oracle = set(find_undefined_registers(src))
    planted = set(PLANTED)
    problems = []
    if oracle != planted:
        problems.append("oracle=%s planted=%s" % (sorted(oracle), sorted(planted)))
    for d in (DECOY_COMMENT, DECOY_STRING, "v9500"):
        if d in oracle:
            problems.append("decoy %s leaked into oracle" % d)
    return (not problems), oracle, problems


def main():
    src = build_lua()
    lines = src.count("\n") + 1

    ok, oracle, problems = _validate_ground_truth(src)
    print("ground-truth check (independent oracle): %s" % ("VALID" if ok else "BROKEN"))
    print("  oracle finds undefined: %s" % sorted(oracle))
    if not ok:
        for p in problems:
            print("  PROBLEM: %s" % p)
        print("\nAborting: the answer key is wrong, so any model score would be meaningless.")
        return 1

    prompt = PROMPT_TMPL % src
    print("Lua file: %d lines, %d chars" % (lines, len(src)))
    print("ground truth undefined : %s" % PLANTED)
    print("decoys that must NOT appear: %s (comment), %s (string), v500 (local function)"
          % (DECOY_COMMENT, DECOY_STRING))
    print()

    results = {}
    for tag, model in (("BASE", BASE), ("ALIAS", ALIAS)):
        r = gen(model, prompt)
        got = parse_names(r.get("response", ""))
        pin = r.get("prompt_eval_count", 0)
        truth = set(PLANTED)
        found = sorted(truth & set(got), key=lambda s: int(s[1:]))
        missed = sorted(truth - set(got), key=lambda s: int(s[1:]))
        false_pos = sorted(set(got) - truth, key=lambda s: int(s[1:]) if s[1:].isdigit() else 0)
        decoys_hit = [d for d in (DECOY_COMMENT, DECOY_STRING, "v500") if d in got]
        results[tag] = {"model": model, "prompt_tokens": pin, "returned": got,
                        "found": found, "missed": missed, "false_positives": false_pos,
                        "decoys_hit": decoys_hit}
        print("=" * 70)
        print("%s   prompt_tokens=%d of num_ctx %d" % (tag, pin, NUM_CTX))
        print("=" * 70)
        print("  returned        : %s" % (got[:20],))
        print("  correctly found : %s   (%d of %d)" % (found, len(found), len(truth)))
        print("  MISSED          : %s" % (missed or "none"))
        print("  FALSE POSITIVES : %d  %s" % (len(false_pos), false_pos[:15]))
        print("  decoys wrongly reported: %s" % (decoys_hit or "none"))
        print()

    with open(OUT, "w", encoding="utf-8") as f:
        json.dump({"lines": lines, "planted": PLANTED, "results": results}, f, indent=2)
    print("saved -> %s" % OUT)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
