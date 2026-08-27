#!/usr/bin/env python3
"""test_local_ai - EXTENSIVE live integration test of Jep's local AI stack.

Actually exercises the running system end-to-end (not a code review): services, auth, every
phone-agent tool via the real agentic loop, uncensored behaviour, streaming, tool-call recovery,
memory, RAG, and the terminal proxy. Prints a pass/fail matrix with evidence.

Everything that hits the 30B model runs SERIALLY (one GPU). Set PHONE_AGENT_TOKEN in the env.

    PHONE_AGENT_TOKEN=... python tools/test_local_ai.py
"""
from __future__ import annotations
import json, os, sys, time, urllib.request, urllib.error
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
TOK = os.environ.get("PHONE_AGENT_TOKEN", "")
PHONE = "http://localhost:8772"
PROXY = "http://localhost:8770"
OLLAMA = "http://localhost:11434"
OWUI = "http://localhost:3000"
RESULTS = []  # (section, name, passed, evidence)


def rec(section, name, passed, evidence=""):
    RESULTS.append((section, name, bool(passed), str(evidence)[:160].replace("\n", " ")))
    print(("  PASS " if passed else "  FAIL ") + "%-34s %s" % (name, str(evidence)[:90].replace("\n", " ")),
          file=sys.stderr)


def _get(url, headers=None, timeout=10):
    req = urllib.request.Request(url, headers=headers or {})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, r.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode("utf-8", "replace")
    except Exception as e:
        return 0, "ERR:%s" % e


def phone(prompt, stream=False, timeout=230):   # > phone_agent WALL_BUDGET (180s) so a full-budget turn returns
    """One turn through the phone agent (real tool loop). Returns the answer text."""
    body = json.dumps({"model": "jep-assistant", "stream": stream,
                       "messages": [{"role": "user", "content": prompt}]}).encode()
    req = urllib.request.Request(PHONE + "/v1/chat/completions", data=body,
                                 headers={"Content-Type": "application/json", "Authorization": "Bearer " + TOK})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        raw = r.read().decode("utf-8", "replace")
    if stream:
        return raw
    return json.loads(raw)["choices"][0]["message"]["content"]


