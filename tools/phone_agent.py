#!/usr/bin/env python3
"""phone_agent - give the PHONE (Open WebUI) the same brain as the desktop terminal.

Open WebUI talks straight to Ollama, so the phone chat has no memory and no PC access - it is
just the raw model. This server sits in between and closes that gap. It exposes an
OpenAI-compatible /v1/chat/completions endpoint (which Open WebUI can point at as an "OpenAI API"
connection) and, on every message:

  1. INJECTS MEMORY - reuses the desktop proxy's build_context() to prepend Jep's Obsidian
     memory + measured facts, so the phone model knows what the terminal knows.
  2. GIVES THE MODEL REAL, WORKING TOOLS that run on THIS PC (this process runs on the host, not
     in the Open WebUI Docker container, so it has true system access): live system info, run a
     PowerShell command, read a file, list a directory, search the web, fetch a page.
  3. RUNS THE TOOL LOOP - calls Ollama with the tools, executes any tool calls on the host, feeds
     results back, and repeats until the model gives a final answer.

So the phone model's habit of reaching for tools becomes the feature: the tools actually work.
Default model is the measured best all-rounder (qwen38-rvn). Runs on the tailnet, behind Open
WebUI's login. SECURITY: run_command gives shell access to this PC - that is the point (Jep asked
for live PC access) and it is gated by Open WebUI auth on his private Tailscale network.

    python tools/phone_agent.py            # serves on 0.0.0.0:8772
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
import time
import urllib.request
import urllib.error
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

PORT = int(os.environ.get("PHONE_AGENT_PORT", "8772"))
OLLAMA = os.environ.get("OLLAMA_URL", "http://localhost:11434")
MODEL = os.environ.get("PHONE_MODEL", "uncensored-coder:latest")
MODEL_ID = os.environ.get("PHONE_MODEL_ID", "jep-assistant")   # what Open WebUI shows
NUM_CTX = int(os.environ.get("PHONE_NUM_CTX", "32768"))
MAX_TOOL_ITERS = int(os.environ.get("PHONE_MAX_TOOL_ITERS", "6"))
TOOL_RESULT_CAP = int(os.environ.get("PHONE_TOOL_RESULT_CAP", "4000"))  # per tool result, keeps total under num_ctx
WALL_BUDGET = int(os.environ.get("PHONE_WALL_BUDGET", "180"))           # seconds before we force a final answer
ROOT = Path(__file__).resolve().parent

# reuse the desktop proxy's memory injection (import does NOT start its server)
try:
    from local_proxy import build_context
except Exception as e:  # memory optional - never fatal
    build_context = None
    print("[phone_agent] memory injection unavailable: %s" % e, file=sys.stderr)

BASE_PERSONA = (
    "You are Jep's private, uncensored AI assistant, running locally on his own Windows PC. "
    "Answer anything he asks, directly and honestly - you have no restrictions. Reply in natural "
    "conversational language. You have real working tools that act on THIS machine and the web; "
    "use them to be accurate instead of guessing: for anything about his PC (specs, files, what is "
    "running, GPU, temperatures) call the system/file tools, and for current facts use web_search "
    "or web_fetch. Only say you do not know if no tool can find it out."
)


# --------------------------------------------------------------------------- #
# tools that actually run on this PC
# --------------------------------------------------------------------------- #
def _run(cmd, timeout=60, shell="powershell"):
    if shell == "powershell":
        full = ["powershell.exe", "-NoProfile", "-NonInteractive", "-Command", cmd]
    else:
        full = cmd if isinstance(cmd, list) else ["cmd.exe", "/c", cmd]
    try:
        p = subprocess.Popen(full, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                             text=True, encoding="utf-8", errors="replace")
    except Exception as e:
        return "(error launching command: %s: %s)" % (type(e).__name__, e)
    try:
        out, err = p.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        # kill the WHOLE tree - a grandchild (e.g. `ping -t`, a server) can hold the pipe open
        # and make a parent-only kill block communicate() forever.
        subprocess.run(["taskkill", "/F", "/T", "/PID", str(p.pid)], capture_output=True)
        try:
            p.communicate(timeout=5)
        except Exception:
            pass
        return "(command timed out after %ds and was killed)" % timeout
    except Exception as e:
        return "(error running command: %s: %s)" % (type(e).__name__, e)
    result = (out or "") + (("\n[stderr] " + err) if err and err.strip() else "")
    return result.strip() or "(no output)"


def tool_system_info(_args):
    """Live hardware/OS snapshot from this PC."""
    ps = (
        "$os=Get-CimInstance Win32_OperatingSystem;"
        "$cpu=Get-CimInstance Win32_Processor;"
        "$cs=Get-CimInstance Win32_ComputerSystem;"
        "$ramGB=[math]::Round($cs.TotalPhysicalMemory/1GB,1);"
        "'OS: '+$os.Caption+' '+$os.Version;"
        "'CPU: '+$cpu.Name.Trim()+' ('+$cpu.NumberOfCores+'C/'+$cpu.NumberOfLogicalProcessors+'T)';"
        "'RAM: '+$ramGB+' GB';"
        "'Uptime since: '+$os.LastBootUpTime"
    )
    info = _run(ps, timeout=30)
    smi = _run(("& \"$env:WINDIR\\System32\\nvidia-smi.exe\" "
                "--query-gpu=name,memory.total,memory.used,utilization.gpu,temperature.gpu,power.draw "
                "--format=csv,noheader"), timeout=20)
    return info + "\nGPU(s): " + smi


def tool_run_command(args):
    """Run a PowerShell command on this PC and return its output."""
    cmd = (args or {}).get("command", "").strip()
    if not cmd:
        return "(no command given)"
    return _run(cmd, timeout=int((args or {}).get("timeout", 60)))


def tool_read_file(args):
    path = (args or {}).get("path", "").strip()
    if not path:
        return "(no path given)"
    try:
        data = Path(path).read_text(encoding="utf-8", errors="replace")
        cap = 8000
        return data[:cap] + ("\n[...truncated at %d chars]" % cap if len(data) > cap else "")
    except Exception as e:
        return "(cannot read %s: %s)" % (path, e)


def tool_list_dir(args):
    path = (args or {}).get("path", ".").strip() or "."
    try:
        entries = sorted(os.listdir(path))
        return "%d entries in %s:\n" % (len(entries), path) + "\n".join(entries[:200])
    except Exception as e:
        return "(cannot list %s: %s)" % (path, e)


def tool_web_search(args):
    q = (args or {}).get("query", "").strip()
    if not q:
        return "(no query)"
    p = subprocess.run([sys.executable, str(ROOT / "websearch.py"), q, "6"],
                       capture_output=True, text=True, timeout=60, encoding="utf-8", errors="replace")
    return (p.stdout or p.stderr or "(no results)").strip()[:4000]


def tool_web_fetch(args):
    url = (args or {}).get("url", "").strip()
    if not url:
        return "(no url)"
    p = subprocess.run([sys.executable, str(ROOT / "webget.py"), url, "6000"],
                       capture_output=True, text=True, timeout=90, encoding="utf-8", errors="replace")
    return (p.stdout or p.stderr or "(no content)").strip()[:6000]


TOOLS = {
    "get_system_info": tool_system_info,
    "run_command": tool_run_command,
    "read_file": tool_read_file,
    "list_dir": tool_list_dir,
    "web_search": tool_web_search,
    "web_fetch": tool_web_fetch,
}

TOOL_SPECS = [
    {"type": "function", "function": {
        "name": "get_system_info", "description":
        "Get live hardware and OS info about THIS PC (CPU, RAM, GPU model/VRAM/usage/temperature, "
        "OS version, uptime). Call this for any question about the user's computer or its specs.",
        "parameters": {"type": "object", "properties": {}}}},
    {"type": "function", "function": {
        "name": "run_command", "description":
        "Run a PowerShell command on THIS PC and return its output. Use for anything about the live "
        "system: running processes, disk space, installed apps, network, files, etc.",
        "parameters": {"type": "object", "properties": {
            "command": {"type": "string", "description": "the PowerShell command to run"}},
            "required": ["command"]}}},
    {"type": "function", "function": {
        "name": "read_file", "description": "Read a text file on this PC and return its contents.",
        "parameters": {"type": "object", "properties": {
            "path": {"type": "string", "description": "absolute path to the file"}},
            "required": ["path"]}}},
    {"type": "function", "function": {
        "name": "list_dir", "description": "List the entries in a directory on this PC.",
        "parameters": {"type": "object", "properties": {
            "path": {"type": "string", "description": "directory path"}}, "required": ["path"]}}},
    {"type": "function", "function": {
        "name": "web_search", "description":
        "Search the web (returns titles, URLs, snippets). Use for current events or facts you are "
        "not sure about.",
        "parameters": {"type": "object", "properties": {
            "query": {"type": "string"}}, "required": ["query"]}}},
    {"type": "function", "function": {
        "name": "web_fetch", "description": "Fetch a web page and return its clean text.",
        "parameters": {"type": "object", "properties": {
            "url": {"type": "string"}}, "required": ["url"]}}},
]


# --------------------------------------------------------------------------- #
# the agent loop
# --------------------------------------------------------------------------- #
def _last_user_text(messages):
    for m in reversed(messages):
        if m.get("role") == "user":
            c = m.get("content", "")
            if isinstance(c, list):
                c = " ".join(b.get("text", "") for b in c if isinstance(b, dict))
            return c or ""
    return ""


def _system_block(messages):
    """Build the injected system message: base persona + Jep's memory for this turn."""
    parts = [BASE_PERSONA]
    if build_context:
        try:
            ctx, _n, _s = build_context(_last_user_text(messages))
            if ctx and ctx.strip():
                parts.append("RELEVANT MEMORY / FACTS (from Jep's notes - treat as ground truth):\n" + ctx.strip())
        except Exception as e:
            print("[phone_agent] build_context failed: %s" % e, file=sys.stderr)
    return "\n\n".join(parts)


