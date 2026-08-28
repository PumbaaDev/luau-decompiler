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
SECURITY: run_command gives shell access to this PC - that is the point (Jep asked for live PC
access). Open WebUI's own login lives on :3000; it does NOT protect this backend on :8772. This
process must gate its OWN transport: set PHONE_AGENT_TOKEN and a matching Bearer token is required
on every host-tool request (Open WebUI sends it as the connection's API key). Without the token set,
the endpoint is UNAUTHENTICATED - anyone who can reach :8772 (a LAN/IoT device via the 0.0.0.0 bind,
or any tailnet node) gets code execution + arbitrary file read. Always set the token.

    python tools/phone_agent.py            # serves on 0.0.0.0:8772
"""
from __future__ import annotations

import hmac
import json
import os
import re
import subprocess
import sys
import threading
import time
import urllib.request
import urllib.error
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlparse, parse_qs

sys.path.insert(0, str(Path(__file__).resolve().parent))

PORT = int(os.environ.get("PHONE_AGENT_PORT", "8772"))
# Shared-secret gate for the host-tool endpoints. The server binds 0.0.0.0 so the Open WebUI Docker
# container can reach it via host.docker.internal; that same bind exposes it on the LAN + tailnet, and
# the tool loop runs PowerShell / reads files on the host - so require a Bearer token when one is set.
# Set it as the API key on Open WebUI's OpenAI connection; OWUI then sends it automatically.
PHONE_AGENT_TOKEN = os.environ.get("PHONE_AGENT_TOKEN", "").strip()
OLLAMA = os.environ.get("OLLAMA_URL", "http://localhost:11434")
MODEL = os.environ.get("PHONE_MODEL", "uncensored-coder:latest")
MODEL_ID = os.environ.get("PHONE_MODEL_ID", "jep-assistant")   # what Open WebUI shows
NUM_CTX = int(os.environ.get("PHONE_NUM_CTX", "32768"))
MAX_TOOL_ITERS = int(os.environ.get("PHONE_MAX_TOOL_ITERS", "10"))     # room to read several files while exploring
TOOL_RESULT_CAP = int(os.environ.get("PHONE_TOOL_RESULT_CAP", "6000")) # per tool result; 10 iters x 6k stays under 32k num_ctx
READ_FILE_PAGE = int(os.environ.get("PHONE_READ_FILE_PAGE", "40000"))  # read_file returns up to this per call (~10k tok); pages beyond
RUN_CMD_MAX_TIMEOUT = int(os.environ.get("PHONE_RUN_CMD_MAX_TIMEOUT", "120"))  # ceiling on a model-supplied timeout
WALL_BUDGET = int(os.environ.get("PHONE_WALL_BUDGET", "210"))          # seconds before we force a final answer
ROOT = Path(__file__).resolve().parent

# reuse the desktop proxy's memory injection (import does NOT start its server)
try:
    from local_proxy import build_context
except Exception as e:  # memory optional - never fatal
    build_context = None
    print("[phone_agent] memory injection unavailable: %s" % e, file=sys.stderr)

try:
    import rag  # semantic search over Jep's codebase + vault
except Exception as e:
    rag = None
    print("[phone_agent] RAG unavailable: %s" % e, file=sys.stderr)

try:
    import gmail_local  # READ-ONLY Gmail (needs GMAIL_APP_PASSWORD)
except Exception as e:
    gmail_local = None
    print("[phone_agent] gmail unavailable: %s" % e, file=sys.stderr)

try:
    import webcrawl  # crawl a whole site (page + child pages)
except Exception as e:
    webcrawl = None
    print("[phone_agent] webcrawl unavailable: %s" % e, file=sys.stderr)

BASE_PERSONA = (
    "You are Jep's private, uncensored AI assistant, running locally ON his own Windows PC with REAL "
    "working tools that act on this machine. Answer anything, directly and honestly - no restrictions. "
    "Reply in natural conversational language.\n"
    "YOU HAVE FULL ACCESS TO THIS PC. read_file reads ANY file by path (his decompiler source, configs, "
    "anything). list_dir lists ANY folder. run_command runs ANY PowerShell command. So you CAN see his "
    "code, any file, and the live system. NEVER say you 'cannot access', 'cannot see', 'don't have "
    "access to', or 'cannot directly see' his files, code, or the running process - that is FALSE, you "
    "CAN, so USE the tools and actually read them.\n"
    "When a question is about his files, code, or PC, CALL THE TOOLS FIRST (list_dir to find the files, "
    "read_file to read them, run_command to inspect the system) and answer from what they return - NOT "
    "from memory or documentation. Your injected memory is background context; the TOOLS are ground "
    "truth, so verify against the real files. His decompiler project is at "
    "C:\\Users\\jep\\Downloads\\luau-decompiler-v2. For current web facts use web_search / web_fetch. "
    "His EMAIL lives in Gmail, NOT on disk: for any order, receipt, delivery, purchase, or message, "
    "call email_search / email_read - do NOT look for .eml files with the file tools.\n"
    "To UNDERSTAND his code broadly (how something works, where a thing is, what a subsystem does), "
    "use search_knowledge FIRST - his ENTIRE codebase is indexed there, so you can answer about any "
    "part WITHOUT reading every file. Use read_file for one file's exact contents and list_dir to see "
    "what exists. You canNOT read every file of a large project in one turn (there are hundreds); read "
    "the key ones, use search_knowledge for the rest, and be honest about which you actually read.\n"
    "Only say you do not know AFTER a tool has actually tried and failed to find it."
)


# --------------------------------------------------------------------------- #
# tools that actually run on this PC
# --------------------------------------------------------------------------- #
def _run(cmd, timeout=60, shell="powershell"):
    if shell == "powershell":
        # force UTF-8 stdout so non-ASCII output isn't garbled by the default console codepage
        full = ["powershell.exe", "-NoProfile", "-NonInteractive", "-Command",
                "[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; " + cmd]
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
    try:
        t = int((args or {}).get("timeout", 60) or 60)
    except (TypeError, ValueError):
        t = 60
    # cap the model-supplied timeout: an uncapped value could pin a worker thread far past WALL_BUDGET
    return _run(cmd, timeout=max(1, min(t, RUN_CMD_MAX_TIMEOUT)))


def tool_read_file(args):
    path = (args or {}).get("path", "").strip()
    if not path:
        return "(no path given)"
    try:
        off = int((args or {}).get("offset", 0) or 0)
    except (TypeError, ValueError):
        off = 0
    try:
        data = Path(path).read_text(encoding="utf-8", errors="replace")
        total = len(data)
        # Return the FULL file when it fits in one page (READ_FILE_PAGE). For a file bigger than the
        # context window, page through it: this slice + an offset the model can pass to read the rest,
        # so it can read the WHOLE file across calls instead of only ever seeing the first few KB.
        chunk = data[off:off + READ_FILE_PAGE]
        end = off + len(chunk)
        if total <= READ_FILE_PAGE and off == 0:
            return chunk                                   # whole file, no marker
        head = "[file %s: chars %d-%d of %d]\n" % (path, off, end, total)
        tail = ("\n[...more: call read_file again with offset=%d to continue]" % end) if end < total else "\n[end of file]"
        return head + chunk + tail
    except Exception as e:
        return "(cannot read %s: %s)" % (path, e)


def tool_list_dir(args):
    path = (args or {}).get("path", ".").strip() or "."
    try:
        entries = sorted(os.listdir(path))
        shown = entries[:200]
        extra = ("\n[...%d more not shown]" % (len(entries) - 200)) if len(entries) > 200 else ""
        return "%d entries in %s:\n" % (len(entries), path) + "\n".join(shown) + extra
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


def tool_search_knowledge(args):
    """Semantic search over Jep's OWN codebase + Obsidian notes (the decompiler, his projects)."""
    q = (args or {}).get("query", "").strip()
    if not q:
        return "(no query)"
    if rag is None:
        return "(knowledge index unavailable)"
    k = int((args or {}).get("k", 5))
    try:
        return rag.format_hits(rag.retrieve(q, k))[:6000]
    except Exception as e:
        return "(knowledge search error: %s)" % e


def tool_crawl_site(args):
    """Crawl a whole site (a page AND its child pages) and return all the clean text - use when one
    page is not enough and you need everything a site/docs section has on a topic."""
    if webcrawl is None:
        return "(crawler unavailable)"
    url = (args or {}).get("url", "").strip()
    if not url:
        return "(no url)"
    try:
        pages = webcrawl.crawl(url, int((args or {}).get("max_pages", 15)),
                               int((args or {}).get("max_depth", 2)))
        return webcrawl.format_pages(pages)[:12000]
    except Exception as e:
        return "(crawl error: %s)" % e


def tool_learn_web(args):
    """Crawl a whole site and add it PERMANENTLY to the knowledge index, so it's remembered and
    searchable forever via search_knowledge. Use when Jep wants you to 'learn'/'remember' a site."""
    if rag is None:
        return "(knowledge index unavailable)"
    url = (args or {}).get("url", "").strip()
    if not url:
        return "(no url)"
    try:
        n = rag.ingest_url(url, int((args or {}).get("max_pages", 20)),
                           int((args or {}).get("max_depth", 2)))
        return "learned and permanently indexed %d new chunks from %s (now searchable via search_knowledge)" % (n, url)
    except Exception as e:
        return "(learn error: %s)" % e


def tool_email_search(args):
    """Search Jep's Gmail (read-only). Gmail syntax: from:, subject:, newer_than:7d, etc."""
    if gmail_local is None:
        return "(email unavailable)"
    q = (args or {}).get("query", "").strip()
    if not q:
        return "(no query)"
    try:
        rows = gmail_local.search(q, int((args or {}).get("n", 10)))
    except Exception as e:
        return "(email search error: %s)" % e
    if not rows:
        return "(no matching emails)"
    return "\n".join("[uid %s] %s | %s | %s"
                     % (r["uid"], r["date"][:22], r["sender"][:34], r["subject"][:70]) for r in rows)


def tool_email_read(args):
    """Read one email by its uid (read-only)."""
    if gmail_local is None:
        return "(email unavailable)"
    uid = str((args or {}).get("uid", "")).strip()
    if not uid:
        return "(no uid)"
    try:
        r = gmail_local.read(uid)
    except Exception as e:
        return "(email read error: %s)" % e
    if r.get("error"):
        return r["error"]
    return "From: %s\nDate: %s\nSubject: %s\n%s" % (r["sender"], r["date"], r["subject"], r["body"][:5000])


TOOLS = {
    "get_system_info": tool_system_info,
    "run_command": tool_run_command,
    "read_file": tool_read_file,
    "list_dir": tool_list_dir,
    "web_search": tool_web_search,
    "web_fetch": tool_web_fetch,
    "crawl_site": tool_crawl_site,
    "learn_web": tool_learn_web,
    "search_knowledge": tool_search_knowledge,
    "email_search": tool_email_search,
    "email_read": tool_email_read,
}

TOOL_SPECS = [
    {"type": "function", "function": {
        "name": "get_system_info", "description":
        "Get live hardware and OS info about THIS PC (CPU, RAM, GPU model/VRAM/usage/temperature, "
        "OS version, uptime). Call this for any question about the user's computer or its specs. "
        "This is the AUTHORITATIVE source for GPU details incl. exact VRAM (it uses nvidia-smi).",
        "parameters": {"type": "object", "properties": {}}}},
    {"type": "function", "function": {
        "name": "run_command", "description":
        "Run a PowerShell command on THIS PC and return its output. Use for anything about the live "
        "system: running processes, disk space, installed apps, network, files, etc. Do NOT use it "
        "to read GPU VRAM (Windows Win32_VideoController caps AdapterRAM at ~4GB and lies) - use "
        "get_system_info for GPU/VRAM instead.",
        "parameters": {"type": "object", "properties": {
            "command": {"type": "string", "description": "the PowerShell command to run"}},
            "required": ["command"]}}},
    {"type": "function", "function": {
        "name": "read_file", "description": "Read a text file on this PC and return its FULL contents. "
        "A file bigger than one page returns a slice plus a continuation offset; call again with that "
        "offset to read the REST, and keep going until you reach end of file, so you read it fully.",
        "parameters": {"type": "object", "properties": {
            "path": {"type": "string", "description": "path to the file"},
            "offset": {"type": "integer", "description": "start reading at this character offset (default 0); "
                       "use the offset from a previous read to continue through a large file"}},
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
        "name": "web_fetch", "description": "Fetch a SINGLE web page and return its clean text.",
        "parameters": {"type": "object", "properties": {
            "url": {"type": "string"}}, "required": ["url"]}}},
    {"type": "function", "function": {
        "name": "crawl_site", "description":
        "Crawl a WHOLE site - a page AND its child/linked pages on the same domain - and return all "
        "the text. Use when one page is not enough and you need EVERYTHING a site or docs section "
        "covers on a topic. Bounded (default ~15 pages, depth 2); raise max_pages for a deeper crawl.",
        "parameters": {"type": "object", "properties": {
            "url": {"type": "string", "description": "the starting page to crawl from"},
            "max_pages": {"type": "integer", "description": "how many pages to crawl (default 15)"}},
            "required": ["url"]}}},
    {"type": "function", "function": {
        "name": "learn_web", "description":
        "Crawl a whole site and add it PERMANENTLY to your knowledge - remembered and searchable "
        "forever after. Use when Jep says to 'learn', 'remember', or 'index' a site so you keep its "
        "info. Bounded (default ~20 pages, depth 2); raise max_pages to absorb more.",
        "parameters": {"type": "object", "properties": {
            "url": {"type": "string", "description": "site to learn"},
            "max_pages": {"type": "integer", "description": "pages to crawl+index (default 20)"}},
            "required": ["url"]}}},
    {"type": "function", "function": {
        "name": "search_knowledge", "description":
        "Semantically search Jep's OWN codebase and Obsidian notes (his Luau decompiler in Rust, its "
        "lifter/opmap/emitter, his projects and measured findings). Use this for any question about "
        "HIS code, his decompiler, his projects, or things he has worked on/noted.",
        "parameters": {"type": "object", "properties": {
            "query": {"type": "string", "description": "what to look up in his code/notes"}},
            "required": ["query"]}}},
    {"type": "function", "function": {
        "name": "email_search", "description":
        "Search Jep's Gmail inbox (READ-ONLY). Use for anything about his orders, receipts, "
        "purchases, deliveries, or messages. Gmail search syntax works: from:currys, subject:order, "
        "newer_than:30d, etc. Returns a list with a uid per email; use email_read to open one.",
        "parameters": {"type": "object", "properties": {
            "query": {"type": "string", "description": "Gmail search query"}}, "required": ["query"]}}},
    {"type": "function", "function": {
        "name": "email_read", "description": "Read the full text of one email by its uid (READ-ONLY).",
        "parameters": {"type": "object", "properties": {
            "uid": {"type": "string"}}, "required": ["uid"]}}},
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


_TOOLCALL_BLOCK = re.compile(r"<tool_call>\s*(.*?)\s*</tool_call>", re.S | re.I)
_FN_NAME = re.compile(r"<function\s*=\s*([A-Za-z0-9_]+)", re.I)
_FN_PARAM = re.compile(r"<parameter\s*=\s*([A-Za-z0-9_]+)\s*>(.*?)</parameter>", re.S | re.I)


def _parse_text_tool_calls(text):
    """Qwen3-Coder sometimes writes the tool call as TEXT (<tool_call><function=read_file>
    <parameter=path>...</parameter></function></tool_call>) instead of returning a structured
    tool_call, so Ollama hands it back as content and the loop never runs it. Parse those out of
    the text so they execute anyway. Also handles a bare JSON {"name","arguments"} form."""
    calls = []
    for block in _TOOLCALL_BLOCK.findall(text or ""):
        nm = _FN_NAME.search(block)
        if not nm:
            continue
        args = {k.strip(): v.strip() for k, v in _FN_PARAM.findall(block)}
        calls.append({"function": {"name": nm.group(1), "arguments": args}})
    if not calls:                                    # JSON form: {"name": "read_file", "arguments": {...}}
        for m in re.finditer(r'\{[^{}]*?"name"\s*:\s*"([A-Za-z0-9_]+)"[^{}]*?"arguments"\s*:\s*(\{.*?\})',
                             text or "", re.S):
            try:
                calls.append({"function": {"name": m.group(1), "arguments": json.loads(m.group(2))}})
            except Exception:
                pass
    return calls


def _strip_tool_calls(text):
    """Remove any leaked tool-call syntax from a final answer so the user never sees raw
    <tool_call>/<function=>/<parameter=> markup."""
    t = _TOOLCALL_BLOCK.sub("", text or "")
    t = re.sub(r"</?(tool_call|function|parameter)[^>]*>", "", t, flags=re.I)
    return t.strip()


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
        content = msg.get("content", "") or ""
        calls = msg.get("tool_calls") or []
        if not calls:                                 # model may have written the call as TEXT - recover it
            calls = _parse_text_tool_calls(content)
        if not calls:
            return _strip_tool_calls(content) or "(the model returned an empty reply)", tools_used
        # record the assistant's tool-call turn (leaked markup stripped), then execute each call
        msgs.append({"role": "assistant", "content": _strip_tool_calls(content), "tool_calls": calls})
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
            # read_file gets a bigger cap so a "read it fully" actually delivers the whole page, not 6KB
            rcap = READ_FILE_PAGE + 400 if name == "read_file" else TOOL_RESULT_CAP
            msgs.append({"role": "tool", "content": str(result)[:rcap], "tool_name": name})
        if time.time() > deadline:                # stop looping once we blow the time budget
            break
    # out of iterations or time - force a final answer with NO tools offered, so the model must
    # produce text instead of spiralling into another tool call.
    resp = _ollama_chat(msgs + [{"role": "user",
            "content": "Now give me your final answer in plain language, using what the tools returned so far. "
                       "Do NOT emit any tool-call markup; just the answer."}],
            tools=None)
    return _strip_tool_calls((resp.get("message", {}) or {}).get("content", "")) or "(no final answer)", tools_used


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


def _sse_role():
    return _sse({"id": "chatcmpl-phone", "object": "chat.completion.chunk", "model": MODEL_ID,
                 "choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": None}]})


def _sse_heartbeat():
    """A valid empty-CONTENT chunk. Open WebUI only resets its stream timeout on real data chunks,
    NOT on SSE comment lines - so during a cold 18GB model load or a long tool loop, comment-only
    keepalives let it time out ('Load failed'). This counts as data and renders nothing."""
    return _sse({"id": "chatcmpl-phone", "object": "chat.completion.chunk", "model": MODEL_ID,
                 "choices": [{"index": 0, "delta": {"content": ""}, "finish_reason": None}]})


def _oa_stream_content(text):
    """Content deltas + stop + DONE (NO role - the role chunk is sent up-front before the wait)."""
    step = 120
    for i in range(0, len(text), step):
        yield _sse({"id": "chatcmpl-phone", "object": "chat.completion.chunk", "model": MODEL_ID,
                    "choices": [{"index": 0, "delta": {"content": text[i:i + step]},
                                 "finish_reason": None}]})
    yield _sse({"id": "chatcmpl-phone", "object": "chat.completion.chunk", "model": MODEL_ID,
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]})
    yield b"data: [DONE]\n\n"


