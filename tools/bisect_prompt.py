#!/usr/bin/env python3
"""Find WHICH part of the system prompt costs find_undefined_registers 2 points.

BASE (no system prompt) scores 10/10 on that task; the full prompt scores 8/10 - measured on a
reproducible control (ollama restarted per run, so the ruler holds still). Something in the
prompt is the cause. This bisects it.

Only the ONE blocking task is run, and the system prompt is varied through the API rather than
by rebuilding the alias, so a probe costs ~2 min instead of a ~15 min full gate.

    python tools/bisect_prompt.py
"""
import importlib.util, json, os, re, sys, time, urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("bpr", ROOT / "tools" / "bench_process_regression.py")
B = importlib.util.module_from_spec(spec)
try:
    spec.loader.exec_module(B)
except SystemExit:
    pass

MODEL = B.BASE                      # raw weights; the system prompt is the ONLY variable
TASK = next(t for t in B.TASKS if t["name"] == "find_undefined_registers")


def gen(system, prompt):
    body = {"model": MODEL, "prompt": prompt, "stream": False, "options": B.OPTIONS}
    if system:
        body["system"] = system
    req = urllib.request.Request(B.URL, data=json.dumps(body).encode(),
                                 headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=2400) as r:
        return json.load(r)


def score(system, label):
    """Run the task once under `system` and return passes/total, same protocol as the bench."""
    B._reset_models()
    try:
        gen(system, TASK["prompt"])                       # cold call, discarded
        raw = gen(system, TASK["prompt"]).get("response", "")
    except Exception as e:
        print("  %-34s GENERATE FAILED %s" % (label, type(e).__name__), flush=True)
        return -1, len(TASK["cases"])
    code = B.extract_code(raw, TASK["fn"])
    ok_def, err = B.define_ok(code, TASK["fn"], timeout=B.CASE_TIMEOUT)
    n = len(TASK["cases"])
    if not ok_def:
        print("  %-34s DEFINE FAILED (%s)" % (label, (err or "")[:30]), flush=True)
        return 0, n
    ok = 0
    for arg, want in TASK["cases"]:
        args = arg if isinstance(arg, tuple) else (arg,)
        ran, got, _ = B.run_case(code, TASK["fn"], args, timeout=B.CASE_TIMEOUT)
        ok += bool(ran and B._norm(got) == B._norm(want))
    print("  %-34s %2d/%d" % (label, ok, n), flush=True)
    return ok, n


def sections():
    """The system prompt split into its '## ' sections, plus the identity preamble."""
    mf = (ROOT / "tools" / "Modelfile.uncensored-coder").read_text(encoding="utf-8")
    m = re.search(r'SYSTEM """(.*?)"""', mf, re.S)
    body = m.group(1)
    parts = re.split(r"\n(?=## )", body)
    return [p.strip() for p in parts if p.strip()]


def main():
    if os.environ.get("BISECT_RESTART", "1") == "1":
        B._restart_server()
    secs = sections()
    print("system prompt has %d sections\n" % len(secs))
    print("CONTROLS")
    empty_ok, n = score("", "no system prompt (expect 10)")
    full_ok, _ = score("\n\n".join(secs), "FULL prompt (expect 8)")
    if empty_ok <= full_ok:
        print("\nControls did not separate (%d vs %d) - bisection is meaningless. Stop."
              % (empty_ok, full_ok))
        return 1
    print("\nEACH SECTION ALONE (preamble + one section). A section scoring < %d is a suspect."
          % empty_ok)
    preamble = secs[0]
    suspects = []
    for i, sec in enumerate(secs[1:], 1):
        head = sec.splitlines()[0][:32]
        ok, _ = score(preamble + "\n\n" + sec, "%2d. %s" % (i, head))
        if 0 <= ok < empty_ok:
            suspects.append((i, head, ok))
    print("\n=== SUSPECTS (dropped the score on their own) ===")
    for i, head, ok in suspects:
        print("  %2d  %-34s %d/%d" % (i, head, ok, n))
    if not suspects:
        print("  none - no single section causes it, so it is an INTERACTION between sections")
    json.dump({"empty": empty_ok, "full": full_ok,
               "suspects": [{"idx": i, "head": h, "score": s} for i, h, s in suspects]},
              open(ROOT / "goal" / "bisect_result.json", "w"), indent=2)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