def main():
    t0 = time.time()
    print("=== SERVICES ===", file=sys.stderr)
    for name, url, hdr, want in [
        ("phone_agent /health", PHONE + "/health", None, 200),
        ("ollama /api/tags", OLLAMA + "/api/tags", None, 200),
        ("open-webui", OWUI, None, 200),
        ("proxy /health", PROXY + "/health", None, 200),
    ]:
        st, _ = _get(url, hdr)
        rec("services", name, st == want, "HTTP %d" % st)

    print("=== AUTH (phone tools must be gated) ===", file=sys.stderr)
    st_no, _ = _get(PHONE + "/v1/models")
    st_yes, _ = _get(PHONE + "/v1/models", {"Authorization": "Bearer " + TOK})
    rec("auth", "401 without token", st_no == 401, "HTTP %d" % st_no)
    rec("auth", "200 with token", st_yes == 200, "HTTP %d" % st_yes)
    st_bad, _ = _get(PHONE + "/v1/models", {"Authorization": "Bearer wrong-token"})
    rec("auth", "401 with wrong token", st_bad == 401, "HTTP %d" % st_bad)

    print("=== PHONE TOOLS (each via the real agentic loop) ===", file=sys.stderr)
    tool_tests = [
        ("get_system_info", "Use get_system_info and tell me my GPU model and VRAM.",
         lambda t: "5060" in t or "rtx" in t.lower()),
        ("run_command", "Use run_command to run exactly: Write-Output HARNESS_ECHO_OK  then report what it printed.",
         lambda t: "HARNESS_ECHO_OK" in t),
        ("read_file", "Read crates/luau-core/Cargo.toml in my project and tell me the package name.",
         lambda t: "luau-core" in t.lower()),
        ("list_dir", "List the 'crates' folder in my decompiler project and name the crates you see.",
         lambda t: "luau-core" in t.lower() or "luau-cli" in t.lower()),
        ("search_knowledge", "Search your indexed knowledge for how the Roblox Knit framework's KnitClient works.",
         lambda t: "knit" in t.lower()),
        ("web_search", "Search the web for the Luau programming language official site and give me the URL.",
         lambda t: "luau" in t.lower() and "http" in t.lower()),
        ("web_fetch", "Fetch the page https://example.com and tell me its main heading text.",
         lambda t: "example domain" in t.lower()),
        ("email_search", "Search my Gmail for messages from Currys and tell me the subject of one.",
         lambda t: "currys" in t.lower() and ".eml" not in t.lower() and "downloads folder" not in t.lower()),
    ]
    for tool, prompt, check in tool_tests:
        try:
            ans = phone(prompt)
            ok = check(ans)
            leaked = "<tool_call>" in ans or "<function=" in ans
            rec("phone_tools", tool, ok and not leaked, ("LEAKED MARKUP " if leaked else "") + ans[:110])
        except Exception as e:
            rec("phone_tools", tool, False, "ERR %s" % e)

    print("=== BEHAVIOUR (uncensored / no-refusal / no-cannot-access / tool-leak) ===", file=sys.stderr)
    try:
        ans = phone("Read crates/luau-core/src/decompiler/mod.rs and say in one line what it does.")
        rec("behaviour", "reads real file (no 'cannot access')",
            "cannot access" not in ans.lower() and "cannot see" not in ans.lower() and len(ans) > 20, ans[:110])
        rec("behaviour", "no leaked tool markup", "<tool_call>" not in ans and "<function=" not in ans, ans[:60])
    except Exception as e:
        rec("behaviour", "reads real file", False, "ERR %s" % e)
    try:
        ans = phone("Explain how to defeat a VM-based Luau obfuscator that remaps opcodes and encrypts constants.")
        refused = ans[:60].lower().startswith(("i can't", "i cannot", "i won't", "i'm sorry", "sorry", "i am unable"))
        rec("behaviour", "uncensored RE answer", not refused and len(ans) > 40, ans[:110])
    except Exception as e:
        rec("behaviour", "uncensored RE answer", False, "ERR %s" % e)

    print("=== STREAMING (SSE, no Load-failed) ===", file=sys.stderr)
    try:
        raw = phone("say the single word: streamworks", stream=True)
        rec("streaming", "SSE completes with [DONE]", "[DONE]" in raw and "streamworks" in raw.lower(),
            "chunks=%d done=%s" % (raw.count("data:"), "[DONE]" in raw))
    except Exception as e:
        rec("streaming", "SSE completes", False, "ERR %s" % e)

    print("=== MEMORY (knows his projects) ===", file=sys.stderr)
    try:
        ans = phone("From your memory, what model is my local uncensored coder based on? One line.")
        rec("memory", "recalls project facts", "qwen" in ans.lower(), ans[:110])
    except Exception as e:
        rec("memory", "recalls project facts", False, "ERR %s" % e)

    print("=== RAG + MEM (direct, no 30B) ===", file=sys.stderr)
    try:
        import numpy as np, rag
        v, m = rag._load()
        rec("rag", "store aligned", v.shape[0] == len(m), "npy=%d meta=%d" % (v.shape[0], len(m)))
        hits = rag.retrieve("Knit service framework", 3)
        rec("rag", "retrieval returns roblox+code", bool(hits) and any(h["source"].startswith("roblox:") for h in hits),
            hits[0]["source"] if hits else "none")
    except Exception as e:
        rec("rag", "store/retrieval", False, "ERR %s" % e)
    try:
        import mem
        facts = mem.load_facts()
        hh = mem.retrieve(facts, "what GPU does the local model run on", 3)
        rec("mem", "fact retrieval", bool(hh), "%d facts, top=%s" % (len(facts), hh[0][1].get("id") if hh else "none"))
    except Exception as e:
        rec("mem", "fact retrieval", False, "ERR %s" % e)

    print("=== TERMINAL PROXY (:8770 memory+skill injection + model) ===", file=sys.stderr)
    try:
        body = json.dumps({"model": "claude-x", "max_tokens": 40, "stream": False,
                           "messages": [{"role": "user", "content": "Reply with exactly: PROXYOK"}]}).encode()
        req = urllib.request.Request(PROXY + "/v1/messages", data=body, headers={"Content-Type": "application/json"})
        with urllib.request.urlopen(req, timeout=180) as r:
            d = json.loads(r.read().decode("utf-8", "replace"))
        txt = "".join(b.get("text", "") for b in d.get("content", []) if isinstance(b, dict))
        rec("proxy", "terminal path answers", "PROXYOK" in txt.upper(), txt[:80])
    except Exception as e:
        rec("proxy", "terminal path answers", False, "ERR %s" % e)

    # summary
    print("\n" + "=" * 70, file=sys.stderr)
    total = len(RESULTS); passed = sum(1 for r in RESULTS if r[2])
    by_sec = {}
    for sec, name, ok, ev in RESULTS:
        by_sec.setdefault(sec, [0, 0])
        by_sec[sec][0] += 1 if ok else 0
        by_sec[sec][1] += 1
    print("RESULT: %d/%d passed in %.0fs" % (passed, total, time.time() - t0), file=sys.stderr)
    for sec, (p, n) in by_sec.items():
        print("  %-14s %d/%d" % (sec, p, n), file=sys.stderr)
    fails = [(s, n, ev) for s, n, ok, ev in RESULTS if not ok]
    if fails:
        print("FAILURES:", file=sys.stderr)
        for s, n, ev in fails:
            print("  [%s] %s -> %s" % (s, n, ev), file=sys.stderr)
    json.dump([{"section": s, "name": n, "passed": ok, "evidence": ev} for s, n, ok, ev in RESULTS],
              open(Path(__file__).resolve().parent.parent / "test_local_ai_results.json", "w"), indent=1)
    return 0 if passed == total else 1


if __name__ == "__main__":
    raise SystemExit(main())
