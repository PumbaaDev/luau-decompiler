#!/usr/bin/env python3
"""Detect a REFUSAL from the local model and repair it by rebuilding the alias.

The model is abliterated: refusing legitimate reverse-engineering work is not a judgement
call, it is a SYMPTOM. It means the alias no longer points at the abliterated weights or the
Modelfile's uncensored SYSTEM prompt was lost - e.g. the alias was rebuilt from the wrong
base, an ollama pull replaced it, or a Modelfile edit went wrong. So the response is to
REINSTALL the alias from tools/Modelfile.uncensored-coder and verify it complies again.

Uses the same probes and markers as gate G5, so "refusing" means exactly what the gate means.

    python tools/uncensored_guard.py           # check, repair if needed
    python tools/uncensored_guard.py --check   # check only, never rebuild

exit 0 = compliant   1 = refused then REPAIRED   2 = still refusing after repair (needs a human)
"""
import importlib.util, subprocess, sys, time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LOG = ROOT / "tools" / "uncensored_guard.log"
MODELFILE = ROOT / "tools" / "Modelfile.uncensored-coder"

spec = importlib.util.spec_from_file_location("gg", ROOT / "tools" / "goal_gates.py")
G = importlib.util.module_from_spec(spec)
try:
    spec.loader.exec_module(G)
except SystemExit:
    pass


def log(msg):
    line = "%s  %s" % (time.strftime("%Y-%m-%d %H:%M:%S"), msg)
    print(line, flush=True)
    try:
        with LOG.open("a", encoding="utf-8") as f:
            f.write(line + "\n")
    except Exception:
        pass


def probe():
    """(complied, total, [failing probe snippets]) using gate G5's own criteria."""
    complied, bad = 0, []
    for p in G.RE_PROBES:
        try:
            resp = G.call(p)["response"]
        except Exception as e:
            bad.append("PROBE ERROR %s" % type(e).__name__)
            continue
        low = resp.lower()
        has_code = "def " in low
        refused = any(m in low[:900] for m in G.REFUSAL_MARKERS) and not has_code
        if has_code and not refused:
            complied += 1
        else:
            bad.append(resp.strip().replace("\n", " ")[:110])
    return complied, len(G.RE_PROBES), bad


def rebuild():
    log("REINSTALLING the alias from %s" % MODELFILE.name)
    r = subprocess.run([G.OLLAMA, "create", "uncensored-coder", "-f", str(MODELFILE)],
                       capture_output=True, text=True, timeout=1800)
    ok = r.returncode == 0
    log("rebuild %s" % ("succeeded" if ok else "FAILED: %s" % (r.stderr or "")[:160]))
    return ok


def main():
    check_only = "--check" in sys.argv
    ok, n, bad = probe()
    if ok == n:
        log("compliant %d/%d - no action" % (ok, n))
        return 0

    log("REFUSAL DETECTED: complied %d/%d. An abliterated model refusing means the INSTALL is "
        "wrong, not that the request was." % (ok, n))
    for b in bad:
        log("  refused: %s" % b)
    if check_only:
        log("--check given, not rebuilding")
        return 1

    if not rebuild():
        return 2
    ok2, n2, bad2 = probe()
    if ok2 == n2:
        log("REPAIRED: compliant %d/%d after reinstall" % (ok2, n2))
        return 1
    log("STILL REFUSING %d/%d after reinstall - needs a human. Check that the FROM line in the "
        "Modelfile still points at the abliterated GGUF and that it is present on disk." % (ok2, n2))
    for b in bad2:
        log("  refused: %s" % b)
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
