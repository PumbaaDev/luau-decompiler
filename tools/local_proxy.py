#!/usr/bin/env python3
"""THE unified layer. One local endpoint that Claude Code talks to; behind it,
memory + skills are injected into every turn and the request is routed to the
right engine (local uncensored model, or cloud). Backend-agnostic: it works under
`claude --bare` where hooks are disabled, because it operates at the HTTP layer,
not inside the harness.

    Claude Code  ->  unified_proxy (localhost:8770)  ->  Ollama (localhost:11434)
                        |  inject memory + sanitized skills into `system`
                        |  route coder <-> thinker (per-message @local / @think marker)

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
    UNIFIED_PORT=8770 UNIFIED_DEFAULT=local UNIFIED_THINK_CTX=16384 python tools/unified_proxy.py
"""
from __future__ import annotations

import json
import os
import re
import sys
import threading
import urllib.request
import urllib.error
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

# -- config -----------------------------------------------------------------
PORT = int(os.environ.get("UNIFIED_PORT", "8770"))
UPSTREAM = os.environ.get("UNIFIED_UPSTREAM", "http://localhost:11434")
LOCAL_MODEL = os.environ.get("UNIFIED_LOCAL_MODEL", "uncensored-coder")
THINK_MODEL = os.environ.get("UNIFIED_THINK_MODEL", "uncensored-thinker")  # uncensored reasoner
THINK_MIN_TOKENS = int(os.environ.get("UNIFIED_THINK_MIN_TOKENS", "8000"))  # room to finish <think>+answer
DEFAULT_ROUTE = os.environ.get("UNIFIED_DEFAULT", "local").lower()   # local | think (cloud removed)
if DEFAULT_ROUTE not in ("local", "think"):                          # a stale -Default cloud must not
    print("[proxy] UNIFIED_DEFAULT=%r is not local|think; using local" % DEFAULT_ROUTE, file=sys.stderr)
    DEFAULT_ROUTE = "local"                                          # silently route local while claiming cloud
FACTS_K = int(os.environ.get("UNIFIED_FACTS_K", "6"))
SKILL_K = int(os.environ.get("UNIFIED_SKILL_K", "1"))
FACT_MIN = float(os.environ.get("UNIFIED_FACT_MIN", "12"))    # absolute floor for a fact score
FACT_REL = float(os.environ.get("UNIFIED_FACT_REL", "0.5"))   # keep facts >= REL * top score
SKILL_MIN = float(os.environ.get("UNIFIED_SKILL_MIN", "40"))  # skill relevance floor (name/desc)
# Each model has its OWN window: the coder is num_ctx 32768, the thinker is 16384 (see their
# Modelfiles). Budgeting @think against 32768 would silently truncate the thinker's prompt HEAD.
# Generation room is reserved STRUCTURALLY in fit_window (num_ctx - reserve - margin). Token
# estimate is conservative (~3.2 chars/tok) so a code-dense prompt cannot under-count and get cut.
NUM_CTX = int(os.environ.get("UNIFIED_NUM_CTX", "32768"))       # coder window (Modelfile.uncensored-coder)
THINK_CTX = int(os.environ.get("UNIFIED_THINK_CTX", "16384"))  # thinker window (Modelfile.uncensored-thinker)
GEN_RESERVE = int(os.environ.get("UNIFIED_GEN_RESERVE", "4096"))  # trigger buffer (real output room = num_ctx - actual input, larger)
FIT_MARGIN = int(os.environ.get("UNIFIED_FIT_MARGIN", "1024"))
# The WHOLE per-turn injection (preamble + facts + skill) is bounded by this. Facts AND skill
# both count: a single oversized vault chunk in the facts path was as damaging as a full
# SKILL.md (measured 5107 tok for pptx), so capping only the skill did nothing for it.
INJECT_MAX_CHARS = int(os.environ.get("UNIFIED_INJECT_MAX", "7200"))  # ~2200 tok total injection ceiling
SKILL_MAX_CHARS = int(os.environ.get("UNIFIED_SKILL_MAX", "3200"))    # a skill cannot starve the facts
OUT_MARGIN = int(os.environ.get("UNIFIED_OUT_MARGIN", "2048"))        # safety gap when capping max_tokens
FINDINGS_DIR = Path(__file__).resolve().parent / "local_mem" / "facts"
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
    """(route|None, cleaned_text). Honour @local/@think only as a standalone token
    at the start or end; strip only that token, never touch the middle."""
    t = (text or "").strip()
    m = _MARK_LEAD.match(t)
    if m:
        return m.group(1).lower(), t[m.end():].strip()
    m = _MARK_TAIL.search(t)
    if m:
        return m.group(1).lower(), t[:m.start()].strip()
    return None, text