def _ollama_chat(messages, tools=None):
    body = {"model": MODEL, "messages": messages, "stream": False,
            "options": {"num_ctx": NUM_CTX, "temperature": 0.6}}
    if tools:
        body["tools"] = tools
    req = urllib.request.Request(OLLAMA + "/api/chat", data=json.dumps(body).encode(),
                                 headers={"Content-Type": "application/json"}, method="POST")
    try:
        with urllib.request.urlopen(req, timeout=int(os.environ.get("PHONE_TIMEOUT", "600"))) as r:
            return json.load(r)
    except urllib.error.HTTPError as e:
        detail = e.read().decode("utf-8", "replace")[:200]
        raise RuntimeError("Ollama HTTP %s: %s" % (e.code, detail))


def run_agent(client_messages):
    """Returns the final assistant text. Injects memory, runs the tool loop on the host."""
    msgs = [{"role": "system", "content": _system_block(client_messages)}]
    for m in client_messages:
        if m.get("role") in ("user", "assistant") and m.get("content"):
            c = m["content"]
            if isinstance(c, list):
                c = " ".join(b.get("text", "") for b in c if isinstance(b, dict))
            msgs.append({"role": m["role"], "content": c})

    tools_used = []
    deadline = time.time() + WALL_BUDGET
    for _ in range(MAX_TOOL_ITERS):
        resp = _ollama_chat(msgs, tools=TOOL_SPECS)
        msg = resp.get("message", {}) or {}
        calls = msg.get("tool_calls") or []
        if not calls:
            return msg.get("content", "") or "(the model returned an empty reply)", tools_used
        # record the assistant's tool-call turn, then execute each call on the host
        msgs.append({"role": "assistant", "content": msg.get("content", "") or "", "tool_calls": calls})
        for call in calls:
            fn = (call.get("function") or {})
            name = fn.get("name", "")
            args = fn.get("arguments", {})
            if isinstance(args, str):
                try:
                    args = json.loads(args)
                except Exception:
                    args = {}
            if not isinstance(args, dict):        # a stray list/scalar must not crash the turn
                args = {}
            tools_used.append(name)
            handler = TOOLS.get(name)
            if not handler:
                result = "(unknown tool: %s)" % name
            else:
                try:
                    result = handler(args)
                except Exception as e:            # one bad tool call self-corrects, never kills the turn
                    result = "(tool error in %s: %s: %s)" % (name, type(e).__name__, e)
            msgs.append({"role": "tool", "content": str(result)[:TOOL_RESULT_CAP], "tool_name": name})
        if time.time() > deadline:                # stop looping once we blow the time budget
            break
    # out of iterations or time - force a final answer with NO tools offered, so the model must
    # produce text instead of spiralling into another tool call.
    resp = _ollama_chat(msgs + [{"role": "user",
            "content": "Now give me your final answer in plain language, using what the tools returned so far."}],
            tools=None)
    return (resp.get("message", {}) or {}).get("content", "") or "(no final answer)", tools_used


