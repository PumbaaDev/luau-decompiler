#!/usr/bin/env python3
"""Anthropic <-> Google Gemini bridge for the unified proxy's @gemini route.

Claude Code speaks the Anthropic /v1/messages protocol; the offline models behind the proxy
speak it too (via Ollama), so their responses stream straight back. Gemini speaks a different
protocol, so @gemini needs translation both ways. To keep the risk surface small and TESTABLE
WITHOUT A KEY, the strategy is:

  1. translate the Anthropic request -> one non-streaming Gemini generateContent call
  2. take the whole answer text back
  3. SYNTHESISE the Anthropic SSE event sequence (or a single Messages JSON) from that text

so the only part that needs a live key is the HTTP call in step 1; every translation function
here is pure and unit-tested against a fake Gemini payload. No token-by-token delta parsing, which
is where a streaming translator would grow subtle, runtime-only bugs.

Privacy: this bridge is fed ONLY the conversation. The proxy deliberately does NOT inject Jep's
private memory/vault on the @gemini route, because a Gemini turn leaves the machine to Google.
"""
import json
import urllib.request
import urllib.error

GEMINI_BASE = "https://generativelanguage.googleapis.com/v1beta/models"


# ---- Anthropic request -> Gemini request ---------------------------------------------------

def _system_text(payload):
    """Anthropic `system` is a string OR a list of {type:text,text:..} blocks. Flatten to text."""
    sys = payload.get("system")
    if isinstance(sys, str):
        return sys
    if isinstance(sys, list):
        out = []
        for b in sys:
            if isinstance(b, dict) and b.get("type") == "text":
                out.append(b.get("text", ""))
            elif isinstance(b, str):
                out.append(b)
        return "\n".join(t for t in out if t)
    return ""


def _blocks_to_text(content):
    """A message's content is a string OR a list of blocks. Pull the human-readable text out.
    tool_use/tool_result are stringified so context is preserved without modelling tool-calling."""
    if isinstance(content, str):
        return content
    if not isinstance(content, list):
        return ""
    parts = []
    for b in content:
        if not isinstance(b, dict):
            parts.append(str(b))
            continue
        t = b.get("type")
        if t == "text":
            parts.append(b.get("text", ""))
        elif t == "tool_result":
            c = b.get("content", "")
            parts.append(_blocks_to_text(c) if not isinstance(c, str) else c)
        elif t == "tool_use":
            parts.append("[called tool %s(%s)]" % (b.get("name", "?"),
                                                    json.dumps(b.get("input", {}))[:200]))
    return "\n".join(p for p in parts if p)


def to_gemini_request(payload, grounding=False):
    """Build the JSON body for models/<model>:generateContent from an Anthropic payload."""
    contents = []
    for m in payload.get("messages", []):
        role = "model" if m.get("role") == "assistant" else "user"
        text = _blocks_to_text(m.get("content", ""))
        if text:
            contents.append({"role": role, "parts": [{"text": text}]})
    # Claude Code emits a tool_result turn and the next typed turn both as role "user"; some Gemini
    # versions require alternating roles. Coalesce consecutive same-role turns so we never 400 on it.
    merged = []
    for c in contents:
        if merged and merged[-1]["role"] == c["role"]:
            merged[-1]["parts"].extend(c["parts"])
        else:
            merged.append({"role": c["role"], "parts": list(c["parts"])})
    contents = merged
    if not contents:                                   # Gemini rejects an empty contents array
        contents = [{"role": "user", "parts": [{"text": "(no content)"}]}]
    body = {"contents": contents}
    sys = _system_text(payload)
    if sys:
        body["systemInstruction"] = {"parts": [{"text": sys}]}
    gen = {}
    if payload.get("temperature") is not None:
        gen["temperature"] = payload["temperature"]
    mt = payload.get("max_tokens")
    if isinstance(mt, int) and mt > 0:
        gen["maxOutputTokens"] = mt
    if gen:
        body["generationConfig"] = gen
    if grounding:
        body["tools"] = [{"google_search": {}}]        # live web grounding
    return body


# ---- Gemini response -> text ----------------------------------------------------------------

