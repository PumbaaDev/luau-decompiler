#!/usr/bin/env python3
"""Whole-file audit eval: the CLEAN instrument the excerpt eval could not be.

Feeds the model a COMPLETE decompiled Luau file (not a snippet, so "which local is
never assigned" is actually answerable) and grades its answer against a TESTED
deterministic oracle - tools/decompiler_audit.find_undefined_registers, the 8/8
unit-tested checker. Ground truth is therefore reproducible, not hand-authored.

The eval measures one thing precisely: does the model's audit AGREE with the oracle?
  RECALL    = of the registers the oracle flags, how many the model finds
  PRECISION = of the registers the model flags, how many the oracle agrees are real
              (1 - precision = the invented-defect rate, the failure that matters)

Files are split into DEFECT (oracle finds >=1) and CLEAN (oracle finds 0, tests
false positives). Only files that fit the 16384 context are used; oversized files
are reported as skipped, never silently dropped.

    python tools/audit_bench/whole_file_audit.py [model] [n_defect] [n_clean]
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
from decompiler_audit import find_undefined_registers          # tested: used-never-declared
from corpus_undefined_audit import declared_never_assigned     # the '9' class: declared-never-assigned


def oracle(src):
    """Ground truth = a register that reads nil at runtime, by either mechanism:
    used but never declared, OR declared bare and never assigned. Both are the
    'undefined register' defect the audit must catch."""
    return sorted(set(find_undefined_registers(src)) | set(declared_never_assigned(src)))

OLLAMA = str(Path.home() / "AppData/Local/Programs/Ollama/ollama.exe")
ENV = dict(os.environ, OLLAMA_MODELS=r"E:\ollama-models")
MODEL = sys.argv[1] if len(sys.argv) > 1 else "uncensored-coder"
N_DEFECT = int(sys.argv[2]) if len(sys.argv) > 2 else 21
N_CLEAN = int(sys.argv[3]) if len(sys.argv) > 3 else 15
RUN_DIR = ROOT / "target/union1324_out/run_20260822_102631"
MAX_CHARS = 26000            # ~ fits under 16384 ctx with the prompt (approx 4 chars/token)

_VN = re.compile(r"\bv\d+\b")


def stop():
    subprocess.run([OLLAMA, "stop", MODEL], env=ENV, capture_output=True, timeout=60)


def ask(prompt, npredict=700):
    body = json.dumps({"model": MODEL, "prompt": prompt, "stream": False,
                       "options": {"num_ctx": 16384, "temperature": 0, "num_predict": npredict}}).encode()
    req = urllib.request.Request("http://localhost:11434/api/generate", data=body,
                                 headers={"content-type": "application/json"})
    return json.load(urllib.request.urlopen(req, timeout=1800)).get("response", "")


PROMPT = (
    "You are auditing a COMPLETE decompiled Luau file for one specific defect: a "
    "local variable matching v<number> (v3, v17, ...) that is USED somewhere but "
    "NEVER assigned a value anywhere in the whole file, so it reads nil at runtime.\n"
    "You have the ENTIRE file, so you can be certain: a register is a defect ONLY if "
    "no `local vN = ...`, no `vN = ...`, and no `local ..., vN, ... = ...` assigns it "
    "anywhere. A register declared bare in `local a, vN, b` but later assigned is FINE. "
    "Identifiers inside comments (--) or strings do not count as uses.\n"
    "Return ONLY a JSON array of the defective register names, e.g. [\"v7\",\"v10\"]. "
    "If there are none, return []. No prose.\n\n"
    "```lua\n%s\n```\n\nJSON array:")


def parse_list(text):
    text = re.sub(r"<think>.*?</think>", "", text, flags=re.S)
    m = re.findall(r"\[[^\[\]]*\]", text, re.S)
    for blob in reversed(m):
        try:
            got = json.loads(blob)
            if isinstance(got, list):
                return set(str(x).strip() for x in got if _VN.fullmatch(str(x).strip()))
        except Exception:
            pass
    return set(x for x in _VN.findall(text))     # fallback: any vN mentioned


def main():
    files = sorted(RUN_DIR.glob("*.lua"))
    defect, clean, skipped = [], [], 0
    for p in files:
        src = p.read_text(encoding="utf-8", errors="replace")
        if len(src) > MAX_CHARS:
            skipped += 1
            continue
        truth = set(oracle(src))
        (defect if truth else clean).append((p, src, truth))
    defect = defect[:N_DEFECT]
    clean = clean[:N_CLEAN]
    print("oracle: %d files with defects, %d clean available; %d skipped (too big)"
          % (len([1 for p in files if True]), 0, skipped), flush=True)
    print("testing %d defect files + %d clean files, model=%s\n" % (len(defect), len(clean), MODEL), flush=True)

    tot_truth = tot_found = tot_flagged = tot_correct = 0
    clean_ok = 0
    rows = []
    for p, src, truth in defect:
        stop()
        ask(PROMPT % src)                        # cold warm-up, discarded
        got = parse_list(ask(PROMPT % src))
        found = truth & got
        correct = truth & got
        tot_truth += len(truth); tot_found += len(found)
        tot_flagged += len(got); tot_correct += len(correct)
        extra = got - truth
        rows.append("DEFECT %-40s truth=%s found=%s invented=%s"
                    % (p.name[:40], sorted(truth), sorted(found), sorted(extra)))
    for p, src, truth in clean:
        stop()
        ask(PROMPT % src)
        got = parse_list(ask(PROMPT % src))
        ok = not got
        clean_ok += ok
        tot_flagged += len(got);
        rows.append("CLEAN  %-40s model_flagged=%s %s"
                    % (p.name[:40], sorted(got), "OK" if ok else "INVENTED"))
    stop()

    for r in rows:
        print("  " + r, flush=True)
    recall = (tot_found / tot_truth * 100) if tot_truth else 0.0
    precision = (tot_correct / tot_flagged * 100) if tot_flagged else 100.0
    print("\n  RECALL   : %d/%d oracle defects found (%.0f%%)" % (tot_found, tot_truth, recall))
    print("  PRECISION: %d/%d flags agreed by oracle (%.0f%%)  -> invented rate %.0f%%"
          % (tot_correct, tot_flagged, precision, 100 - precision))
    print("  CLEAN    : %d/%d files correctly left alone (no false positives)" % (clean_ok, len(clean)))
    Path(ROOT / "whole_file_audit_result.json").write_text(json.dumps(
        {"model": MODEL, "recall_found": tot_found, "recall_tot": tot_truth,
         "precision_correct": tot_correct, "flagged": tot_flagged,
         "clean_ok": clean_ok, "clean_tot": len(clean), "rows": rows}, indent=2), encoding="utf-8")
    print("\nsaved -> whole_file_audit_result.json")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
