#!/usr/bin/env python3
"""The audit tool the measurements pointed to: ORACLE FINDS, MODEL EXPLAINS.

MEASURED 24 Aug 2026 (tools/audit_bench/whole_file_audit.py): asked to ENUMERATE
undefined registers over a whole file, the model floods false positives (93%
invented, dumps every register on large files). But the tested deterministic
oracle (find_undefined_registers) does that job precisely and free. So this driver
splits the work along the measured strengths:

  1. ORACLE (deterministic, precise) finds the candidate defective registers.
  2. For EACH candidate, the MODEL gets ONLY the lines where that register appears
     and explains, in 1-2 sentences, what it should hold and the runtime
     consequence. One target, focused context - never enumeration, so the
     over-flagging failure mode cannot occur.

Output: a readable audit report per file. The model can also flag a suspected
oracle false-positive, which is logged rather than trusted blindly.

    python tools/audit_bench/oracle_explain.py <file.lua>       # one file
    python tools/audit_bench/oracle_explain.py --corpus [N]     # N defect files, write report
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
from decompiler_audit import find_undefined_registers
from corpus_undefined_audit import declared_never_assigned
sys.path.insert(0, str(Path(__file__).resolve().parent))   # tools/audit_bench, for oracle_extra
from oracle_extra import oracle_extra, zero_call_functions

OLLAMA = str(Path.home() / "AppData/Local/Programs/Ollama/ollama.exe")
ENV = dict(os.environ, OLLAMA_MODELS=r"E:\ollama-models")
MODEL = os.environ.get("AUDIT_MODEL", "uncensored-coder")
RUN_DIR = ROOT / "target/union1324_out/run_20260822_102631"
CTX_LINES = 2       # lines of context around each occurrence


def oracle(src):
    return sorted(set(find_undefined_registers(src)) | set(declared_never_assigned(src)))


def context_for(src, reg):
    """The lines where `reg` appears, each with +/- CTX_LINES, merged. Focused
    view so the model reasons about ONE register, not the whole file."""
    lines = src.splitlines()
    want = set()
    pat = re.compile(r"\b%s\b" % re.escape(reg))
    for i, ln in enumerate(lines):
        if pat.search(ln):
            for j in range(max(0, i - CTX_LINES), min(len(lines), i + CTX_LINES + 1)):
                want.add(j)
    out, prev = [], -2
    for i in sorted(want):
        if i != prev + 1 and out:
            out.append("    ...")
        out.append("%4d: %s" % (i + 1, lines[i]))
        prev = i
    return "\n".join(out)


def stop():
    subprocess.run([OLLAMA, "stop", MODEL], env=ENV, capture_output=True, timeout=60)


def ask(prompt, npredict=260):
    body = json.dumps({"model": MODEL, "prompt": prompt, "stream": False,
                       "options": {"num_ctx": 16384, "temperature": 0, "num_predict": npredict}}).encode()
    req = urllib.request.Request("http://localhost:11434/api/generate", data=body,
                                 headers={"content-type": "application/json"})
    return json.load(urllib.request.urlopen(req, timeout=600)).get("response", "").strip()


PROMPT = (
    "A deterministic checker confirmed the local register `%s` in a decompiled Luau "
    "file is used but never assigned, so it is nil at runtime. Focus ONLY on `%s`; "
    "do not examine other variables.\n\n"
    "Lines where `%s` appears:\n%s\n\n"
    "Reply in EXACTLY this format, one line each, then STOP. Do not re-examine or add "
    "commentary:\n"
    "HELD: <what value %s was meant to hold, inferred from its use, <=15 words>\n"
    "BREAKS: <the concrete runtime error or misbehaviour, <=15 words>\n"
    "If it is actually assigned in the lines above, instead reply exactly:\n"
    "FALSE POSITIVE: <the assignment line>")


# The extended families (tools/audit_bench/oracle_extra.py) are ALSO oracle-found
# and model-explained, but they are not the nil-register family, so they get their
# own focused prompts. dead_store is anchored to a line; truncated_body and
# dropped_call are file-level (the lifter's own counts are the evidence).

EXTRA_PROMPT_DEAD = (
    "A deterministic checker confirmed the local `%s` in a decompiled Luau file is "
    "assigned a value that is never read, while the base it was computed from is "
    "used again below. This is a decompiler defect: the branch arm's value was "
    "written to a fresh local instead of back to its register. Evidence: %s\n\n"
    "Lines around it:\n%s\n\n"
    "Reply in EXACTLY this format, one line each, then STOP. No extra commentary:\n"
    "LOST: <what the discarded value was for, inferred from its use, <=15 words>\n"
    "EFFECT: <the concrete behavioural consequence, <=15 words>")

EXTRA_PROMPT_FILE = (
    "A deterministic checker flagged a file-level decompiler defect of kind `%s` in "
    "a decompiled Luau file. The evidence comes from the lifter's own bytecode-vs-AST "
    "counts, so the defect is confirmed: %s\n\n"
    "Relevant context:\n%s\n\n"
    "In ONE sentence (<=25 words), state the concrete consequence for someone relying "
    "on this decompiled output. Do not speculate beyond the evidence.")


def context_at_line(src, line, radius=3):
    """The source lines around a 1-based line number, numbered. Used for anchored
    extra findings so the model sees exactly the defect site, no whole-file scan."""
    lines = src.splitlines()
    lo, hi = max(0, line - 1 - radius), min(len(lines), line + radius)
    return "\n".join("%4d: %s" % (i + 1, lines[i]) for i in range(lo, hi))


def context_file_level(src, kind):
    """Context for a file-level finding: the comment headers the lifter wrote (which
    ARE the evidence), plus - for dropped_call - the candidate short functions."""
    lines = src.splitlines()
    head = [l for l in lines[:8] if l.strip().startswith("--")]
    if kind == "dropped_call":
        zc = zero_call_functions(src)
        if zc:
            head.append("functions whose body shows no call (candidates): "
                        + ", ".join(zc[:12]))
    else:  # truncated_body: header + a slice of the body so the model sees the gap
        head = lines[:24]
    return "\n".join(head)


def explain_extra(src, warm_reset=True, dry=False):
    """Model-explain each extended-family finding. dry=True returns the deterministic
    findings with no model call (wiring check, and safe to run while the GPU is busy)."""
    out = []
    for f in oracle_extra(src):
        if dry:
            out.append({"kind": f.kind, "anchor": f.anchor, "line": f.line,
                        "detail": f.detail, "explanation": ""})
            continue
        if f.kind == "dead_store" and f.line is not None:
            prompt = EXTRA_PROMPT_DEAD % (f.anchor, f.detail, context_at_line(src, f.line))
        else:
            prompt = EXTRA_PROMPT_FILE % (f.kind, f.detail, context_file_level(src, f.kind))
        if warm_reset:
            stop()
            ask(prompt, npredict=160)                    # warm-up discard for reproducibility
        expl = ask(prompt, npredict=160)
        lines = [ln.strip() for ln in expl.splitlines() if ln.strip()]
        out.append({"kind": f.kind, "anchor": f.anchor, "line": f.line,
                    "detail": f.detail, "explanation": " ".join(lines[:2])})
    return out


def audit_file(path: Path, warm_reset=True):
    src = path.read_text(encoding="utf-8", errors="replace")
    regs = oracle(src)
    findings = []
    for reg in regs:
        ctx = context_for(src, reg)
        if warm_reset:
            stop()
            ask(PROMPT % (reg, reg, reg, ctx, reg))     # warm-up discard for reproducibility
        expl = ask(PROMPT % (reg, reg, reg, ctx, reg))
        # if the model gave a real HELD/BREAKS explanation, a trailing "FALSE
        # POSITIVE: ...declared but not assigned" line is just it restating the
        # defect (that IS the defect) - drop the contradiction, keep the substance.
        lines = [ln for ln in expl.splitlines() if ln.strip()]
        if any(ln.upper().startswith("HELD:") for ln in lines):
            lines = [ln for ln in lines if not ln.upper().startswith("FALSE POSITIVE")]
        expl = " ".join(ln.strip() for ln in lines[:2])
        findings.append({"register": reg, "explanation": expl})
    return regs, findings


def _extra_line(e):
    """Report line for one extended-family finding."""
    loc = "" if e["line"] is None else " (line %d)" % e["line"]
    tag = e["anchor"] or e["kind"]
    return "- **%s**%s `[%s]` — %s" % (tag, loc, e["kind"], e["explanation"] or e["detail"])


def main():
    args = sys.argv[1:]
    dry = "--dry" in args                       # deterministic-only, no model calls
    args = [a for a in args if a != "--dry"]
    if not args:
        print(__doc__); return 1

    if args[0] == "--corpus":
        n = int(args[1]) if len(args) > 1 else 21

        def has_defect(p):
            src = p.read_text(encoding="utf-8", errors="replace")
            return len(src) <= 26000 and bool(oracle(src) or oracle_extra(src))

        files = [p for p in sorted(RUN_DIR.glob("*.lua")) if has_defect(p)][:n]

        if dry:
            print("DRY RUN - deterministic findings only, no model calls\n")
            for p in files:
                src = p.read_text(encoding="utf-8", errors="replace")
                regs = oracle(src)
                extra = explain_extra(src, dry=True)
                print("%s" % p.name)
                if regs:
                    print("  undefined_register: %s" % regs)
                for e in extra:
                    loc = "" if e["line"] is None else ":%d" % e["line"]
                    print("  %s%s  %s" % (e["kind"], loc, e["detail"]))
            print("\n%d files, no model used" % len(files))
            return 0

        report = ["# Decompiler audit report - oracle-finds / model-explains",
                  "model: %s   files: %d\n" % (MODEL, len(files))]
        for p in files:
            print("auditing %s ..." % p.name, flush=True)
            src = p.read_text(encoding="utf-8", errors="replace")
            regs, findings = audit_file(p)
            extra = explain_extra(src)
            report.append("## %s" % p.name)
            for f in findings:
                report.append("- **%s** `[undefined_register]` — %s" % (f["register"], f["explanation"]))
            for e in extra:
                report.append(_extra_line(e))
            report.append("")
        stop()
        out = ROOT / "audit_report.md"
        out.write_text("\n".join(report), encoding="utf-8")
        print("\nwrote %s (%d files)" % (out, len(files)))
    else:
        p = Path(args[0])
        if not p.exists():
            p = RUN_DIR / args[0]
        src = p.read_text(encoding="utf-8", errors="replace")
        if dry:
            print("\n# %s (DRY)" % p.name)
            print("undefined_register: %s" % oracle(src))
            for e in explain_extra(src, dry=True):
                loc = "" if e["line"] is None else ":%d" % e["line"]
                print("%s%s  %s" % (e["kind"], loc, e["detail"]))
            return 0
        regs, findings = audit_file(p)
        extra = explain_extra(src)
        stop()
        print("\n# %s" % p.name)
        print("oracle found %d undefined register(s): %s\n" % (len(regs), regs))
        for f in findings:
            print("## %s [undefined_register]" % f["register"])
            print(f["explanation"] + "\n")
        for e in extra:
            loc = "" if e["line"] is None else " (line %d)" % e["line"]
            print("## %s%s [%s]" % (e["anchor"] or e["kind"], loc, e["kind"]))
            print((e["explanation"] or e["detail"]) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
