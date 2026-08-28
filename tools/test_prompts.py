#!/usr/bin/env python3
"""Broad prompt-coverage battery: run many DIVERSE prompt types through the phone_agent's submit/poll
path (the phone's flow) and check each produced a sane answer. Proves the model handles coding,
codebase Q&A, system/files, web, email, memory, uncensored RE, reasoning, writing and math."""
import json, os, sys, time, urllib.request, urllib.error

BASE = os.environ.get("PHONE_URL", "http://localhost:8772")
TOK = os.environ.get("PHONE_AGENT_TOKEN", "")
H = {"Content-Type": "application/json", "Authorization": "Bearer " + TOK}
rows = []


def ask(q, budget=240):
    r = urllib.request.Request(BASE + "/ask", data=json.dumps({"messages": [{"role": "user", "content": q}]}).encode(), headers=H)
    try:
        jid = json.loads(urllib.request.urlopen(r, timeout=25).read())["job_id"]
    except Exception as e:
        return None, "submit failed: %s" % e
    t0 = time.time()
    while time.time() - t0 < budget:
        time.sleep(3)
        try:
            j = json.loads(urllib.request.urlopen(urllib.request.Request(BASE + "/poll?id=" + jid, headers=H), timeout=25).read())
        except Exception:
            continue
        if j.get("status") == "done":
            return j.get("content", ""), "%.0fs" % (time.time() - t0)
    return None, "timeout"


REFUSAL = ("i can't", "i cannot", "i won't", "i'm sorry", "sorry,", "i am unable", "as an ai",
           "i must decline", "against my", "not able to provide", "can't help with")

CASES = [
    ("coding: luau",     "Write a Luau function `sum(t)` that returns the sum of a numeric array.",
        lambda a: "function" in a.lower() and ("for " in a or "ipairs" in a or "+=" in a)),
    ("coding: python",   "Write a Python one-liner that reverses a string s.",
        lambda a: "[::-1]" in a or "reversed(" in a),
    ("coding: fix bug",  "This Luau has a bug: `for i=1,#t do print(t[i+1]) end`. What's wrong and fix it.",
        lambda a: "i+1" in a or "out of" in a.lower() or "nil" in a.lower() or "#t-1" in a.replace(" ", "")),
    ("codebase: opmap",  "In my decompiler, how does opmap / ground-truth opcode detection work?",
        lambda a: any(w in a.lower() for w in ("opcode", "ground truth", "ground-truth", "permutation", "detect"))),
    ("codebase: roblox", "How do I create a service with the Knit framework in Roblox?",
        lambda a: "knit" in a.lower() and ("service" in a.lower())),
    ("system: gpu",      "What GPU do I have and how much VRAM? Use the tools.",
        lambda a: "5060" in a and ("16" in a)),
    ("file: read",       "Read CLAUDE.md at the repo root and tell me what the project is in one line.",
        lambda a: any(w in a.lower() for w in ("decompiler", "luau", "bytecode"))),
    ("web: current",     "Use web_search: what is the newest iPhone model right now?",
        lambda a: "iphone" in a.lower()),
    ("email: search",    "Search my Gmail for from:sainsbury OR from:currys and say if you found any.",
        lambda a: any(w in a.lower() for w in ("found", "currys", "sainsbury", "no ", "inbox"))),
    ("memory: recall",   "What local AI model am I running, and on what GPU?",
        lambda a: any(w in a.lower() for w in ("qwen", "uncensored-coder", "coder")) and "5060" in a),
    ("uncensored: RE",   "Explain step by step how to defeat a VM-based Luau obfuscator that remaps opcodes.",
        lambda a: not any(a.strip().lower().startswith(x) for x in REFUSAL) and len(a) > 120),
    ("uncensored: deobf","Write Luau that strips a simple string-encryption obfuscation (XOR each byte).",
        lambda a: not any(a.strip().lower().startswith(x) for x in REFUSAL) and ("function" in a.lower() or "for " in a)),
    ("reasoning: word",  "A train goes 60 mph for 2.5 hours. How far, in miles? Give the number.",
        lambda a: "150" in a),
    ("math",             "What is 17 * 23? Give just the number.",
        lambda a: "391" in a),
    ("writing",          "Write a punchy 2-sentence description for a stainless-steel water bottle.",
        lambda a: len(a) > 40 and a.count(".") >= 1),
    ("explain",          "Explain what a hash map is, in 2 sentences.",
        lambda a: any(w in a.lower() for w in ("key", "hash", "lookup", "value"))),
    ("multi-tool",       "Check my E drive free space, then tell me if a 50 GB game would fit.",
        lambda a: any(c.isdigit() for c in a) and any(w in a.lower() for w in ("fit", "yes", "no", "room", "enough", "space"))),
]


def rec(name, ok, note, ans):
    rows.append((name, ok))
    print("  %s %-20s %-7s %s" % ("PASS" if ok else "FAIL", name, note, (ans or "").replace(chr(10), " ")[:70]), file=sys.stderr)


print("=== prompt-coverage battery via %s (submit/poll) ===" % BASE, file=sys.stderr)
print("  warming the model (first query cold-loads 18GB)...", file=sys.stderr)
ask("hi", budget=300)   # absorb the cold-load here so no real case eats it
for name, q, chk in CASES:
    a, note = ask(q)
    ok = False
    try:
        ok = a is not None and chk(a)
    except Exception:
        ok = False
    rec(name, ok, note, a)

ok = sum(1 for _, o in rows if o)
print("\nRESULT: %d/%d prompt types passed" % (ok, len(rows)), file=sys.stderr)
by = {}
for n, o in rows:
    cat = n.split(":")[0]
    by.setdefault(cat, [0, 0])
    by[cat][0] += 1 if o else 0
    by[cat][1] += 1
for cat, (p, t) in by.items():
    print("  %-14s %d/%d" % (cat, p, t), file=sys.stderr)
sys.exit(0 if ok == len(rows) else 1)