def extract_text(resp):
    """Pull the answer text and output-token count from a generateContent response.
    Returns (text, out_tokens, finish_reason). Raises ValueError if the model was blocked."""
    fb = resp.get("promptFeedback") or {}
    if fb.get("blockReason"):
        raise ValueError("blocked: %s" % fb.get("blockReason"))
    cands = resp.get("candidates") or []
    if not cands:
        raise ValueError("no candidates in Gemini response")
    cand = cands[0]
    finish = cand.get("finishReason", "STOP")
    parts = ((cand.get("content") or {}).get("parts")) or []
    text = "".join(p.get("text", "") for p in parts if isinstance(p, dict))
    usage = resp.get("usageMetadata") or {}
    out_tok = int(usage.get("candidatesTokenCount", 0) or 0)
    if not text and finish not in ("STOP", "MAX_TOKENS"):
        raise ValueError("empty answer (finishReason=%s)" % finish)
    return text, out_tok, finish


# ---- text -> Anthropic wire format ----------------------------------------------------------

_FINISH_MAP = {"STOP": "end_turn", "MAX_TOKENS": "max_tokens", "SAFETY": "end_turn",
               "RECITATION": "end_turn", "OTHER": "end_turn"}


def _sse(event, data):
    return ("event: %s\ndata: %s\n\n" % (event, json.dumps(data))).encode("utf-8")


def anthropic_sse(text, model, in_tokens=0, out_tokens=0, finish="STOP", msg_id="msg_gemini"):
    """Synthesise the Anthropic streaming event sequence for a complete answer.
    The text is emitted in a few deltas so the terminal renders progressively, not in one blob."""
    stop = _FINISH_MAP.get(finish, "end_turn")
    chunks = _chunk(text, 96) or [""]
    out = bytearray()
    out += _sse("message_start", {"type": "message_start", "message": {
        "id": msg_id, "type": "message", "role": "assistant", "model": model,
        "content": [], "stop_reason": None, "stop_sequence": None,
        "usage": {"input_tokens": in_tokens, "output_tokens": 0}}})
    out += _sse("content_block_start", {"type": "content_block_start", "index": 0,
                "content_block": {"type": "text", "text": ""}})
    for c in chunks:
        out += _sse("content_block_delta", {"type": "content_block_delta", "index": 0,
                    "delta": {"type": "text_delta", "text": c}})
    out += _sse("content_block_stop", {"type": "content_block_stop", "index": 0})
    out += _sse("message_delta", {"type": "message_delta",
                "delta": {"stop_reason": stop, "stop_sequence": None},
                "usage": {"output_tokens": out_tokens}})
    out += _sse("message_stop", {"type": "message_stop"})
    return bytes(out)


def anthropic_json(text, model, in_tokens=0, out_tokens=0, finish="STOP", msg_id="msg_gemini"):
    """Synthesise a single non-streaming Anthropic Messages response."""
    return json.dumps({
        "id": msg_id, "type": "message", "role": "assistant", "model": model,
        "content": [{"type": "text", "text": text}],
        "stop_reason": _FINISH_MAP.get(finish, "end_turn"), "stop_sequence": None,
        "usage": {"input_tokens": in_tokens, "output_tokens": out_tokens},
    }).encode("utf-8")


def _chunk(s, n):
    return [s[i:i + n] for i in range(0, len(s), n)]


def error_sse(message, model="gemini", stream=True):
    """A clean, user-visible message rendered as a normal assistant turn (not an HTTP 500),
    so a missing key or a Google error reads as text in the terminal instead of breaking it."""
    if stream:
        return anthropic_sse(message, model, finish="STOP", msg_id="msg_gemini_note")
    return anthropic_json(message, model, finish="STOP", msg_id="msg_gemini_note")


# ---- the live call (the only part needing a key) --------------------------------------------

def call_gemini(model, key, body, timeout=120):
    # Key travels in the x-goog-api-key HEADER, never the URL query string - a URL can leak into
    # logs, proxies, and any exception that echoes it; a header does not.
    url = "%s/%s:generateContent" % (GEMINI_BASE, model)
    req = urllib.request.Request(url, data=json.dumps(body).encode("utf-8"),
                                 headers={"Content-Type": "application/json",
                                          "x-goog-api-key": key}, method="POST")
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.load(r)