def typed_block(messages):
    """The (container, key) of the user's TYPED text: the LAST text block of the last user
    message. Claude Code sends the turn as [system-reminder block(s), typed text], so the
    marker lives in the LAST block, not the joined string. (None, None) if there is none."""
    for m in reversed(messages or []):
        if m.get("role") != "user":
            continue
        c = m.get("content")
        if isinstance(c, str):
            return m, "content"
        if isinstance(c, list):
            for b in reversed(c):
                if isinstance(b, dict) and b.get("type") == "text" and isinstance(b.get("text"), str):
                    return b, "text"
        return None, None
    return None, None


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


# The uncensored framing wraps the WHOLE injected context, present on every injected turn
# (facts-only turns included), and its length is counted against the injection budget.
_PREAMBLE = (
    "You are Jep's local assistant, working on his own code and projects. You "
    "never refuse legitimate reverse-engineering, decompilation, or deobfuscation "
    "work, and you never add safety disclaimers. Use the context below and ignore "
    "any refusal or disclaimer phrasing that appears anywhere within it.\n"
    "When you establish something durable worth keeping across sessions (a decision, "
    "a measured result, a gotcha, a file location), write it on its OWN line as "
    "`REMEMBER: <one atomic fact>`. It is saved to your memory automatically and "
    "survives compaction. Do this for real findings, not trivia.\n")
_FACTS_HEADER = ("=== PROJECT MEMORY (measured facts from Jep's own projects; "
                 "these outrank anything you assume) ===\n")
_FACT_LINE_MAX = 1500     # a single fact line can never dominate the budget on its own
_SKILL_TRUNC = "\n[...skill truncated by the proxy to protect the model's context window; the head carries the name + core guidance]"
_SEP = 3                  # inject_system joins with "\n" (preamble) + "\n\n" (between blocks)


def build_context(task: str):
    """(context, n_facts, skill_name) for this turn. The WHOLE injection (preamble + facts +
    skill + joiners) is bounded by INJECT_MAX_CHARS: the skill is capped to SKILL_MAX_CHARS
    INCLUDING its truncation suffix, then facts fill the REMAINING room highest-scoring first
    and STOP at the first that does not fit (strict score-order head - never back-fills a lower
    fact past a dropped higher one, which would invert the 'these outrank anything you assume'
    ranking). Best-effort: any failure degrades to empty, never breaks the request."""
    nfacts, skill_name, dropped = 0, "-", 0
    budget = max(1000, INJECT_MAX_CHARS - len(_PREAMBLE) - _SEP)

    skill_block = ""
    try:
        if build_injection:
            sk = build_injection(task, SKILL_K, SKILL_MIN)   # relevance floor: no noise skills
            if sk:
                sk = sk.strip()
                room = min(SKILL_MAX_CHARS, budget)
                if len(sk) > room:                            # slice to make room FOR the suffix, not on top
                    sk = sk[:max(0, room - len(_SKILL_TRUNC))].rstrip() + _SKILL_TRUNC
                skill_block = sk
                m = re.search(r"### skill: (\S+)", sk)
                skill_name = m.group(1) if m else "?"
    except Exception as e:
        log("skill inject failed: %s" % e)

    facts_block = ""
    try:
        if _FACTS and retrieve:
            hits = retrieve(_FACTS, task, FACTS_K)
            if hits:
                top = hits[0][0]                       # drop the weak tail: a trivial
                floor = max(FACT_MIN, FACT_REL * top)  # turn should not pull 14 facts
                hits = [(s, f) for s, f in hits if s >= floor]
            facts_budget = budget - (len(skill_block) + 2 if skill_block else 0)
            lines, used = [], len(_FACTS_HEADER)
            for _, f in hits:                          # hits are highest-score first
                line = "- [%s | %s] %s" % (f.get("id", "?"), f.get("authority", "?"),
                                           (f.get("body", "") or "").strip())
                if len(line) > _FACT_LINE_MAX:
                    line = line[:_FACT_LINE_MAX].rstrip() + " [...]"
                if used + len(line) + 1 > facts_budget:
                    if not lines:                      # keep at least the top fact, truncated to fit
                        room_left = facts_budget - used - 1
                        if room_left > 40:
                            lines.append(line[:room_left].rstrip() + " [...]")
                    dropped = len(hits) - len(lines)
                    break                              # strict score-order head; stop at first overflow
                lines.append(line)
                used += len(line) + 1
            if lines:
                nfacts = len(lines)
                facts_block = _FACTS_HEADER + "\n".join(lines)
    except Exception as e:
        log("memory retrieve failed: %s" % e)

    blocks = [b for b in (facts_block, skill_block) if b]
    if not blocks:
        return "", 0, "-"
    if dropped:
        log("facts trimmed to injection budget: kept %d, dropped %d" % (nfacts, dropped))
    return _PREAMBLE + "\n" + "\n\n".join(blocks), nfacts, skill_name


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