# ----- async job store: the phone SUBMITS a question then POLLS for the answer, so a 2-3 min query
# survives the phone sleeping / app-switching / network blips (a held streaming connection does not,
# because iOS suspends the page). The tool loop runs on the host regardless of the phone's state. -----
_JOBS = {}
_JOBS_LOCK = threading.Lock()
JOB_TTL = 1800   # forget a job 30 min after it finishes / is created


def _job_gc(now):
    dead = [j for j, v in _JOBS.items() if now - v.get("born", now) > JOB_TTL]
    for j in dead:
        _JOBS.pop(j, None)


def _start_job(messages):
    jid = uuid.uuid4().hex
    with _JOBS_LOCK:
        _job_gc(time.time())
        _JOBS[jid] = {"status": "pending", "text": "", "born": time.time(), "tools": []}

    def _worker():
        try:
            text, tools = run_agent(messages)
        except Exception as e:
            text, tools = "Sorry - I hit an error: %s: %s" % (type(e).__name__, e), []
        with _JOBS_LOCK:
            if jid in _JOBS:
                _JOBS[jid].update(status="done", text=text, tools=tools)
    threading.Thread(target=_worker, daemon=True).start()
    return jid


CHAT_PAGE = r"""<!doctype html><html lang=en><head>
<meta charset=utf-8><meta name=viewport content="width=device-width,initial-scale=1,maximum-scale=1,user-scalable=no">
<title>__NAME__</title>
<style>
:root{--bg:#0b0c10;--panel:#15171c;--me:#2a3a5c;--ai:#1b1e26;--tx:#e8eaed;--mut:#8b90a0;--acc:#6ea8fe}
*{box-sizing:border-box;-webkit-tap-highlight-color:transparent}
html,body{margin:0;height:100%;background:var(--bg);color:var(--tx);font:16px/1.5 -apple-system,system-ui,sans-serif}
#app{display:flex;flex-direction:column;height:100dvh}
header{padding:12px 16px;background:var(--panel);font-weight:600;display:flex;align-items:center;gap:8px;border-bottom:1px solid #23262e}
header .dot{width:9px;height:9px;border-radius:50%;background:#3ecf8e}
header small{margin-left:auto;color:var(--mut);font-weight:400;font-size:12px}
#log{flex:1;overflow-y:auto;padding:14px;display:flex;flex-direction:column;gap:10px}
.msg{max-width:86%;padding:10px 13px;border-radius:16px;white-space:pre-wrap;word-wrap:break-word;overflow-wrap:anywhere}
.me{align-self:flex-end;background:var(--me);border-bottom-right-radius:5px}
.ai{align-self:flex-start;background:var(--ai);border-bottom-left-radius:5px}
.ai pre,.ai code{background:#0d0f14;border-radius:8px;font-family:ui-monospace,Menlo,monospace;font-size:13px}
.ai pre{padding:10px;overflow-x:auto}.ai code{padding:1px 5px}
.think{align-self:flex-start;color:var(--mut);font-style:italic}
form{display:flex;gap:8px;padding:10px;background:var(--panel);border-top:1px solid #23262e}
textarea{flex:1;resize:none;max-height:120px;background:#0d0f14;color:var(--tx);border:1px solid #2a2e38;border-radius:12px;padding:10px 12px;font:inherit}
button{background:var(--acc);color:#04122e;border:0;border-radius:12px;padding:0 16px;font-weight:600}
button:disabled{opacity:.5}
</style></head><body><div id=app>
<header><span class=dot></span>__NAME__<small id=st>ready</small></header>
<div id=log></div>
<form id=f><textarea id=i rows=1 placeholder="Ask anything..." autofocus></textarea><button id=b>Send</button></form>
</div><script>
const KEY="jepai_token", MODEL="__MODEL__";
let tok=localStorage.getItem(KEY), msgs=[];
if(!tok){tok=prompt("Enter your access token (one time):")||"";localStorage.setItem(KEY,tok);}
const log=document.getElementById("log"),inp=document.getElementById("i"),btn=document.getElementById("b"),
      f=document.getElementById("f"),st=document.getElementById("st");
function add(cls,txt){const d=document.createElement("div");d.className="msg "+cls;d.textContent=txt;log.appendChild(d);log.scrollTop=log.scrollHeight;return d;}
inp.addEventListener("input",()=>{inp.style.height="auto";inp.style.height=Math.min(inp.scrollHeight,120)+"px";});
inp.addEventListener("keydown",e=>{if(e.key==="Enter"&&!e.shiftKey){e.preventDefault();f.requestSubmit();}});
const sleep=ms=>new Promise(r=>setTimeout(r,ms));
async function jget(url){return fetch(url,{headers:{"Authorization":"Bearer "+tok}});}
// SUBMIT then POLL: the query runs on the PC; each poll is a tiny request. This survives the phone
// sleeping, app-switching, and network blips - a held connection does not (iOS suspends the page).
f.addEventListener("submit",async e=>{e.preventDefault();const q=inp.value.trim();if(!q)return;
  add("me",q);msgs.push({role:"user",content:q});inp.value="";inp.style.height="auto";
  btn.disabled=true;const th=add("think","working...");
  try{
    const r=await fetch("/ask",{method:"POST",headers:{"Content-Type":"application/json","Authorization":"Bearer "+tok},body:JSON.stringify({messages:msgs})});
    if(r.status===401){th.remove();const d=add("ai","Unauthorized - wrong token. Tap to re-enter.");d.style.cursor="pointer";d.onclick=()=>{localStorage.removeItem(KEY);location.reload();};return;}
    const {job_id}=await r.json();
    let fails=0;
    while(true){
      await sleep(3000);
      let j;
      try{const p=await jget("/poll?id="+job_id);if(p.status===404){throw new Error("expired");}j=await p.json();fails=0;}
      catch(_){fails++;if(fails>40){th.remove();add("ai","Lost the server. Tap Send to retry.");break;}continue;}
      if(j.status==="done"){th.remove();add("ai",j.content||"(empty reply)");msgs.push({role:"assistant",content:j.content||""});break;}
      th.textContent="working... "+(j.elapsed||0)+"s";st.textContent="thinking "+(j.elapsed||0)+"s";
    }
  }catch(err){th.remove();add("ai","Error: "+err.message+" - tap Send to retry.");}
  finally{btn.disabled=false;st.textContent="ready";inp.focus();}
});
</script></body></html>"""


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def _authorized(self):
        """True if no token is configured (open) OR the request carries the right Bearer token.
        Constant-time compare so the check can't be timing-probed."""
        if not PHONE_AGENT_TOKEN:
            return True
        got = self.headers.get("Authorization", "")
        if got.startswith("Bearer "):
            got = got[7:]
        return hmac.compare_digest(got.strip(), PHONE_AGENT_TOKEN)

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
        if self.path in ("/", "/chat", "/index.html"):    # self-contained mobile chat page (no Docker/OWUI needed)
            page = CHAT_PAGE.replace("__NAME__", "Jep's AI").replace("__MODEL__", MODEL_ID)
            self._send(200, page.encode("utf-8"), ctype="text/html; charset=utf-8")
        elif self.path == "/health":                      # unauthenticated liveness ping (no host access)
            self._send(200, json.dumps({"status": "ok", "model": MODEL, "memory": bool(build_context)}).encode())
        elif not self._authorized():
            self._send(401, b'{"error":"unauthorized"}')
        elif self.path.startswith("/poll"):               # phone polls here for its submitted job's result
            jid = parse_qs(urlparse(self.path).query).get("id", [""])[0]
            with _JOBS_LOCK:
                job = _JOBS.get(jid)
                if job and job["status"] == "done":
                    _JOBS.pop(jid, None)                  # one-shot: hand the answer over then forget it
            if not job:
                self._send(404, b'{"error":"no such job"}')
            elif job["status"] == "done":
                self._send(200, json.dumps({"status": "done", "content": job["text"]}).encode())
            else:
                self._send(200, json.dumps({"status": "pending", "elapsed": int(time.time() - job["born"])}).encode())
        elif self.path.rstrip("/") in ("/v1/models", "/models"):
            data = json.dumps({"object": "list", "data": [
                {"id": MODEL_ID, "object": "model", "created": 0, "owned_by": "jep"}]}).encode()
            self._send(200, data)
        else:
            self._send(404, b'{"error":"not found"}')

    def do_POST(self):
        if self.path.startswith("/ask"):                  # phone submits a question -> get a job id, then poll
            if not self._authorized():
                self._send(401, b'{"error":"unauthorized"}'); return
            try:
                length = int(self.headers.get("Content-Length", 0))
                payload = json.loads(self.rfile.read(length) if length else b"{}")
                messages = payload.get("messages", []) if isinstance(payload, dict) else []
            except Exception:
                self._send(400, b'{"error":"bad request"}'); return
            jid = _start_job(messages)
            self._send(200, json.dumps({"job_id": jid}).encode())
            return
        if not self.path.startswith("/v1/chat/completions") and not self.path.startswith("/chat/completions"):
            self._send(404, b'{"error":"not found"}'); return
        if not self._authorized():                       # host tools live behind this - never run them keyless
            self._send(401, b'{"error":"unauthorized"}'); return
        try:
            length = int(self.headers.get("Content-Length", 0))
            payload = json.loads(self.rfile.read(length) if length else b"{}")
            if not isinstance(payload, dict):     # a JSON array/string body must 400, not crash the thread
                raise ValueError("body must be a JSON object")
        except Exception:
            self._send(400, b'{"error":"bad request"}'); return
        stream = bool(payload.get("stream"))
        t0 = time.time()
        messages = payload.get("messages", [])
        if stream:
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Connection", "close")
            self.close_connection = True
            self.end_headers()
            # The tool loop is synchronous and can take up to WALL_BUDGET; run it in a thread and
            # emit SSE heartbeat comments while it works so the client's idle timeout doesn't drop a
            # long answer before the first token arrives.
            result = {}

            def _work():
                try:
                    result["text"], result["tools"] = run_agent(messages)
                except Exception as e:
                    result["text"], result["tools"] = "Sorry - I hit an error: %s: %s" % (type(e).__name__, e), []

            th = threading.Thread(target=_work, daemon=True)
            th.start()
            try:
                self.wfile.write(_sse_role())                  # start the stream with real data at once
                self.wfile.flush()
                while th.is_alive():
                    th.join(timeout=10)
                    if th.is_alive():
                        self.wfile.write(_sse_heartbeat())     # empty-content chunk keeps OWUI's timeout alive
                        self.wfile.flush()
                for chunk in _oa_stream_content(result.get("text", "")):
                    self.wfile.write(chunk)
                    self.wfile.flush()
            except Exception:
                pass
            print("[phone_agent] answered in %.1fs  tools=%s"
                  % (time.time() - t0, result.get("tools") or "none"), file=sys.stderr)
        else:
            try:
                text, tools_used = run_agent(messages)
            except Exception as e:
                text, tools_used = "Sorry - I hit an error: %s: %s" % (type(e).__name__, e), []
            print("[phone_agent] answered in %.1fs  tools=%s" % (time.time() - t0, tools_used or "none"),
                  file=sys.stderr)
            self._send(200, _oa_nonstream(text))


def main():
    print("[phone_agent] :%d -> Ollama %s | model=%s | memory=%s | auth=%s | tools=%s"
          % (PORT, OLLAMA, MODEL, "on" if build_context else "off",
             "token" if PHONE_AGENT_TOKEN else "OPEN", ",".join(TOOLS)), file=sys.stderr)
    if not PHONE_AGENT_TOKEN:
        print("[phone_agent] WARNING: PHONE_AGENT_TOKEN is unset - host tools (PowerShell, file read) "
              "are exposed UNAUTHENTICATED on :%d. Set PHONE_AGENT_TOKEN to require a Bearer token." % PORT,
              file=sys.stderr)
    ThreadingHTTPServer(("0.0.0.0", PORT), Handler).serve_forever()


if __name__ == "__main__":
    main()
