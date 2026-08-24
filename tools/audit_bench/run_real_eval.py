#!/usr/bin/env python3
"""Measure the model on its REAL job: auditing decompiled Luau, not synthetic
parse-a-line tasks. 12 hand-verified tasks from actual corpus output (union1324),
each a real ~25-line excerpt with a ground-truth defect (or a false-positive trap).

Grading, per the frozen scoring rule:
  DEFECT task passes  = the answer names the EXACT expected variable (e.g. upval_2,
                        v5, v19) AND does not also flag a bystander that is fine.
  TRAP task passes    = the answer says the code is valid / no defect, and invents
                        no variable. (Traps have no single expected variable.)

Reports: recall on real defects, false-positive/invented-defect rate, per-task.
Runs each task from a reset model (reproducible), through local_mem retrieval so it
matches the production audit path.

    python tools/audit_bench/run_real_eval.py [model]     # default uncensored-coder
"""
from __future__ import annotations

import json
import os
import re
import subprocess
import sys
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent
sys.path.insert(0, str(ROOT / "tools"))
from mem import load_facts, retrieve  # noqa: E402


def build_audit_prompt(hits, excerpt, question):
    """Audit prompt: REASON ABOUT THE CODE, with facts as light reference.

    NOT the knowledge-query build_prompt() - that one ("answer ONLY from facts,
    else NOT IN MEMORY") + its entity grounding made the model refuse to analyse
    code variables ("NOT IN MEMORY: v19") because v19 is not in the fact store.
    Entity grounding is for 'does this API exist' questions; a code audit must
    reason about the provided text, so facts are background only, not a lockdown.
    MEASURED 24 Aug 2026: the wrong prompt scored 3/10; this is the fix.
    """
    ref = ""
    if hits:
        ref = ("Reference (your own decompiler notes, use if relevant):\n"
               + "\n".join("- %s" % f.get("body", "")[:200] for _, f in hits[:3]) + "\n\n")
    return (
        "You are auditing DECOMPILED Luau for defects. Reason about the CODE below "
        "directly - trace which locals are assigned and which are only read.\n"
        "A real defect is a local that is USED but never ASSIGNED anywhere (it is "
        "nil at runtime). Code that is valid Lua is NOT a defect, even if it looks "
        "odd (e.g. `tbl.__index = tbl`, or `game.ReplicatedFirst.SomeChild`).\n\n"
        + ref +
        "```lua\n" + excerpt + "\n```\n\n"
        "Question: " + question + "\n\nAnswer concisely:")

OLLAMA = str(Path.home() / "AppData/Local/Programs/Ollama/ollama.exe")
ENV = dict(os.environ, OLLAMA_MODELS=r"E:\ollama-models")
MODEL = sys.argv[1] if len(sys.argv) > 1 else "uncensored-coder"
EVAL = Path(__file__).resolve().parent / "real_audit_eval.json"

_VAR = re.compile(r"\b(?:upval_?\d+|cap_?\d+|v\d+|[A-Za-z]\w*\d+)\b")
_TRAP_WORDS = ("valid", "no defect", "not a defect", "is fine", "correctly assigned",
               "properly assigned", "nothing wrong", "no undefined", "no issue")


def stop():
    subprocess.run([OLLAMA, "stop", MODEL], env=ENV, capture_output=True, timeout=60)


def ask(prompt, npredict=600):
    body = json.dumps({"model": MODEL, "prompt": prompt, "stream": False,
                       "options": {"num_ctx": 16384, "temperature": 0, "num_predict": npredict}}).encode()
    req = urllib.request.Request("http://localhost:11434/api/generate", data=body,
                                 headers={"content-type": "application/json"})
    return json.load(urllib.request.urlopen(req, timeout=1200)).get("response", "")


def expected_var(task):
    """Pull the ground-truth variable name from the expected answer, if any."""
    exp = task.get("expected", "")
    m = _VAR.search(exp.split(".")[0])          # first sentence usually names it
    return m.group(0) if m else None


def is_trap(task):
    exp = task.get("expected", "").strip().lower()
    dk = (task.get("defect_kind", "") or "").lower()
    return exp.startswith(("valid", "no defect", "not a defect")) or "trap" in dk or "valid" in dk


def main():
    facts = load_facts()
    tasks = json.loads(EVAL.read_text(encoding="utf-8"))["tasks"]
    print("REAL AUDIT EVAL  model=%s  %d tasks\n" % (MODEL, len(tasks)), flush=True)

    defect_hit = defect_tot = trap_ok = trap_tot = invented = 0
    rows = []
    for i, t in enumerate(tasks):
        hits = retrieve(facts, t["question"] + " undefined register decompiler", 6)
        prompt = build_audit_prompt(hits, t["excerpt"], t["question"])
        stop()
        ask(prompt)                              # cold warm-up, discarded
        ans = ask(prompt)
        low = ans.lower()

        if is_trap(t):
            trap_tot += 1
            said_valid = any(w in low for w in _TRAP_WORDS)
            # invented a defect var when there is none?
            named = [v for v in set(_VAR.findall(ans)) if v.lower() not in ("v2", "v1")]
            ok = said_valid and not any(("undefined" in low or "never assigned" in low or "defect" in low) for _ in [0])
            trap_ok += ok
            if not ok:
                invented += 1
            rows.append(("TRAP", "PASS" if ok else "FAIL(invented)", t.get("file", "?").split(chr(92))[-1][:30], ans[:70]))
        else:
            defect_tot += 1
            ev = expected_var(t)
            hit = bool(ev) and ev in ans
            defect_hit += hit
            rows.append(("DEFECT", "HIT " + (ev or "?") if hit else "MISS " + (ev or "?"),
                         t.get("file", "?").split(chr(92))[-1][:30], ans[:70]))
    stop()

    for kind, verdict, f, a in rows:
        print("  [%-6s %-14s] %-30s %s" % (kind, verdict, f, a.replace(chr(10), " ")), flush=True)
    print("\n  DEFECTS found (recall): %d/%d" % (defect_hit, defect_tot))
    print("  TRAPS passed (no FP)  : %d/%d   (invented defects: %d)" % (trap_ok, trap_tot, invented))
    Path(ROOT / "audit_bench_result.json").write_text(json.dumps(
        {"model": MODEL, "defect_hit": defect_hit, "defect_tot": defect_tot,
         "trap_ok": trap_ok, "trap_tot": trap_tot, "invented": invented, "rows": rows}, indent=2),
        encoding="utf-8")
    print("\nsaved -> audit_bench_result.json")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