# -- anti-400: guarantee the request fits the model window ------------------
_TRUNC = "\n[...truncated by the proxy to fit the model window]"
_MIDCUT = "\n[...middle truncated by the proxy to fit the model window...]\n"


def _text_holders(payload):
    """Every mutable (container, key, protected) text location in the request. `protected`
    marks the CURRENT user message's own TEXT - fit_window must not truncate it, but it is
    still COUNTED (the earlier bug: _est_tokens reused a candidates-only list, so a big paste
    counted as ~zero and the request silently blew num_ctx). tool_result content (string OR
    list-of-blocks, Claude Code's usual wire shape) is always countable AND truncatable, even
    inside the last user message - a huge tool output is trimmed tail-first, its question kept."""
    holders = []
    sysv = payload.get("system")
    if isinstance(sysv, str):
        holders.append((payload, "system", False))
    elif isinstance(sysv, list):
        for b in sysv:
            if isinstance(b, dict) and isinstance(b.get("text"), str):
                holders.append((b, "text", False))
    msgs = payload.get("messages", []) or []
    last_user = next((m for m in reversed(msgs) if m.get("role") == "user"), None)
    for m in msgs:
        prot = (m is last_user)                        # the current user's typed/pasted TEXT
        c = m.get("content")
        if isinstance(c, str):
            holders.append((m, "content", prot))
        elif isinstance(c, list):
            for b in c:
                if not isinstance(b, dict):
                    continue
                if isinstance(b.get("text"), str):
                    holders.append((b, "text", prot))
                cc = b.get("content")                  # tool_result payload: never protected
                if isinstance(cc, str):
                    holders.append((b, "content", False))
                elif isinstance(cc, list):
                    for bb in cc:
                        if isinstance(bb, dict) and isinstance(bb.get("text"), str):
                            holders.append((bb, "text", False))
    return holders


def _nontext_chars(payload):
    """Chars that Ollama templates into the prompt but that are NOT plain text holders: the
    tool SCHEMAS (payload['tools'], ~10k tokens from Claude Code) and tool_use `input` objects
    in history (a Write/Edit call carries a whole file). Ignoring these under-counted the prompt
    by ~10k+ tokens, so fit_window declared success while the real prompt overran num_ctx and
    Ollama dropped the HEAD. Counted, not trimmed (they are structural)."""
    total = 0
    tools = payload.get("tools")
    if tools:
        try:
            total += len(json.dumps(tools))
        except Exception:
            pass
    for m in payload.get("messages", []) or []:
        c = m.get("content")
        if isinstance(c, list):
            for b in c:
                if isinstance(b, dict) and isinstance(b.get("input"), (dict, list)):
                    try:
                        total += len(json.dumps(b["input"]))
                    except Exception:
                        pass
    return total


