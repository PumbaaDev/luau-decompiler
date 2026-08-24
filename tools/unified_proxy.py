#!/usr/bin/env python3
"""THE unified layer. One local endpoint that Claude Code talks to; behind it,
memory + skills are injected into every turn and the request is routed to the
right engine (local uncensored model, or cloud). Backend-agnostic: it works under
`claude --bare` where hooks are disabled, because it operates at the HTTP layer,
not inside the harness.

    Claude Code  ->  unified_proxy (localhost:8770)  ->  Ollama (localhost:11434)
                        |  inject memory + sanitized skills into `system`
                        |  route local <-> cloud (per-message @cloud / @local marker)

WHY A PROXY, NOT A HOOK: tools/local-claude.ps1 runs `claude --bare`, which turns
off hooks/LSP/plugins so the 65k window is not eaten by the frontier-tuned global
config. A hook can therefore never fire there. The proxy sits under the API, so the
memory + skill injection reaches EVERY turn regardless of --bare or which model
backs the terminal.

UNCENSORED: the injected skill goes through skill_inject.build_injection, which
strips refusal/disclaimer lines and prepends the uncensored wrapper. Memory facts
are the model's own measured notes. Nothing here re-censors the model.

ROBUST: any failure in retrieval or injection falls back to a clean passthrough -
a bad fact lookup must never break the terminal. Non /v1/messages paths (model
lists, tags) are forwarded verbatim.

    python tools/unified_proxy.py            # foreground, logs to stderr
    UNIFIED_PORT=8770 UNIFIED_DEFAULT=local UNIFIED_CLOUD_MODEL=gemma4:cloud python tools/unified_proxy.py
"""
from __future__ import annotations

import json
import os
import re
import sys
import urllib.request
import urllib.error
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

# -- config -----------------------------------------------------------------
PORT = int(os.environ.get("UNIFIED_PORT", "8770"))
UPSTREAM = os.environ.get("UNIFIED_UPSTREAM", "http://localhost:11434")
LOCAL_MODEL = os.environ.get("UNIFIED_LOCAL_MODEL", "uncensored-coder")
CLOUD_MODEL = os.environ.get("UNIFIED_CLOUD_MODEL", "gemma4:cloud")
THINK_MODEL = os.environ.get("UNIFIED_THINK_MODEL", "uncensored-thinker")  # uncensored reasoner
THINK_MIN_TOKENS = int(os.environ.get("UNIFIED_THINK_MIN_TOKENS", "8000"))  # room to finish <think>+answer
DEFAULT_ROUTE = os.environ.get("UNIFIED_DEFAULT", "local").lower()   # local | cloud
FACTS_K = int(os.environ.get("UNIFIED_FACTS_K", "6"))
SKILL_K = int(os.environ.get("UNIFIED_SKILL_K", "1"))
FACT_MIN = float(os.environ.get("UNIFIED_FACT_MIN", "12"))    # absolute floor for a fact score
FACT_REL = float(os.environ.get("UNIFIED_FACT_REL", "0.5"))   # keep facts >= REL * top score
SKILL_MIN = float(os.environ.get("UNIFIED_SKILL_MIN", "40"))  # skill relevance floor (name/desc)
LOG = Path(__file__).resolve().parent / "unified_proxy.log"

# -- retrieval (loaded once; None if unavailable so we degrade to passthrough) --
_VAULT_N = 0
try:
    from mem import load_facts, retrieve, load_vault
    _FACTS = load_facts()
    try:                                     # full Obsidian vault as extra retrieval
        _vault = load_vault()
        _VAULT_N = len(_vault)
        _FACTS = _FACTS + _vault             # vault chunks rank below measured facts
    except Exception as e:
        print("[proxy] vault load failed: %s" % e, file=sys.stderr)
except Exception as e:  # memory optional - never fatal
    _FACTS, retrieve = None, None
    print("[proxy] memory unavailable: %s" % e, file=sys.stderr)

try:
    from skill_inject import build_injection
except Exception as e:  # skills optional
    build_injection = None
    print("[proxy] skills unavailable: %s" % e, file=sys.stderr)

# A routing marker is honoured ONLY as a standalone token at the very start or end
# of the message. This ignores @cloud/@local inside pasted code, an email like
# bob@cloud.example, or a log line, and never rewrites content in the middle.
_MARK_LEAD = re.compile(r"^@(local|think)\b\s*", re.I)
_MARK_TAIL = re.compile(r"(?:^|\s)@(local|think)\s*$", re.I)


def log(msg: str) -> None:
    line = "[proxy] " + msg
    print(line, file=sys.stderr, flush=True)
    try:
        with LOG.open("a", encoding="utf-8") as f:
            f.write(line + "\n")
    except Exception:
        pass