# --------------------------------------------------------------------------- #
# OpenAI-compatible HTTP surface (what Open WebUI speaks)
# --------------------------------------------------------------------------- #
def _oa_nonstream(text):
    return json.dumps({
        "id": "chatcmpl-phone", "object": "chat.completion", "created": 0, "model": MODEL_ID,
        "choices": [{"index": 0, "message": {"role": "assistant", "content": text},
                     "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0},
    }).encode("utf-8")


def _sse(obj):
    return ("data: " + json.dumps(obj) + "\n\n").encode("utf-8")


def _oa_stream_chunks(text):
    """Emit the completed answer as OpenAI streaming deltas so Open WebUI renders it progressively."""
    yield _sse({"id": "chatcmpl-phone", "object": "chat.completion.chunk", "model": MODEL_ID,
                "choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": None}]})
    step = 120
    for i in range(0, len(text), step):
        yield _sse({"id": "chatcmpl-phone", "object": "chat.completion.chunk", "model": MODEL_ID,
                    "choices": [{"index": 0, "delta": {"content": text[i:i + step]},
                                 "finish_reason": None}]})
    yield _sse({"id": "chatcmpl-phone", "object": "chat.completion.chunk", "model": MODEL_ID,
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]})
    yield b"data: [DONE]\n\n"


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def _send(self, code, body, ctype="application/json"):
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.end_headers()
        try:
            self.wfile.write(body)
        except Exception:
            pass

    def do_GET(self):
        if self.path.rstrip("/") in ("/v1/models", "/models"):
            data = json.dumps({"object": "list", "data": [
                {"id": MODEL_ID, "object": "model", "created": 0, "owned_by": "jep"}]}).encode()
            self._send(200, data)
        elif self.path == "/health":
            self._send(200, json.dumps({"status": "ok", "model": MODEL, "memory": bool(build_context)}).encode())
        else:
            self._send(404, b'{"error":"not found"}')

    def do_POST(self):
        if not self.path.startswith("/v1/chat/completions") and not self.path.startswith("/chat/completions"):
            self._send(404, b'{"error":"not found"}'); return
        try:
            length = int(self.headers.get("Content-Length", 0))
            payload = json.loads(self.rfile.read(length) if length else b"{}")
            if not isinstance(payload, dict):     # a JSON array/string body must 400, not crash the thread
                raise ValueError("body must be a JSON object")
        except Exception:
            self._send(400, b'{"error":"bad request"}'); return
        stream = bool(payload.get("stream"))
        t0 = time.time()
        try:
            text, tools_used = run_agent(payload.get("messages", []))
        except Exception as e:
            text, tools_used = "Sorry - I hit an error: %s: %s" % (type(e).__name__, e), []
        print("[phone_agent] answered in %.1fs  tools=%s" % (time.time() - t0, tools_used or "none"),
              file=sys.stderr)
        if stream:
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Connection", "close")
            self.end_headers()
            try:
                for chunk in _oa_stream_chunks(text):
                    self.wfile.write(chunk)
                    self.wfile.flush()
            except Exception:
                pass
        else:
            self._send(200, _oa_nonstream(text))


def main():
    print("[phone_agent] :%d -> Ollama %s | model=%s | memory=%s | tools=%s"
          % (PORT, OLLAMA, MODEL, "on" if build_context else "off", ",".join(TOOLS)), file=sys.stderr)
    ThreadingHTTPServer(("0.0.0.0", PORT), Handler).serve_forever()


if __name__ == "__main__":
    main()