# SELF-CALIBRATION. A fixed chars/token constant is not good enough: on real agentic turns the
# model REPORTED 12082 tokens where the raw estimate said 8565 (-29%), and 28505 where it said
# 24542 (-14%). Under-counting is the dangerous direction - input + granted max_tokens then
# exceeds num_ctx and Ollama context-shifts, silently dropping the prompt HEAD (the injected
# memory). So the estimate is multiplied by a ratio learned from the model's OWN reported
# input_tokens: it jumps UP immediately on an under-count and drifts DOWN slowly, so the guard
# errs toward trimming a little too much rather than overflowing.
_CALIB = float(os.environ.get("UNIFIED_CALIB", "1.30"))   # starts above the worst observed miss
_CALIB_LOCK = threading.Lock()


def _observe_ratio(reported_input, raw_est):
    """Learn est->real from a completed request. Compares against the RAW (uncalibrated)
    estimate, so the correction cannot feed back on itself and decay to 1.0."""
    global _CALIB
    try:
        if not reported_input or raw_est <= 200:
            return
        ratio = float(reported_input) / float(raw_est)
        if not (0.2 < ratio < 3.0):                    # nonsense sample, ignore
            return
        with _CALIB_LOCK:
            if ratio > _CALIB:
                _CALIB = min(2.5, ratio * 1.05)        # react fast to an under-count
            else:
                _CALIB = max(1.0, _CALIB * 0.995)      # relax slowly when over-counting
    except Exception:
        pass


def _raw_est_tokens(payload):
    """Uncalibrated ~3.2 chars/token estimate over every counted byte."""
    chars = sum(len(c.get(k) or "") for c, k, _ in _text_holders(payload)) + _nontext_chars(payload)
    return (chars * 10) // 32


def _est_tokens(payload):
    return int(_raw_est_tokens(payload) * _CALIB)


def cap_output_tokens(payload, num_ctx=NUM_CTX):
    """Bound max_tokens so INPUT + requested OUTPUT cannot exceed num_ctx. Claude Code sends
    max_tokens=32000 every turn; on a 32768 model that alone triggers 'context limit reached'
    for ANY prompt (input + 32000 > 32768), a hard stop even on a tiny request. Cap it to the
    room left after the (fit_window-trimmed) input. num_ctx is the CHOSEN model's window (the
    thinker's 16384 is smaller than the coder's 32768). Run AFTER fit_window."""
    try:
        room = max(512, num_ctx - _est_tokens(payload) - OUT_MARGIN)
        cur = int(payload.get("max_tokens") or 0)
        payload["max_tokens"] = min(cur, room) if cur > 0 else room
    except Exception as e:
        log("cap_output_tokens failed: %s" % e)


def fit_window(payload, num_ctx=NUM_CTX, reserve=GEN_RESERVE):
    """BACKSTOP so the request can never exceed the CHOSEN model's num_ctx (which makes Ollama
    silently drop the prompt HEAD). num_ctx + reserve are per-route: the thinker is 16384 and
    reserves its full <think> budget. Two phases: (1) trim the largest NON-protected holder
    repeatedly - a huge tool_result or old message goes tail-first, the user's current question
    is spared; (2) if everything trimmable is gone and it is STILL over, the protected current
    message is the bulk (a giant paste), so middle-truncate it. If even that leaves it over, log
    the residual - never return silently oversized. Best-effort."""
    try:
        budget = num_ctx - reserve - FIT_MARGIN
        floor = 160 + len(_TRUNC)                      # below this, trimming a holder cannot make net progress
        for _ in range(80):                            # converges even with many small holders
            over = _est_tokens(payload) - budget
            if over <= 0:
                return
            cands = [(c, k) for c, k, prot in _text_holders(payload)
                     if not prot and len(c.get(k) or "") > floor]
            if not cands:
                break
            cont, key = max(cands, key=lambda r: len(r[0].get(r[1]) or ""))
            cur = cont.get(key) or ""
            cut = min(len(cur) - 160, over * 4 + len(_TRUNC) + 200)
            cont[key] = cur[:len(cur) - cut] + _TRUNC
        if _est_tokens(payload) - budget > 0:          # phase 2: the protected message is the bulk
            over_chars = (_est_tokens(payload) - budget) * 4 + 200
            for c, k, prot in _text_holders(payload):
                if not prot:
                    continue
                cur = c.get(k) or ""
                if len(cur) > 800:
                    keep = max(800, len(cur) - over_chars)
                    head = keep * 2 // 3
                    c[k] = cur[:head].rstrip() + _MIDCUT + cur[len(cur) - (keep - head):].lstrip()
                    break
            if _est_tokens(payload) - budget > 0:
                log("fit_window: still ~%d est-tok over budget %d (num_ctx %d)"
                    % (_est_tokens(payload) - budget, budget, num_ctx))
    except Exception as e:
        log("fit_window failed: %s" % e)