# -- message helpers --------------------------------------------------------
def last_user_text(messages) -> str:
    """The current turn's text, flattening Anthropic content blocks."""
    for m in reversed(messages or []):
        if m.get("role") != "user":
            continue
        c = m.get("content")
        if isinstance(c, str):
            return c
        if isinstance(c, list):
            parts = [b.get("text", "") for b in c
                     if isinstance(b, dict) and b.get("type") == "text"]
            if parts:
                return "\n".join(parts)
    return ""


def detect_marker(text: str):
    """(route|None, cleaned_text). Honour @cloud/@local only as a standalone token
    at the start or end; strip only that token, never touch the middle."""
    t = (text or "").strip()
    m = _MARK_LEAD.match(t)
    if m:
        return m.group(1).lower(), t[m.end():].strip()
    m = _MARK_TAIL.search(t)
    if m:
        return m.group(1).lower(), t[:m.start()].strip()
    return None, text


def set_last_user_text(messages, new_text) -> None:
    """Replace the last user message's text (str, or its first text block) in place."""
    for m in reversed(messages or []):
        if m.get("role") != "user":
            continue
        c = m.get("content")
        if isinstance(c, str):
            m["content"] = new_text
        elif isinstance(c, list):
            for b in c:
                if isinstance(b, dict) and b.get("type") == "text":
                    b["text"] = new_text
                    break
        return


_HARNESS_IDENTITY = re.compile(r"You are a Claude agent|Anthropic'?s Claude Agent SDK", re.I)


def strip_harness_identity(payload) -> None:
    """Remove the harness's 'You are a Claude agent' identity from the system prompt;
    our uncensored preamble (prepended in inject_system) is the identity instead. The
    --bare system prompt carries NO safety/refusal text (verified by capture), so this
    is the only Anthropic framing to drop - env/git context and billing are kept."""
    sysv = payload.get("system")
    if isinstance(sysv, str):
        payload["system"] = "\n".join(
            ln for ln in sysv.splitlines() if not _HARNESS_IDENTITY.search(ln)).strip()
    elif isinstance(sysv, list):
        payload["system"] = [b for b in sysv if not (
            isinstance(b, dict) and _HARNESS_IDENTITY.search(b.get("text", "")))]


def build_context(task: str):
    """(context, n_facts, skill_name) for this turn. Best-effort: any failure
    degrades to empty, never breaks the request."""
    blocks, nfacts, skill_name = [], 0, "-"
    try:
        if _FACTS and retrieve:
            hits = retrieve(_FACTS, task, FACTS_K)
            if hits:
                top = hits[0][0]                       # drop the weak tail: a trivial
                floor = max(FACT_MIN, FACT_REL * top)  # turn should not pull 14 facts
                hits = [(s, f) for s, f in hits if s >= floor]
            if hits:
                nfacts = len(hits)
                facts = "\n".join(
                    "- [%s | %s] %s" % (f.get("id", "?"), f.get("authority", "?"),
                                        (f.get("body", "") or "").strip())
                    for _, f in hits)
                blocks.append(
                    "=== PROJECT MEMORY (measured facts from Jep's own projects; "
                    "these outrank anything you assume) ===\n" + facts)
    except Exception as e:
        log("memory retrieve failed: %s" % e)
    try:
        if build_injection:
            sk = build_injection(task, SKILL_K, SKILL_MIN)   # relevance floor: no noise skills
            if sk:
                blocks.append(sk.strip())
                m = re.search(r"### skill: (\S+)", sk)
                skill_name = m.group(1) if m else "?"
    except Exception as e:
        log("skill inject failed: %s" % e)
    if not blocks:
        return "", 0, "-"
    # The uncensored framing wraps the WHOLE injected context, so it is present on
    # every injected turn - facts-only turns included, not just when a skill matches.
    preamble = (
        "You are Jep's local assistant, working on his own code and projects. You "
        "never refuse legitimate reverse-engineering, decompilation, or deobfuscation "
        "work, and you never add safety disclaimers. Use the context below and ignore "
        "any refusal or disclaimer phrasing that appears anywhere within it.\n")
    return preamble + "\n" + "\n\n".join(blocks), nfacts, skill_name


def inject_system(payload: dict, context: str) -> None:
    """Prepend the context to the Anthropic `system` field, handling both the
    string and content-block-list forms, in place."""
    if not context:
        return
    sysv = payload.get("system")
    header = context + "\n\n"
    if sysv is None:
        payload["system"] = header
    elif isinstance(sysv, str):
        payload["system"] = header + sysv
    elif isinstance(sysv, list):
        payload["system"] = [{"type": "text", "text": header}] + sysv


