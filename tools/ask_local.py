"""Ask the local model a question with the relevant knowledge retrieved for it.

CORE.md is baked into local-coder as a SYSTEM prompt, so the operating rules and
hardware limits are always present. The rest - 240 memory files and 128 vault
notes, ~2.6MB - cannot fit any context, so this scores the index for the query
and injects only the top chunks.

CORRECTIONS.md is injected AFTER the notes, always. The notes are a historical
record and older entries carry figures that were later measured and replaced
(YPP thresholds, the channel video count, the title-case finding). Retrieval
surfaces those stale entries and they outweigh a SYSTEM prompt, so the overrides
must sit last, where they carry the most weight. Measured: without this the
model answered the OLD YPP thresholds from a stale note.

Scoring is term-overlap with an IDF weight. No embedding model, because loading
one alongside gemma4 would spill the 5060 Ti - the failure this project keeps
hitting.

    python tools/ask_local.py "why does disassemble print wrong operands?"
    python tools/ask_local.py --show "what breaks if num_ctx is unset?"
"""
import argparse
import json
import math
import re
import sys
import urllib.request
from collections import Counter
from pathlib import Path

KB = Path(__file__).parent / "local_kb" / "index.jsonl"
CORR = Path(__file__).parent / "local_kb" / "CORRECTIONS.md"
URL = "http://localhost:11434/api/generate"
MODEL = "uncensored-coder"   # default upgraded to Qwen3-Coder-30B-A3B-Heretic, 80.6/83, 24 Aug 2026
sys.path.insert(0, str(Path(__file__).resolve().parent))
from skill_inject import build_injection   # sanitized, uncensored skill injection
STOP = {"the", "a", "an", "and", "or", "of", "to", "in", "is", "it", "that",
        "this", "for", "on", "with", "as", "by", "be", "are", "was", "how",
        "what", "why", "when", "do", "does", "did", "i", "you", "we", "my", "our"}


def tok(s):
    return [w for w in re.findall(r"[a-z0-9_]+", (s or "").lower())
            if w not in STOP and len(w) > 2]


def load():
    if not KB.exists():
        sys.exit("no index at %s. Run: python tools/build_local_kb.py" % KB)
    return [json.loads(l) for l in KB.open(encoding="utf-8") if l.strip()]


def retrieve(rows, query, k=8):
    q = Counter(tok(query))
    if not q:
        return []
    df = Counter()
    docs = []
    for r in rows:
        t = set(tok(r["text"]))
        docs.append(t)
        for w in q:
            if w in t:
                df[w] += 1
    n = len(rows)
    scored = []
    for r, t in zip(rows, docs):
        s = sum(qc * math.log(1 + n / (1 + df[w])) for w, qc in q.items() if w in t)
        if s > 0:
            covered = sum(1 for w in q if w in t) / len(q)
            s *= 1 + covered          # mostly-about beats a passing mention
            scored.append((s, r))
    scored.sort(key=lambda x: -x[0])
    return scored[:k]


def ask(prompt):
    body = json.dumps({"model": MODEL, "prompt": prompt, "stream": False,
                       "think": False,          # gemma4 returns EMPTY without this
                       "options": {"num_ctx": 65536, "temperature": 0,
                                   "num_predict": 4000}}).encode()
    req = urllib.request.Request(URL, data=body,
                                 headers={"content-type": "application/json"})
    return json.load(urllib.request.urlopen(req, timeout=600)).get("response", "").strip()


def build_prompt(hits, q, skill_block=""):
    ctx = "\n\n---\n\n".join(
        "[%s: %s]\n%s" % (r["kind"], r["source"], r["text"]) for _, r in hits)
    corr = CORR.read_text(encoding="utf-8") if CORR.exists() else ""
    return (
        (skill_block + "\n\n" if skill_block else "") +
        "Answer using the notes below. They are measured facts from this user's "
        "own projects and outrank anything you think you know. If the notes do "
        "not cover it, say so plainly rather than guessing.\n\n"
        "=== NOTES ===\n" + ctx + "\n=== END NOTES ===\n\n"
        "=== CORRECTIONS - these OVERRIDE the notes above ===\n" + corr +
        "\n=== END CORRECTIONS ===\n\n"
        "Question: " + q + "\n\nAnswer:")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("question", nargs="+")
    ap.add_argument("--k", type=int, default=8, help="chunks to retrieve")
    ap.add_argument("--show", action="store_true", help="print what was retrieved")
    ap.add_argument("--skill", action="store_true",
                    help="inject the best-matching local-usable skill (sanitized, uncensored)")
    ap.add_argument("--skill-k", type=int, default=1, help="how many skills to inject")
    a = ap.parse_args()
    q = " ".join(a.question)

    skill_block = build_injection(q, a.skill_k) if a.skill else ""
    if a.skill and (a.show or not skill_block):
        print("skill blocks injected: %d" % skill_block.count("### skill:"), file=sys.stderr)

    hits = retrieve(load(), q, a.k)
    if a.show or not hits:
        print("retrieved %d chunks:" % len(hits), file=sys.stderr)
        for s, r in hits:
            print("   %6.1f  %-9s %s" % (s, r["kind"], r["source"][:52]), file=sys.stderr)
        print("", file=sys.stderr)

    print(ask(build_prompt(hits, q, skill_block)))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