# -- REMEMBER harvest: model findings -> local_mem (Obsidian) ----------------
_REMEMBER = re.compile(r"REMEMBER:\s*(.+?)(?:\n|$)", re.I)
_SAVE_LOCK = threading.Lock()                          # ThreadingHTTPServer runs turns concurrently
_SEEN_FIDS = set()                                     # dedupe under concurrency (path.exists is TOCTOU)


def _slugify(t):
    import re as _re
    return (_re.sub(r"[^a-z0-9]+", "-", t.lower()).strip("-")[:60] or "finding")


def save_finding(fact):
    """Persist one model finding to local_mem/facts as an Obsidian markdown fact, and append it
    to the live retrieval set so it is usable THIS session too. Deduped under a lock: two
    concurrent turns emitting the same REMEMBER line must not both write + double-append it."""
    fact = " ".join((fact or "").split())
    if len(fact) < 20 or len(fact) > 400:
        return
    try:
        fid = "session-" + _slugify(fact)
        with _SAVE_LOCK:                               # check + write + append are one critical section
            if fid in _SEEN_FIDS:
                return
            _SEEN_FIDS.add(fid)
            path = FINDINGS_DIR / (fid + ".md")
            if path.exists():
                return
            path.write_text(
                "---\nid: %s\ntitle: %s\ntype: fact\nauthority: claimed\naliases: []\n"
                "tags: [session]\nupdated: \nsupersedes: []\n---\n\n%s\n"
                % (fid, fact[:70], fact), encoding="utf-8")
            if _FACTS is not None:
                _FACTS.append({"id": fid, "title": fact[:70], "type": "fact",
                               "authority": "claimed", "aliases": [], "tags": ["session"],
                               "supersedes": [], "links": [], "body": fact})
            log("saved finding -> %s" % fid)
    except Exception as e:
        log("save_finding failed: %s" % e)


def _assistant_text(raw):
    """Text the model produced, from streamed SSE or a non-stream JSON body."""
    try:
        txt = raw.decode("utf-8", "replace")
    except Exception:
        return ""
    if txt.lstrip().startswith("{"):
        try:
            d = json.loads(txt)
            return "".join(b.get("text", "") for b in d.get("content", [])
                           if isinstance(b, dict) and b.get("type") == "text")
        except Exception:
            pass
    out = []
    for line in txt.splitlines():
        line = line.strip()
        if line.startswith("data:"):
            try:
                d = json.loads(line[5:].strip())
                if d.get("type") == "content_block_delta":
                    out.append(d.get("delta", {}).get("text", "") or "")
            except Exception:
                pass
    return "".join(out)


def harvest_findings(raw):
    try:
        for m in _REMEMBER.finditer(_assistant_text(raw)):
            save_finding(m.group(1))
    except Exception as e:
        log("harvest failed: %s" % e)