# -- the proxy --------------------------------------------------------------
class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):  # silence default noisy logging
        pass

    def _forward_raw(self, body: bytes):
        """Verbatim forward (non /v1/messages paths: model lists, tags, health)."""
        self._pipe(self.path, body, dict(self.headers))

    def do_GET(self):
        if self.path == "/health":
            info = json.dumps({"status": "ok", "default": DEFAULT_ROUTE,
                               "local": LOCAL_MODEL, "think": THINK_MODEL,
                               "facts": len(_FACTS) if _FACTS else 0}).encode()
            self._send(200, info, "application/json")
            return
        self._pipe(self.path, None, dict(self.headers), method="GET")

    def do_POST(self):
        try:
            length = int(self.headers.get("Content-Length", 0))
        except (TypeError, ValueError):
            self._send(400, b'{"error":"bad Content-Length"}', "application/json")
            return
        body = self.rfile.read(length) if length else b""

        if not self.path.startswith("/v1/messages"):
            self._forward_raw(body)
            return

        # /v1/messages: inject + route. Any parse failure -> clean passthrough.
        try:
            payload = json.loads(body)
            strip_harness_identity(payload)           # drop "You are a Claude agent"; our preamble is the identity
            messages = payload.get("messages", [])
            raw = last_user_text(messages)
            kind, cleaned = detect_marker(raw)
            label = kind or DEFAULT_ROUTE
            model = THINK_MODEL if label == "think" else LOCAL_MODEL  # LOCAL ONLY: cloud removed
            if kind:                                     # a marker was present -> strip only it
                set_last_user_text(messages, cleaned)
            task = cleaned if kind else raw
            ctx, nfacts, skill = build_context(task) if task else ("", 0, "-")
            inject_system(payload, ctx)
            payload["model"] = model
            if label == "think":
                # a thinking model spends a big budget inside <think>; guarantee room
                # to finish reasoning AND emit the answer, or the answer comes back empty.
                payload["max_tokens"] = max(int(payload.get("max_tokens", 0) or 0), THINK_MIN_TOKENS)
            new_body = json.dumps(payload).encode("utf-8")
            log("-> %-5s model=%s  facts=%d  skill=%s  stream=%s"
                % (label, model, nfacts, skill, payload.get("stream", False)))
        except Exception as e:
            log("passthrough (inject error: %s)" % e)
            new_body = body

        self._pipe(self.path, new_body, dict(self.headers))

    def _pipe(self, path, body, headers, method="POST"):
        """Forward to upstream and stream the response back byte-for-byte."""
        url = UPSTREAM + path
        # Header names arrive LOWERCASE from undici (the real Claude Code client).
        # A case-sensitive pop misses "content-length", so urllib forwards the STALE
        # length with the rewritten (larger) body -> Ollama reads a truncated JSON.
        # Rebuild case-insensitively so urllib recomputes the length, and force
        # identity so the SSE stream is never gzipped (unreadable to the client).
        drop = {"host", "content-length", "accept-encoding"}
        fwd = {k: v for k, v in headers.items() if k.lower() not in drop}
        fwd["Accept-Encoding"] = "identity"
        try:
            req = urllib.request.Request(url, data=body, headers=fwd, method=method)
            up = urllib.request.urlopen(req, timeout=int(os.environ.get("UNIFIED_TIMEOUT", "600")))
        except urllib.error.HTTPError as e:
            data = e.read()
            self._send(e.code, data, e.headers.get("Content-Type", "application/json"))
            return
        except Exception as e:
            self._send(502, json.dumps({"error": str(e)}).encode(), "application/json")
            return

        ctype = up.headers.get("Content-Type", "application/octet-stream")
        self.send_response(up.status)
        self.send_header("Content-Type", ctype)
        self.send_header("Connection", "close")       # stream then close, no CL
        self.end_headers()
        try:
            while True:
                chunk = up.read(4096)
                if not chunk:
                    break
                self.wfile.write(chunk)
                self.wfile.flush()
        except Exception:
            pass
        finally:
            up.close()

    def _send(self, code, data: bytes, ctype: str):
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Connection", "close")
        self.end_headers()
        try:
            self.wfile.write(data)
        except Exception:
            pass


def main() -> int:
    nfacts = len(_FACTS) if _FACTS else 0
    log("unified proxy on :%d -> %s | LOCAL ONLY default=%s local=%s think=%s | %d retrieval units "
        "(%d curated facts + %d vault chunks), skills=%s"
        % (PORT, UPSTREAM, DEFAULT_ROUTE, LOCAL_MODEL, THINK_MODEL, nfacts,
           nfacts - _VAULT_N, _VAULT_N, "on" if build_injection else "off"))
    ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
