#!/usr/bin/env python3
"""Test the phone's REAL path end-to-end: through the Tailscale Serve URL (not localhost), using the
same submit(/ask)+poll(/poll) flow the chat page uses. This is the path that was failing; test it."""
import json, os, sys, time, urllib.request, urllib.error

BASE = os.environ.get("SERVE_URL", "http://jepy.tail1b4f8e.ts.net:8772")
TOK = os.environ.get("PHONE_AGENT_TOKEN", "")
H = {"Content-Type": "application/json", "Authorization": "Bearer " + TOK}
rows = []


def _req(path, data=None, headers=None, timeout=25):
    r = urllib.request.Request(BASE + path, data=data, headers=headers or {},
                               method="POST" if data is not None else "GET")
    try:
        with urllib.request.urlopen(r, timeout=timeout) as x:
            return x.status, x.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as e:
        return e.code, ""
    except Exception as e:
        return -1, str(e)


def ask_poll(q, budget=260):
    """Submit a question and poll to completion, exactly like the phone page."""
    st, body = _req("/ask", json.dumps({"messages": [{"role": "user", "content": q}]}).encode(), H)
    if st != 200:
        return None, "submit HTTP %d" % st
    jid = json.loads(body)["job_id"]
    t0 = time.time()
    while time.time() - t0 < budget:
        time.sleep(3)
        st, body = _req("/poll?id=" + jid, None, H)
        if st != 200:
            return None, "poll HTTP %d" % st
        j = json.loads(body)
        if j.get("status") == "done":
            return j.get("content", ""), "%.0fs" % (time.time() - t0)
    return None, "timeout"


def rec(name, ok, note):
    rows.append((name, ok, note))
    print("  %s %-26s %s" % ("PASS" if ok else "FAIL", name, note[:80]), file=sys.stderr)


print("=== via Tailscale Serve: %s ===" % BASE, file=sys.stderr)
# 1. chat page loads
st, body = _req("/", None, {})
rec("chat page GET /", st == 200 and "Jep's AI" in body, "HTTP %d" % st)
# 2. auth: /ask without token -> 401
st, _ = _req("/ask", b'{"messages":[]}', {"Content-Type": "application/json"})
rec("auth: no token -> 401", st == 401, "HTTP %d" % st)
# 3. auth: /ask wrong token -> 401
st, _ = _req("/ask", b'{"messages":[]}', {"Content-Type": "application/json", "Authorization": "Bearer wrong"})
rec("auth: wrong token -> 401", st == 401, "HTTP %d" % st)
# 4. quick query
a, note = ask_poll("In one word, what is 2+2? Just the number.")
rec("quick query (submit+poll)", a is not None and "4" in (a or ""), note + " | " + (a or "")[:40])
# 5. system/disk tool
a, note = ask_poll("How much free space is on my E drive? Use the tools.")
rec("system tool (E: disk)", a is not None and any(c.isdigit() for c in (a or "")), note + " | " + (a or "")[:50])
# 6. read_file tool
a, note = ask_poll("Read crates/luau-core/Cargo.toml and tell me the package name.")
rec("read_file tool", a is not None and "luau-core" in (a or ""), note + " | " + (a or "")[:50])
# 7. email tool
a, note = ask_poll("Search my Gmail for from:currys and give the subject of one.")
rec("email tool (Gmail)", a is not None and "currys" in (a or "").lower(), note + " | " + (a or "")[:50])
# 8. web tool
a, note = ask_poll("Use web_search to find the official Luau language website URL.")
rec("web_search tool", a is not None and "luau" in (a or "").lower(), note + " | " + (a or "")[:50])
# 9. find_in_files tool (new): literal code search
a, note = ask_poll("Use find_in_files to find where 'def run_agent' is defined in the tools folder.")
rec("find_in_files tool", a is not None and "phone_agent" in (a or "").lower(), note + " | " + (a or "")[:50])
# 10. remember_fact tool (new): must not error (persistence verified out-of-band)
a, note = ask_poll("Use remember_fact to remember that Jep's favourite test marker is Zeta-Quokka-7788.")
rec("remember_fact tool", a is not None and any(w in (a or "").lower() for w in ("saved", "remember", "quokka")),
    note + " | " + (a or "")[:50])

ok = sum(1 for _, o, _ in rows if o)
print("\nRESULT (real phone path via Serve): %d/%d passed" % (ok, len(rows)), file=sys.stderr)
sys.exit(0 if ok == len(rows) else 1)