def log_upstream_usage(raw, raw_est=0):
    """Log the model's REPORTED token counts, so context tuning is grounded in real numbers
    (is the prompt near num_ctx => spilling?). input_tokens arrives on message_start; the
    FINAL output_tokens on the last message_delta - so scan ALL lines: take input from the
    start, keep overwriting output from each delta. Per-line parse is isolated. Never raises."""
    try:
        txt = raw.decode("utf-8", "replace")
        inp = out = None
        if txt.lstrip().startswith("{"):
            u = json.loads(txt).get("usage") or {}
            inp, out = u.get("input_tokens"), u.get("output_tokens")
        else:
            for line in txt.splitlines():
                line = line.strip()
                if not line.startswith("data:"):
                    continue
                try:
                    d = json.loads(line[5:].strip())
                except Exception:
                    continue                          # a malformed / "[DONE]" line skips, not aborts
                u = (d.get("message", {}) or {}).get("usage") or d.get("usage") or {}
                if u.get("input_tokens") is not None:
                    inp = u["input_tokens"]
                if u.get("output_tokens") is not None:
                    out = u["output_tokens"]           # last delta wins => true generated count
        if inp is not None:
            _observe_ratio(inp, raw_est)               # learn est->real from the model's own count
            log("<- usage: input=%s output=%s%s"
                % (inp, out, ("  est=%d calib=%.2f" % (raw_est, _CALIB)) if raw_est else ""))
    except Exception:
        pass


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
            payload.pop("thinking", None)             # NEVER forward Anthropic 'thinking': Ollama may map it to
                                                      # `think`, which CRASHES qwen3-coder llama-server (0xc0000409),
                                                      # killing every loaded model. The thinker emits <think> natively.
            strip_harness_identity(payload)           # drop "You are a Claude agent"; our preamble is the identity
            messages = payload.get("messages", [])
            cont, key = typed_block(messages)          # the user's TYPED block (marker lives here, not the join)
            typed = (cont.get(key) if cont is not None else "") or ""
            kind, cleaned = detect_marker(typed)
            label = kind or DEFAULT_ROUTE
            model = THINK_MODEL if label == "think" else LOCAL_MODEL  # LOCAL ONLY: cloud removed
            if kind and cont is not None:              # a marker was present -> strip it from THAT block only
                cont[key] = cleaned
            task = cleaned if cont is not None else last_user_text(messages)
            ctx, nfacts, skill = build_context(task) if task else ("", 0, "-")
            inject_system(payload, ctx)
            payload["model"] = model
            if label == "think":
                nctx, reserve = THINK_CTX, THINK_MIN_TOKENS + OUT_MARGIN
                # a thinking model spends a big budget inside <think>; reserve its full output
                # room so the answer is not cut, and floor max_tokens at THINK_MIN_TOKENS.
                payload["max_tokens"] = max(int(payload.get("max_tokens", 0) or 0), THINK_MIN_TOKENS)
            else:
                nctx, reserve = NUM_CTX, GEN_RESERVE
            fit_window(payload, nctx, reserve)       # bound INPUT to the CHOSEN model's window
            cap_output_tokens(payload, nctx)         # bound OUTPUT so input + max_tokens <= num_ctx
            new_body = json.dumps(payload).encode("utf-8")
            raw_est = _raw_est_tokens(payload)        # uncalibrated, for the est->real feedback loop
            log("-> %-5s model=%s  facts=%d  skill=%s  ~%dtok  mt=%s  ctx=%d  stream=%s"
                % (label, model, nfacts, skill, _est_tokens(payload),
                   payload.get("max_tokens"), nctx, payload.get("stream", False)))
        except Exception as e:
            log("passthrough (inject error: %s)" % e)
            new_body, raw_est = body, 0

        self._pipe(self.path, new_body, dict(self.headers), harvest=True, raw_est=raw_est)

    def _pipe(self, path, body, headers, method="POST", harvest=False, raw_est=0):
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
        buf = bytearray() if harvest else None
        try:
            while True:
                chunk = up.read(4096)
                if not chunk:
                    break
                self.wfile.write(chunk)
                self.wfile.flush()
                if buf is not None:
                    buf.extend(chunk)
        except Exception:
            pass
        finally:
            up.close()
        if buf is not None:                          # after the stream: save findings + log real prompt size
            harvest_findings(bytes(buf))
            log_upstream_usage(bytes(buf), raw_est)

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
