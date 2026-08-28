#!/usr/bin/env python3
"""Live A/B: does moving the injected facts from the system HEAD (old) to the newest USER message
(new, prefix-stable) change how well the local model USES those facts? Same bytes, different position.
Sends both variants of several fact-dependent questions to the real local model and checks adherence.
Commit the prefix-stable change only if NEW >= OLD."""
import json, sys, urllib.request
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent))
import local_proxy as LP

OLLAMA = LP.OLLAMA if hasattr(LP, "OLLAMA") else "http://localhost:11434"
MODEL = "uncensored-coder:latest"

# fact-dependent questions with a checkable answer token the injected facts should supply
CASES = [
    ("What GPU does Jep have and how much VRAM?", ["5060", "16"]),
    ("What is Jep's Luau decompiler written in?", ["rust"]),
    ("What local AI model does Jep run for coding?", ["qwen", "coder", "uncensored"]),
]


def ask(system, user):
    body = {"model": MODEL, "keep_alive": -1, "stream": False,
            "messages": [{"role": "system", "content": system}, {"role": "user", "content": user}],
            "options": {"num_ctx": 16384, "temperature": 0}}
    req = urllib.request.Request(OLLAMA + "/api/chat", data=json.dumps(body).encode(),
                                 headers={"Content-Type": "application/json"}, method="POST")
    with urllib.request.urlopen(req, timeout=600) as r:
        return (json.load(r).get("message", {}) or {}).get("content", "") or ""


def hit(ans, toks):
    a = ans.lower()
    return any(t in a for t in toks)


old_ok = new_ok = 0
for q, toks in CASES:
    ctx, n, skill = LP.build_context(q)             # SAME facts selected for both variants
    block = LP._context_block(ctx)                  # the per-turn facts/skill block (preamble stripped)
    # OLD: full context at the system head, bare question
    a_old = ask(ctx, q)
    # NEW (prefix-stable): system = static preamble only, facts moved into the user message
    a_new = ask(LP._PREAMBLE, (block + "\n\n" + q) if block else q)
    o, nw = hit(a_old, toks), hit(a_new, toks)
    old_ok += o
    new_ok += nw
    print("  [old %s | new %s] facts=%d  q=%s" % ("OK " if o else "MISS", "OK " if nw else "MISS", n, q[:44]),
          flush=True)

print("\nADHERENCE  old(system-head) %d/%d   new(prefix-stable) %d/%d" % (old_ok, len(CASES), new_ok, len(CASES)))
print("VERDICT:", "SAFE - new >= old, prefix-stable does not hurt adherence"
      if new_ok >= old_ok else "REGRESSION - new position hurts fact use, DO NOT ship")
sys.exit(0 if new_ok >= old_ok else 1)
