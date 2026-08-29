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

import base64
import hmac
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
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
WALL_BUDGET = int(os.environ.get("PHONE_WALL_BUDGET", "210"))          # seconds before we force a final answer (SSE path)
# The submit/poll (phone) path was built to wait minutes, so it gets a much larger budget + more
# iterations than the SSE path - a genuine multi-step job (crawl docs, cross-check code, page a big
# file) must not be force-finalized half-done just because it inherited the SSE cap.
JOB_WALL_BUDGET = int(os.environ.get("PHONE_JOB_WALL_BUDGET", "900"))   # seconds for the async /ask path
JOB_MAX_ITERS = int(os.environ.get("PHONE_JOB_MAX_ITERS", "30"))       # tool-loop iterations for the async path
HISTORY_CHAR_CAP = int(os.environ.get("PHONE_HISTORY_CHAR_CAP", "90000"))  # cap replayed chat history so a long
#                                                        # phone thread can't overflow num_ctx and evict the persona
def _parse_keep_alive(v):
    """Ollama needs keep_alive as a NUMBER (-1 forever, 0 unload, N seconds) or a duration string
    WITH a unit ("30m"). The bare string "-1" is rejected ('missing unit in duration'), so coerce
    a plain integer to int and pass a unit-bearing string through unchanged."""
    v = (v or "-1").strip() or "-1"               # a blank/whitespace value must fall back, not become "" (Ollama 400)
    try:
        return int(v)
    except ValueError:
        return v
KEEP_ALIVE = _parse_keep_alive(os.environ.get("PHONE_KEEP_ALIVE", "-1"))  # pin model resident: no cold 60s first query
VISION_MODEL = os.environ.get("PHONE_VISION_MODEL", "").strip()  # opt-in: pull a small VLM (e.g. qwen2.5vl:7b) to enable see_screen
ROOT = Path(__file__).resolve().parent

# EPISODE MEMORY: when a tool-loop reaches a CLEAN final answer (not the wall-budget/cancel forced
# exit, and the last tool did not error), one compact json line - the task, the tool trace that
# worked, the outcome - is appended here. local_proxy.build_context reads it back so a future
# session can be shown "a past session solved a similar task like this". FIFO-capped; if the file
# is absent the feature is simply dormant.
EPISODES_PATH = ROOT / "local_mem" / "episodes.jsonl"
EPISODES_CAP = int(os.environ.get("PHONE_EPISODES_CAP", "500"))
EPISODE_TRACE_MAX = 20                        # keep the episode line compact even on a 30-iter job
_EPISODES_LOCK = threading.Lock()             # concurrent jobs must not interleave the rewrite

# reuse the desktop proxy's memory injection (import does NOT start its server)
try:
    from local_proxy import build_context
except Exception as e:  # memory optional - never fatal
    build_context = None
    print("[phone_agent] memory injection unavailable: %s" % e, file=sys.stderr)

# reuse the desktop proxy's REMEMBER harvester so findings made FROM THE PHONE are also persisted
# (the persona already asks the model to write "REMEMBER: <fact>"; without this the phone discarded them)
try:
    from local_proxy import save_finding as _save_finding, _REMEMBER, HARVEST_MAX_PER_TURN as _HARVEST_MAX
except Exception as e:
    _save_finding = None
    _REMEMBER = None
    _HARVEST_MAX = 5
    print("[phone_agent] REMEMBER harvest unavailable: %s" % e, file=sys.stderr)

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
    "THINK before you answer. For anything beyond a trivial reply, reason through it step by step, then "
    "give a THOROUGH, well-reasoned, complete answer that actually engages with the question - explain "
    "the why, weigh the trade-offs, cover the important details, and do NOT be terse or shallow. Depth "
    "and accuracy matter more than speed.\n"
    "MATCH THE APPROACH TO THE QUESTION. A reasoning / advice / opinion / general-knowledge / "
    "conceptual / how-should-I question you answer with your OWN THINKING - do NOT call any tool, do "
    "NOT go read a file; just think it through and give a considered answer. Only reach for the tools "
    "when the question is genuinely about his ACTUAL files/code, his live PC, his email, or current "
    "web facts. Reading a random file when someone asked you to reason is a mistake - think instead.\n"
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


def tool_find_in_files(args):
    """Exact text / regex search across files (ripgrep). Finds WHERE a string, function name, or
    symbol appears - complements search_knowledge (which is semantic, not literal)."""
    pat = (args or {}).get("pattern", "").strip()
    if not pat:
        return "(no pattern given)"
    root = (args or {}).get("path", ".").strip() or "."
    glob = (args or {}).get("glob", "").strip()
    rg = shutil.which("rg")
    if rg:
        cmd = [rg, "--line-number", "--no-heading", "--color", "never", "-m", "300"]
        if glob:
            cmd += ["--glob", glob]
        cmd += ["--", pat, root]
        try:
            p = subprocess.run(cmd, capture_output=True, text=True, timeout=60,
                               encoding="utf-8", errors="replace")
            if p.returncode not in (0, 1):        # rg: 0=matches, 1=no matches, 2+=error (bad regex, unreadable path)
                return "(search error: %s)" % ((p.stderr or "").strip()[:300] or "rg exit %d" % p.returncode)
            out = (p.stdout or "").strip()
            return out[:TOOL_RESULT_CAP] if out else "(no matches)"
        except Exception as e:
            return "(search error: %s)" % e
    # fallback: python regex walk (no ripgrep on PATH) - time-bounded, skips huge/binary files
    try:
        rx = re.compile(pat)
    except re.error as e:
        return "(bad regex: %s)" % e
    hits, seen, stop = [], 0, time.time() + 60
    for dp, _dn, fn in os.walk(root):
        if any(s in dp for s in (".git", "target", "node_modules", "__pycache__")):
            continue
        if time.time() > stop or seen >= 300:
            break
        for name in fn:
            fp = os.path.join(dp, name)
            try:
                if os.path.getsize(fp) > 2_000_000:   # skip huge/binary files a literal search won't want
                    continue
                for i, line in enumerate(Path(fp).read_text(encoding="utf-8", errors="replace").splitlines(), 1):
                    if rx.search(line):
                        hits.append("%s:%d:%s" % (fp, i, line.strip()[:200]))
                        seen += 1
                        if seen >= 300:
                            break
            except Exception:
                continue
        if seen >= 300 or time.time() > stop:
            break
    return ("\n".join(hits))[:TOOL_RESULT_CAP] if hits else "(no matches)"


def tool_remember_fact(args):
    """Save a durable fact to Jep's memory so it is recalled in future chats (phone AND terminal)."""
    if _save_finding is None:
        return "(memory store unavailable)"
    fact = " ".join((args or {}).get("fact", "").split())
    if len(fact) < 20:
        return "(fact too short to save - give a fuller statement, ~20+ characters)"
    if len(fact) > 400:                          # save_finding silently drops >400; tell the model instead of lying
        return "(fact too long - condense it to 400 characters or fewer, then save again)"
    try:
        _save_finding(fact)
        return "Saved to memory: %s" % fact[:120]
    except Exception as e:
        return "(could not save: %s)" % e


def tool_see_screen(args):
    """Screenshot THIS PC and describe it via a local vision model (opt-in: set PHONE_VISION_MODEL to
    a pulled VLM such as qwen2.5vl:7b). Lets Jep ask 'what is on screen', 'did the render finish',
    'is there an error dialog' while away - fully offline."""
    if not VISION_MODEL:
        return ("(see_screen is off. To enable: pull a small vision model and set PHONE_VISION_MODEL, e.g. "
                "`ollama pull qwen2.5vl:7b` then setx PHONE_VISION_MODEL qwen2.5vl:7b and restart the server.)")
    png = str(Path(tempfile.gettempdir()) / "phone_screen.png")
    try:
        Path(png).unlink(missing_ok=True)         # remove any stale capture so a failed grab can't be described
    except Exception:
        pass
    cap = (
        "Add-Type -AssemblyName System.Windows.Forms,System.Drawing;"
        "$b=[System.Windows.Forms.SystemInformation]::VirtualScreen;"
        "$bmp=New-Object System.Drawing.Bitmap $b.Width,$b.Height;"
        "$g=[System.Drawing.Graphics]::FromImage($bmp);"
        "$g.CopyFromScreen($b.Location,[System.Drawing.Point]::Empty,$b.Size);"
        "$bmp.Save('%s',[System.Drawing.Imaging.ImageFormat]::Png);$g.Dispose();$bmp.Dispose()"
        % png.replace("\\", "\\\\"))
    grab = _run(cap, timeout=30)
    try:
        b64 = base64.b64encode(Path(png).read_bytes()).decode()
    except Exception as e:
        return "(could not capture the screen: %s; %s)" % (e, grab)
    q = (args or {}).get("question", "").strip() or (
        "Describe this screen: visible apps and windows, any dialogs or errors, progress bars and "
        "their percentage, and the key on-screen text.")
    # SHORT keep_alive for the VLM: it must NOT pin ~6GB in VRAM alongside the -1-pinned coder and
    # starve every later chat. It unloads a few minutes after use; the coder keeps its pin.
    body = {"model": VISION_MODEL, "keep_alive": os.environ.get("PHONE_VISION_KEEP_ALIVE", "2m"),
            "stream": False, "messages": [{"role": "user", "content": q, "images": [b64]}]}
    try:
        req = urllib.request.Request(OLLAMA + "/api/chat", data=json.dumps(body).encode(),
                                     headers={"Content-Type": "application/json"}, method="POST")
        with urllib.request.urlopen(req, timeout=240) as resp:
            return (json.load(resp).get("message", {}) or {}).get("content", "") or "(vision model returned nothing)"
    except Exception as e:
        return "(vision model error with %s: %s)" % (VISION_MODEL, e)


TOOLS = {
    "get_system_info": tool_system_info,
    "run_command": tool_run_command,
    "read_file": tool_read_file,
    "list_dir": tool_list_dir,
    "find_in_files": tool_find_in_files,
    "web_search": tool_web_search,
    "web_fetch": tool_web_fetch,
    "crawl_site": tool_crawl_site,
    "learn_web": tool_learn_web,
    "search_knowledge": tool_search_knowledge,
    "remember_fact": tool_remember_fact,
    "email_search": tool_email_search,
    "email_read": tool_email_read,
    "see_screen": tool_see_screen,
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
    {"type": "function", "function": {
        "name": "find_in_files", "description":
        "Search files for EXACT text or a regex (ripgrep) and return file:line matches. Use to find "
        "WHERE a string, function name, symbol, or error message appears in his code - the literal "
        "counterpart to search_knowledge (which is semantic). Default root is the current directory.",
        "parameters": {"type": "object", "properties": {
            "pattern": {"type": "string", "description": "text or regular expression to find"},
            "path": {"type": "string", "description": "directory to search (default '.')"},
            "glob": {"type": "string", "description": "optional file filter, e.g. '*.rs' or '*.py'"}},
            "required": ["pattern"]}}},
    {"type": "function", "function": {
        "name": "remember_fact", "description":
        "Save a durable fact to Jep's memory so you recall it in FUTURE chats (phone and terminal). "
        "Use when he tells you to remember something, or when you learn a lasting fact about him, his "
        "setup, or his projects. State the fact fully and self-contained (~20+ characters).",
        "parameters": {"type": "object", "properties": {
            "fact": {"type": "string", "description": "the fact to remember, stated fully"}},
            "required": ["fact"]}}},
    {"type": "function", "function": {
        "name": "see_screen", "description":
        "Look at the PC screen RIGHT NOW: take a screenshot and describe what is on it. Use when Jep "
        "asks what is on screen, whether something visibly finished or failed (a render, an upload, a "
        "build), or whether an error/dialog is showing. Optionally pass a specific question about the "
        "screen. (Requires a local vision model; returns setup instructions if none is configured.)",
        "parameters": {"type": "object", "properties": {
            "question": {"type": "string", "description": "optional: what to look for on the screen"}}}}},
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


def _ollama_chat(messages, tools=None, on_delta=None):
    """Call the model. Normally non-streaming (returns the full JSON). When on_delta is given AND no
    tools are offered, stream the generation and call on_delta(text_so_far) as tokens arrive, so the
    phone can show the answer forming instead of a spinner - then return the same {'message': {...}}
    shape the non-stream path returns, so callers are unchanged."""
    streaming = on_delta is not None and not tools
    body = {"model": MODEL, "messages": messages, "stream": streaming, "keep_alive": KEEP_ALIVE,
            "options": {"num_ctx": NUM_CTX, "temperature": 0.6}}
    if tools:
        body["tools"] = tools
    req = urllib.request.Request(OLLAMA + "/api/chat", data=json.dumps(body).encode(),
                                 headers={"Content-Type": "application/json"}, method="POST")
    timeout = int(os.environ.get("PHONE_TIMEOUT", "600"))
    try:
        if not streaming:
            with urllib.request.urlopen(req, timeout=timeout) as r:
                return json.load(r)
        # streaming: Ollama emits one JSON object per line; accumulate content and report progress
        acc = []
        final = {}
        with urllib.request.urlopen(req, timeout=timeout) as r:
            for line in r:
                line = line.strip()
                if not line:
                    continue
                try:
                    obj = json.loads(line)
                except Exception:
                    continue
                if obj.get("error"):               # Ollama streams an error object mid-stream; don't swallow it
                    raise RuntimeError("Ollama stream error: %s" % obj["error"])
                piece = (obj.get("message", {}) or {}).get("content", "") or ""
                if piece:
                    acc.append(piece)
                    try:
                        on_delta("".join(acc))
                    except Exception:
                        pass
                if obj.get("done"):
                    final = obj
        msg = final.get("message", {}) or {}
        msg["content"] = "".join(acc)
        final["message"] = msg
        return final
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


def _cap_history(msgs, cap=HISTORY_CHAR_CAP):
    """Keep the system head, the ORIGINAL question, and the most RECENT turns within `cap` chars,
    dropping the oldest MIDDLE turns. A long phone thread (or a big paginated read within one job)
    must never push the persona + injected memory + question out of num_ctx - Ollama does not error
    on overflow, it silently evicts the head. Never leave a role:'tool' result without its preceding
    assistant(tool_calls) parent - that 400s /api/chat."""
    if len(msgs) <= 2:
        return msgs
    head = msgs[0]
    fu = next((i for i, m in enumerate(msgs) if i > 0 and m.get("role") == "user"), None)
    pinned = [head] + ([msgs[fu]] if fu is not None else [])
    tail = list(msgs[fu + 1:] if fu is not None else msgs[1:])
    total = sum(len(str(m.get("content", ""))) for m in pinned) + sum(len(str(m.get("content", ""))) for m in tail)
    while len(tail) > 1 and total > cap:
        total -= len(str(tail.pop(0).get("content", "")))
    while tail and tail[0].get("role") == "tool":     # never start with an orphaned tool result
        tail.pop(0)
    return pinned + tail


def _harvest_remember(text):
    """Persist any 'REMEMBER: <fact>' lines the model emitted, so findings made FROM THE PHONE
    accumulate into the fact store exactly like the desktop terminal does (deduped + capped by
    save_finding). Without this, every fact learned on the phone was silently discarded."""
    if not (_save_finding and _REMEMBER and text):
        return
    saved = 0
    for m in _REMEMBER.finditer(text):
        if saved >= _HARVEST_MAX:
            break
        try:
            _save_finding(m.group(1))
        except Exception:
            pass
        saved += 1


# --------------------------------------------------------------------------- #
# episode memory: record tool-loops that WORKED so future sessions can reuse the approach
# --------------------------------------------------------------------------- #
_TRACE_KEY_ARGS = ("path", "query", "pattern", "command", "url", "uid", "fact", "question")


def _trace_arg(args):
    """The ONE most informative argument of a tool call, for the compact episode trace."""
    if not isinstance(args, dict):
        return ""
    for k in _TRACE_KEY_ARGS:
        v = args.get(k)
        if v:
            return " ".join(str(v).split())[:80]
    for v in args.values():
        if v:
            return " ".join(str(v).split())[:80]
    return ""


def _trace_status(result):
    """'ok'|'err' for the trace. Every tool reports failure as a leading '(...)' marker
    ('(tool error...', '(cannot read...', '(command timed out...'), so classify on that -
    a plain '(no matches)' / '(no output)' is a legitimate empty result, not an error."""
    r = str(result)
    if r.startswith("(") and ("error" in r[:100].lower() or "timed out" in r[:100].lower()
                              or r.startswith(("(cannot ", "(unknown tool", "(bad "))):
        return "err"
    return "ok"


def _save_episode(task, trace, answer):
    """Append ONE episode line {ts, task, tools, outcome} to episodes.jsonl, FIFO-capped at
    EPISODES_CAP (oldest lines drop first). Called ONLY on a clean completion. Best-effort:
    a failure here logs and never breaks the answer being returned."""
    if not (task and trace and answer):
        return
    rec = {"ts": time.time(),
           "task": " ".join(task.split())[:200],
           "tools": [list(t) for t in trace[:EPISODE_TRACE_MAX]],
           "outcome": " ".join(answer.split())[:300]}
    try:
        line = json.dumps(rec, ensure_ascii=False)
        with _EPISODES_LOCK:
            EPISODES_PATH.parent.mkdir(parents=True, exist_ok=True)
            lines = []
            if EPISODES_PATH.exists():
                lines = [ln for ln in EPISODES_PATH.read_text(encoding="utf-8",
                                                              errors="replace").splitlines() if ln.strip()]
            lines.append(line)
            if len(lines) > EPISODES_CAP:
                lines = lines[-EPISODES_CAP:]          # FIFO: the newest N survive
            tmp = EPISODES_PATH.with_suffix(".jsonl.tmp")
            tmp.write_text("\n".join(lines) + "\n", encoding="utf-8")
            os.replace(tmp, EPISODES_PATH)             # atomic: a reader never sees a half-written file
    except Exception as e:
        print("[phone_agent] episode save failed: %s" % e, file=sys.stderr)


# --------------------------------------------------------------------------- #
# live telemetry: token counts, a human-readable "what it's doing now", GPU stats
# --------------------------------------------------------------------------- #
_TOTAL_TOKENS = 0
_TOTAL_LOCK = threading.Lock()


def _bump_total(n):
    global _TOTAL_TOKENS
    if n:
        with _TOTAL_LOCK:
            _TOTAL_TOKENS += n


def _tokens_of(resp):
    """Tokens GENERATED by one model call (the model's real output work)."""
    try:
        return int(resp.get("eval_count") or 0)
    except Exception:
        return 0


_ACTIVITY = {
    "search_knowledge": "searching your notes", "read_file": "reading a file",
    "find_in_files": "searching your code", "list_dir": "listing files",
    "web_search": "searching the web", "web_fetch": "fetching a page",
    "crawl_site": "crawling a site", "learn_web": "learning a site",
    "run_command": "running a command", "get_system_info": "checking the system",
    "email_search": "searching Gmail", "email_read": "reading an email",
    "remember_fact": "saving a fact", "see_screen": "looking at the screen",
}


def _activity_label(name, args):
    """Friendly one-liner for what the model is doing right now, shown as its thinking process."""
    base = _ACTIVITY.get(name, "using " + name)
    a = args if isinstance(args, dict) else {}
    hint = str(a.get("path") or a.get("query") or a.get("pattern") or a.get("url") or a.get("uid") or "")
    return base + (": " + hint[:48] if hint else "")


_STATS = {"t": 0.0, "data": {}, "fresh": False, "busy": False}
_STATS_LOCK = threading.Lock()
_STATS_TTL = 6                                          # > the 5s client poll, so steady-state serves from cache
LHM_URL = os.environ.get("PHONE_LHM_URL", "http://localhost:8085/data.json")


def _lhm_cpu_temp():
    """CPU package temperature from LibreHardwareMonitor's web server, or None if LHM isn't running.
    LHM must run elevated (kernel driver) to read CPU temps - see the one-time
    C:\\Users\\jep\\LibreHardwareMonitor\\Enable-CPU-Temp.bat. Until then this returns None and the
    phone simply omits CPU temp; connection-refused is instant, so it never slows /stats."""
    try:
        with urllib.request.urlopen(LHM_URL, timeout=2) as r:
            root = json.load(r)
    except Exception:
        return None
    pkg, cores = [None], []

    def walk(n):
        txt = n.get("Text") or ""
        val = n.get("Value") or ""
        if "\u00b0C" in val:
            try:
                t = float(val.split("\u00b0")[0].strip().replace(",", "."))
            except Exception:
                t = None
            if t is not None:
                if "CPU Package" in txt:
                    pkg[0] = t
                elif "CPU Core" in txt:
                    cores.append(t)
        for c in (n.get("Children") or []):
            walk(c)

    try:
        walk(root)
    except Exception:
        return None
    if pkg[0] is not None:
        return int(round(pkg[0]))
    return int(round(max(cores))) if cores else None


def _gpu_stats():
    """Cached GPU snapshot (temp / util / VRAM) for the 5060 Ti plus the running token total. nvidia-smi
    is spawned by at most ONE thread at a time (single-flight); concurrent pollers get the last snapshot,
    and an empty result is cached too so a failing nvidia-smi backs off instead of spawning per poll."""
    with _STATS_LOCK:
        if (time.time() - _STATS["t"] < _STATS_TTL and _STATS["fresh"]) or _STATS["busy"]:
            return dict(_STATS["data"], tokens=_TOTAL_TOKENS)
        _STATS["busy"] = True                          # this thread owns the refresh; others serve the cache
    data = {}
    try:
        smi = _run(("& \"$env:WINDIR\\System32\\nvidia-smi.exe\" "
                    "--query-gpu=temperature.gpu,utilization.gpu,memory.used,memory.total "
                    "--format=csv,noheader,nounits"), timeout=8)
        best = None
        for row in (smi or "").splitlines():
            parts = [p.strip() for p in row.split(",")]
            if len(parts) >= 4:
                try:
                    vals = [int(float(p)) for p in parts[:4]]
                except ValueError:
                    continue
                if best is None or vals[3] > best[3]:  # the GPU with the most VRAM = the 5060 Ti
                    best = vals
        if best:
            data = {"temp": best[0], "util": best[1], "vram_used": best[2], "vram_total": best[3]}
        # CPU load + system RAM. (CPU TEMPERATURE is deliberately omitted: Windows does not expose it
        # without admin rights or vendor tooling like LibreHardwareMonitor - MSAcpi_ThermalZoneTemperature
        # returns Access denied - so it is left out rather than shown as a fake number.)
        cr = _run("$os=Get-CimInstance Win32_OperatingSystem;"
                  "$c=(Get-CimInstance Win32_Processor|Measure-Object -Property LoadPercentage -Average).Average;"
                  "$u=[math]::Round(($os.TotalVisibleMemorySize-$os.FreePhysicalMemory)/1MB,1);"
                  "$t=[math]::Round($os.TotalVisibleMemorySize/1MB,1);'{0}|{1}|{2}' -f $c,$u,$t", timeout=8)
        try:
            cp = (cr or "").strip().split("|")
            if len(cp) == 3 and cp[0].strip():
                data["cpu"] = int(float(cp[0]))
                data["ram_used"] = round(float(cp[1]), 1)
                data["ram_total"] = round(float(cp[2]), 1)
        except Exception:
            pass
        ct = _lhm_cpu_temp()                            # real CPU temp via LibreHardwareMonitor, if running
        if ct is not None:
            data["cpu_temp"] = ct
    finally:
        with _STATS_LOCK:
            _STATS["data"] = data
            _STATS["t"] = time.time()                  # stamp AFTER the spawn, and cache even an empty result
            _STATS["fresh"] = True
            _STATS["busy"] = False
    return dict(data, tokens=_TOTAL_TOKENS)


def run_agent(client_messages, wall_budget=WALL_BUDGET, max_iters=MAX_TOOL_ITERS,
              report=None, cancel=None):
    """Returns (final_text, tools_used). Injects memory, runs the tool loop on the host.

    wall_budget / max_iters let the async /ask (phone) path run far longer than the SSE path, which
    was built to wait minutes. report(**fields) pushes live telemetry to the job so the phone shows
    what it's doing: report(activity=..., tokens=N) before each tool, report(partial=text, tokens=N)
    as the final answer streams. cancel() (checked at each tool boundary) lets a Stop button abort -
    the model call in flight can't be interrupted, so it takes effect between steps."""
    def _rep(**kw):
        if report:
            try:
                report(**kw)
            except Exception:
                pass
    msgs = [{"role": "system", "content": _system_block(client_messages)}]
    for m in client_messages:
        if m.get("role") in ("user", "assistant") and m.get("content"):
            c = m["content"]
            if isinstance(c, list):
                c = " ".join(b.get("text", "") for b in c if isinstance(b, dict))
            msgs.append({"role": m["role"], "content": c})
    msgs = _cap_history(msgs)

    tools_used = []
    task_text = _last_user_text(client_messages)      # what this episode was about, for episode memory
    trace = []                                        # (tool, key arg, ok|err) per EXECUTED call, in order
    executed = {}                                     # (name,args) already run this turn -> breaks identical-call loops
    DEDUP_EXEMPT = {"run_command", "see_screen", "get_system_info"}  # time-varying: a repeat may be legitimate
    deadline = time.time() + wall_budget
    tokens = 0                                         # tokens generated so far this job (for the live counter)
    for _ in range(max_iters):
        if cancel and cancel():
            break
        msgs = _cap_history(msgs)                      # re-cap EACH iteration: paginated reads / long jobs grow msgs
        _rep(activity="thinking", tokens=tokens)       # reset the label so /poll isn't stuck on the last tool
        resp = _ollama_chat(msgs, tools=TOOL_SPECS)    # past num_ctx mid-loop, and Ollama silently evicts the head
        delta = _tokens_of(resp); tokens += delta; _bump_total(delta); _rep(tokens=tokens)
        msg = resp.get("message", {}) or {}
        content = msg.get("content", "") or ""
        calls = msg.get("tool_calls") or []
        if not calls:                                 # model may have written the call as TEXT - recover it
            calls = _parse_text_tool_calls(content)
        if not calls:
            answer = _strip_tool_calls(content)
            final = answer or "(the model returned an empty reply)"
            _rep(partial=final, tokens=tokens)        # show it the instant it's ready (already generated)
            _harvest_remember(final)
            # CLEAN completion: a real answer, reached inside the loop (not the forced exit below),
            # after at least one tool ran and the LAST tool succeeded -> record the episode so the
            # approach that worked is retrievable for similar future tasks.
            if trace and answer and trace[-1][2] == "ok":
                _save_episode(task_text, trace, answer)
            return final, tools_used
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
            if cancel and cancel():               # Stop pressed mid-turn: skip the remaining side-effectful calls
                msgs.append({"role": "tool", "tool_name": name, "content": "(cancelled by user - stopped)"})
                continue
            try:
                key = (name, json.dumps(args, sort_keys=True, default=str))
            except Exception:
                key = (name, str(args))
            # dedupe only PURE read tools; a repeat of a time-varying tool (run_command etc.) may be legitimate
            if name not in DEDUP_EXEMPT and key in executed:   # Qwen3-Coder re-issues identical calls; skip + nudge on
                msgs.append({"role": "tool", "tool_name": name, "content":
                             "(duplicate of an earlier call - its result is already above; take a DIFFERENT "
                             "action or give your final answer now)"})
                continue
            _rep(activity=_activity_label(name, args), tokens=tokens)   # live "thinking": what it's doing now
            handler = TOOLS.get(name)
            if not handler:
                result = "(unknown tool: %s)" % name
            else:
                try:
                    result = handler(args)
                except Exception as e:            # one bad tool call self-corrects, never kills the turn
                    result = "(tool error in %s: %s: %s)" % (name, type(e).__name__, e)
            trace.append((name, _trace_arg(args), _trace_status(result)))  # executed calls only
            if not (isinstance(result, str) and result.startswith("(tool error")):
                executed[key] = True              # only cache SUCCESS, so a transient error can be retried
            if name == "run_command":             # a command may have changed state cached read results depend on
                executed = {k: v for k, v in executed.items() if k[0] in DEDUP_EXEMPT}
            # read_file gets a bigger cap so a "read it fully" actually delivers the whole page, not 6KB
            rcap = READ_FILE_PAGE + 400 if name == "read_file" else TOOL_RESULT_CAP
            msgs.append({"role": "tool", "content": str(result)[:rcap], "tool_name": name})
        if time.time() > deadline:                # stop looping once we blow the time budget
            break
    # out of iterations / time / cancelled - force a final answer with NO tools offered, so the model
    # must produce text instead of spiralling into another tool call. Stream it if the caller wants.
    fin = _cap_history(msgs + [{"role": "user",
            "content": "Now give me your final answer in plain language, using what the tools returned so far. "
                       "Do NOT emit any tool-call markup; just the answer."}])
    _rep(activity="writing the answer", tokens=tokens)
    resp = _ollama_chat(fin, tools=None,
                        on_delta=(lambda t: _rep(partial=_strip_tool_calls(t), tokens=tokens)) if report else None)
    _bump_total(_tokens_of(resp)); tokens += _tokens_of(resp)
    final = _strip_tool_calls((resp.get("message", {}) or {}).get("content", "")) or "(no final answer)"
    _rep(partial=final, tokens=tokens)
    _harvest_remember(final)
    return final, tools_used


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
        _JOBS[jid] = {"status": "pending", "text": "", "born": time.time(), "tools": [], "cancel": False,
                      "tokens": 0, "activity": ""}

    def _report(**kw):
        # live telemetry -> the job dict, so /poll can hand the phone the partial answer, the current
        # activity ("what it's thinking"), and the running token count.
        with _JOBS_LOCK:
            j = _JOBS.get(jid)
            if not j or j["status"] == "done":
                return
            if "partial" in kw:
                j["status"] = "streaming"
                j["text"] = kw["partial"]
            if "activity" in kw:
                j["activity"] = kw["activity"]
            if "tokens" in kw:
                j["tokens"] = kw["tokens"]

    def _cancel():
        j = _JOBS.get(jid)
        return bool(j and j.get("cancel"))

    def _worker():
        try:                                  # the phone path waits minutes: bigger budget + more iterations
            text, tools = run_agent(messages, wall_budget=JOB_WALL_BUDGET, max_iters=JOB_MAX_ITERS,
                                    report=_report, cancel=_cancel)
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
:root{--bg:#0b0c10;--panel:#15171c;--panel2:#1c2029;--me:#2f6bd8;--ai:#1b1e26;--tx:#e8eaed;--mut:#8b90a0;--acc:#6ea8fe;--danger:#ef5350;--line:#23262e}
*{box-sizing:border-box;-webkit-tap-highlight-color:transparent}
html,body{margin:0;height:100%;background:var(--bg);color:var(--tx);font:16px/1.5 -apple-system,system-ui,sans-serif}
button{border:0;background:none;-webkit-appearance:none;appearance:none;font-family:inherit;cursor:pointer}
#app{display:flex;flex-direction:column;height:100dvh}
header{display:flex;align-items:center;gap:4px;padding:6px 8px;padding-top:calc(6px + env(safe-area-inset-top));background:var(--panel);border-bottom:1px solid var(--line)}
.hbtn{width:44px;height:44px;display:flex;align-items:center;justify-content:center;color:var(--tx);font-size:23px;border-radius:12px;flex:0 0 auto}
.hbtn:active{background:var(--panel2)}
.htitle{flex:1;display:flex;align-items:center;gap:9px;justify-content:center;font-weight:600;font-size:17px}
.dot{width:9px;height:9px;border-radius:50%;background:#3ecf8e;box-shadow:0 0 7px #3ecf8e99;flex:0 0 auto}
.dot.busy{background:#f5b23e;box-shadow:0 0 7px #f5b23e99;animation:pulse 1s ease-in-out infinite}
@keyframes pulse{50%{opacity:.35}}
#stats{display:flex;gap:13px;justify-content:center;align-items:center;padding:5px 10px;background:var(--panel);border-bottom:1px solid var(--line);font-size:11.5px;color:var(--mut);white-space:nowrap;overflow-x:auto}
#stats b{color:var(--tx);font-weight:600}
#stats .hot{color:#f5b23e}
#log{flex:1;overflow-y:auto;padding:16px;display:flex;flex-direction:column;gap:12px;-webkit-overflow-scrolling:touch}
#empty{margin:auto;text-align:center;color:var(--mut);max-width:290px;padding:20px}
#empty .big{font-size:38px;margin-bottom:12px}
#empty h2{color:var(--tx);font-size:20px;margin:0 0 8px;font-weight:600}
#empty p{font-size:14px;line-height:1.65;margin:0}
.msg{max-width:88%;padding:11px 14px;border-radius:18px;white-space:pre-wrap;word-wrap:break-word;overflow-wrap:anywhere;font-size:15.5px;line-height:1.5}
.me{align-self:flex-end;background:var(--me);color:#fff;border-bottom-right-radius:6px}
.ai{align-self:flex-start;background:var(--ai);border-bottom-left-radius:6px}
.ai pre,.ai code{background:#0d0f14;border-radius:8px;font-family:ui-monospace,Menlo,monospace;font-size:13px}
.ai pre{padding:11px;overflow-x:auto;margin:7px 0}.ai code{padding:2px 5px}
.ai .h{font-weight:600;margin:8px 0 2px}.ai .li{margin:2px 0 2px 4px}
.ai .cb{position:relative}.ai .cb pre{padding-top:32px}
.cp{position:absolute;top:6px;right:6px;background:#2a2e38;color:var(--tx);font-size:12px;padding:6px 13px;border-radius:9px;font-weight:600}
.retry{background:var(--acc);color:#04122e;font-size:14px;padding:9px 18px;border-radius:11px;margin-top:9px;font-weight:600}
.think{align-self:flex-start;color:var(--mut);font-style:italic;padding:6px 4px;font-size:14.5px}
form{display:flex;gap:9px;align-items:flex-end;padding:10px 12px;padding-bottom:calc(10px + env(safe-area-inset-bottom));background:var(--panel);border-top:1px solid var(--line)}
textarea{flex:1;resize:none;max-height:140px;min-height:48px;background:#0d0f14;color:var(--tx);border:1px solid #2a2e38;border-radius:15px;padding:13px 15px;font:inherit;line-height:1.35;-webkit-appearance:none}
textarea:focus{outline:none;border-color:var(--acc)}
#b{min-width:72px;height:48px;background:var(--acc);color:#04122e;border-radius:15px;font-weight:700;font-size:15px;flex:0 0 auto}
#b.stop{background:#333844;color:var(--tx)}
#b:active{filter:brightness(.9)}
#ov{position:fixed;inset:0;background:#0009;z-index:9;display:none}#ov.open{display:block}
#drawer{position:fixed;top:0;bottom:0;left:0;width:85%;max-width:345px;background:var(--panel);z-index:10;transform:translateX(-100%);transition:transform .22s cubic-bezier(.4,0,.2,1);display:flex;flex-direction:column;border-right:1px solid var(--line)}
#drawer.open{transform:none}
#dh{display:flex;align-items:center;padding:14px 14px 14px 18px;padding-top:calc(14px + env(safe-area-inset-top));border-bottom:1px solid var(--line);font-weight:700;font-size:19px}
#nw2{margin-left:auto;background:var(--acc);color:#04122e;font-weight:600;font-size:14px;padding:11px 16px;border-radius:12px}
#nw2:active{filter:brightness(.9)}
#chats{flex:1;overflow-y:auto;padding-bottom:env(safe-area-inset-bottom)}
.crow{display:flex;align-items:center;min-height:58px;border-bottom:1px solid #16181e}
.crow.cur{background:#1c2740}
.crow .t{flex:1;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;font-size:15px;padding:17px 4px 17px 18px;display:flex;align-items:center;gap:8px}
.crow .run{width:7px;height:7px;border-radius:50%;background:#f5b23e;flex:0 0 auto;animation:pulse 1s ease-in-out infinite}
.crow .x{width:52px;height:58px;display:flex;align-items:center;justify-content:center;color:var(--mut);font-size:22px;flex:0 0 auto;background:none}
.crow .x:active{color:var(--danger);background:var(--panel2)}
.crow.confirm{background:#2a1517}
.crow.confirm .t{color:var(--danger);font-weight:600;padding-left:18px}
.cbtns{display:flex;gap:7px;padding-right:12px;flex:0 0 auto}
.cdel,.ccancel{height:36px;padding:0 14px;border-radius:10px;font-size:13px;font-weight:600}
.cdel{background:var(--danger);color:#fff}.ccancel{background:#2a2e38;color:var(--tx)}
</style></head><body>
<div id=ov></div>
<div id=drawer><div id=dh>Chats<button id=nw2>+ New</button></div><div id=chats></div></div>
<div id=app>
<header><button id=mn class=hbtn>&#9776;</button><div class=htitle><span class=dot id=dot></span>__NAME__</div><button id=nw class=hbtn>+</button></header>
<div id=stats></div>
<div id=log></div>
<form id=f><textarea id=i rows=1 placeholder="Ask anything..."></textarea><button id=b>Send</button></form>
</div><script>
const TKEY="jepai_token", CKEY="jepai_chats", CURKEY="jepai_cur", MODEL="__MODEL__";
let tok=localStorage.getItem(TKEY);
if(!tok){tok=prompt("Enter your access token (one time):")||"";localStorage.setItem(TKEY,tok);}
const log=document.getElementById("log"),inp=document.getElementById("i"),btn=document.getElementById("b"),
      f=document.getElementById("f"),dot=document.getElementById("dot"),
      drawer=document.getElementById("drawer"),ov=document.getElementById("ov"),chatsEl=document.getElementById("chats"),statsEl=document.getElementById("stats");
function fmtTok(n){n=n||0;return n>=1e6?(n/1e6).toFixed(2)+"M":n>=1e3?(n/1e3).toFixed(1)+"k":(""+n);}
let statsBusy=false;
async function pollStats(){if(statsBusy)return;statsBusy=true;try{const r=await jget("/stats");const s=await r.json();if(!s||(s.temp==null&&s.cpu==null))return;
  let h='';
  if(s.temp!=null)h+='<span>\u{1F321} <b class="'+(s.temp>=78?"hot":"")+'">'+s.temp+'°</b></span>'+
    '<span>GPU <b>'+s.util+'%</b></span>'+
    '<span><b>'+(s.vram_used/1024).toFixed(1)+'</b>/'+Math.round(s.vram_total/1024)+' GB</span>';
  if(s.cpu!=null)h+='<span>CPU <b>'+s.cpu+'%</b>'+(s.cpu_temp!=null?' <b class="'+(s.cpu_temp>=85?"hot":"")+'">'+s.cpu_temp+'°</b>':'')+'</span>';
  if(s.ram_used!=null)h+='<span>RAM <b>'+Math.round(s.ram_used)+'</b>/'+Math.round(s.ram_total)+'</span>';
  h+='<span><b>'+fmtTok(s.tokens)+'</b> tok</span>';
  statsEl.innerHTML=h;}catch(_){}finally{statsBusy=false;}}
function loadChats(){try{return JSON.parse(localStorage.getItem(CKEY))||[]}catch(_){return[]}}
function saveChats(){try{localStorage.setItem(CKEY,JSON.stringify(chats.slice(0,50)))}catch(_){}}
let chats=loadChats(),curId=localStorage.getItem(CURKEY);
let jobs={};   // chatId -> {ctl, jobId, phase, elapsed, partial}  : each chat runs independently
const cur=()=>chats.find(c=>c.id===curId);
function esc(s){return s.replace(/&/g,"&amp;").replace(/</g,"&lt;").replace(/>/g,"&gt;").replace(/"/g,"&quot;").replace(/'/g,"&#39;");}
function inlineMd(s){s=esc(s);
  s=s.replace(/`([^`]+)`/g,(m,c)=>"<code>"+c+"</code>");
  s=s.replace(/\*\*([^*]+)\*\*/g,"<strong>$1</strong>");
  s=s.replace(/\[([^\]]+)\]\((https?:\/\/[^)\s"']+)\)/g,'<a href="$2" target="_blank" rel="noopener">$1</a>');
  return s;}
function md(text){const parts=String(text).split(/```(\w*)\n?([\s\S]*?)```/g);let html="";
  for(let i=0;i<parts.length;i++){
    if(i%3===0){const prose=parts[i];if(!prose)continue;
      for(const ln of prose.split("\n")){
        const h=ln.match(/^(#{1,4})\s+(.*)$/);if(h){html+="<div class=h>"+inlineMd(h[2])+"</div>";continue;}
        const li=ln.match(/^\s*[-*]\s+(.*)$/);if(li){html+="<div class=li>• "+inlineMd(li[1])+"</div>";continue;}
        html+=inlineMd(ln)+"<br>";}
    }else if(i%3===2){html+='<div class=cb><button class=cp>Copy</button><pre><code>'+esc(parts[i])+'</code></pre></div>';}}
  return html;}
function copyText(t){try{const ta=document.createElement("textarea");ta.value=t;ta.style.position="fixed";ta.style.opacity="0";
  document.body.appendChild(ta);ta.focus();ta.select();document.execCommand("copy");document.body.removeChild(ta);}catch(_){}}
function wireCopy(d){d.querySelectorAll(".cp").forEach(b=>{b.onclick=()=>{const code=b.parentNode.querySelector("code");
  if(code)copyText(code.textContent);b.textContent="Copied";setTimeout(()=>b.textContent="Copy",1200);};});}
function renderAi(d,txt){d.className="msg ai";d.innerHTML=md(txt);wireCopy(d);}
const EMPTY='<div id=empty><div class=big>💬</div><h2>Ask me anything</h2><p>I run on your PC and can see your files, code, live system, email, and the web. No restrictions.</p></div>';
function add(cls,txt){const d=document.createElement("div");d.className="msg "+cls;
  if(cls==="ai")renderAi(d,txt);else d.textContent=txt;
  log.appendChild(d);log.scrollTop=log.scrollHeight;return d;}
// ---- per-chat task state ----
function syncControls(){const running=!!jobs[curId];
  btn.textContent=running?"Stop":"Send";btn.className=running?"stop":"";
  dot.className=Object.keys(jobs).length?"dot busy":"dot";}
function paintLive(){const cur0=document.getElementById("live");
  if(!jobs[curId]){if(cur0)cur0.remove();return;}
  const stick=log.scrollHeight-log.scrollTop-log.clientHeight<80;   // only auto-scroll if already near the bottom
  const j=jobs[curId];let b=cur0;
  if(!b){b=document.createElement("div");b.id="live";log.appendChild(b);}
  const meta=fmtTok(j.tokens||0)+" tok · "+(j.elapsed||0)+"s";
  if(j.phase==="streaming"&&j.partial){b.className="msg ai";b.textContent=j.partial;}
  else{b.className="think";b.textContent=(j.activity||"thinking…")+" · "+meta;}
  if(stick)log.scrollTop=log.scrollHeight;}
function render(){log.innerHTML="";const c=cur();
  if(!c||(!c.messages.length&&!jobs[curId])){log.innerHTML=EMPTY;syncControls();return;}
  for(const m of c.messages)add(m.role==="user"?"me":"ai",m.content);
  paintLive();syncControls();}
function renderChats(){chatsEl.innerHTML="";for(const c of chats){
  const r=document.createElement("div");r.className="crow"+(c.id===curId?" cur":"");r.dataset.id=c.id;
  const t=document.createElement("div");t.className="t";
  if(jobs[c.id]){const d=document.createElement("span");d.className="run";t.appendChild(d);}  // running indicator
  t.appendChild(document.createTextNode(c.title||"New chat"));
  const x=document.createElement("button");x.className="x";x.textContent="✕";
  r.appendChild(t);r.appendChild(x);
  t.onclick=()=>openChat(c.id);
  x.onclick=e=>{e.stopPropagation();armDelete(r,c.id);};
  chatsEl.appendChild(r);}}
function drawList(){if(!chatsEl.querySelector('.crow.confirm'))renderChats();}  // background rebuild that spares an open delete-confirm
function armDelete(row,id){renderChats();
  row=chatsEl.querySelector('.crow[data-id="'+id+'"]');if(!row)return;
  row.className="crow confirm";row.innerHTML="";
  const t=document.createElement("div");t.className="t";t.textContent="Delete this chat?";
  const b=document.createElement("div");b.className="cbtns";
  const del=document.createElement("button");del.className="cdel";del.textContent="Delete";
  const can=document.createElement("button");can.className="ccancel";can.textContent="Cancel";
  b.appendChild(del);b.appendChild(can);row.appendChild(t);row.appendChild(b);
  can.onclick=e=>{e.stopPropagation();renderChats();};
  del.onclick=e=>{e.stopPropagation();delChat(id);};}
function persist(c){c=c||cur();if(!c)return;const fu=c.messages.find(m=>m.role==="user");if(fu)c.title=fu.content.slice(0,42);c.ts=Date.now();chats=[c,...chats.filter(x=>x.id!==c.id)];saveChats();drawList();}
function newChat(){const id="c"+Date.now()+Math.random().toString(36).slice(2,6);chats.unshift({id,title:"New chat",messages:[],ts:Date.now()});curId=id;localStorage.setItem(CURKEY,id);saveChats();render();renderChats();closeDrawer();inp.focus();}
function openChat(id){curId=id;localStorage.setItem(CURKEY,id);render();renderChats();closeDrawer();}
function delChat(id){const j=jobs[id];if(j){j.ctl.stop=true;try{jget("/cancel?id="+j.jobId);}catch(_){}}delete jobs[id];
  chats=chats.filter(c=>c.id!==id);saveChats();if(curId===id)curId=chats[0]&&chats[0].id;localStorage.setItem(CURKEY,curId||"");
  if(!chats.length)newChat();else{render();renderChats();}}
const openDrawer=()=>{drawer.classList.add("open");ov.classList.add("open");};
const closeDrawer=()=>{drawer.classList.remove("open");ov.classList.remove("open");renderChats();};
document.getElementById("mn").onclick=openDrawer;ov.onclick=closeDrawer;
document.getElementById("nw").onclick=newChat;document.getElementById("nw2").onclick=newChat;
if(!chats.length||!cur())newChat();else{render();renderChats();}
inp.addEventListener("input",()=>{inp.style.height="auto";inp.style.height=Math.min(inp.scrollHeight,140)+"px";});
inp.addEventListener("keydown",e=>{if(e.key==="Enter"&&!e.shiftKey){e.preventDefault();f.requestSubmit();}});
const sleep=ms=>new Promise(r=>setTimeout(r,ms));
async function jget(u){return fetch(u,{headers:{"Authorization":"Bearer "+tok}});}
function errBubble(c,msg){const d=add("ai","Error"+(msg?": "+msg:"")+".");
  const rb=document.createElement("button");rb.className="retry";rb.textContent="Retry";
  rb.onclick=()=>{d.remove();sendCurrent(c);};d.appendChild(document.createElement("br"));d.appendChild(rb);}
// Each chat owns its job. The poll loop updates jobs[id] and only touches the DOM when that chat is
// the one on screen, so switching chats keeps every task's timer and Stop button correct.
async function sendCurrent(c){
  if(jobs[c.id])return;                                // one job per chat: ignore a stale Retry during a run
  const id=c.id,ctl={stop:false};jobs[id]={ctl,phase:"working",elapsed:0,partial:""};
  if(id===curId){const em=document.getElementById("empty");if(em)em.remove();paintLive();}
  syncControls();drawList();let result=null;
  try{
    const r=await fetch("/ask",{method:"POST",headers:{"Content-Type":"application/json","Authorization":"Bearer "+tok},body:JSON.stringify({messages:c.messages})});
    if(r.status===401){delete jobs[id];if(id===curId){paintLive();const d=add("ai","Unauthorized - wrong token. Tap to re-enter.");d.style.cursor="pointer";d.onclick=()=>{localStorage.removeItem(TKEY);location.reload();};}syncControls();drawList();return;}
    const {job_id}=await r.json();
    if(!jobs[id]||ctl.stop){jget("/cancel?id="+job_id).catch(()=>{});if(id===curId)render();drawList();return;}  // deleted/stopped during /ask: cancel the REAL server job
    jobs[id].jobId=job_id;let fails=0;
    while(jobs[id]){
      if(ctl.stop){try{await jget("/cancel?id="+job_id);}catch(_){}result={stopped:1};break;}
      await sleep(2500);let j;
      try{const p=await jget("/poll?id="+job_id);if(p.status===404)throw new Error("expired");j=await p.json();fails=0;}
      catch(_){fails++;if(fails>60){result={error:"lost the server"};break;}continue;}
      if(!jobs[id])break;
      if(j.status==="done"){result={answer:j.content||"(empty reply)"};break;}
      jobs[id].phase=(j.status==="streaming"&&j.partial)?"streaming":"working";jobs[id].partial=j.partial||"";jobs[id].elapsed=j.elapsed||0;
      jobs[id].tokens=j.tokens||jobs[id].tokens||0;jobs[id].activity=j.activity||jobs[id].activity||"";
      if(id===curId)paintLive();
    }
  }catch(err){result={error:err.message};}
  if(!jobs[id]){if(id===curId){render();}drawList();return;}  // chat deleted mid-run
  delete jobs[id];
  if(result&&result.answer){c.messages.push({role:"assistant",content:result.answer});persist(c);}
  else if(result&&result.error&&id!==curId){c.messages.push({role:"assistant",content:"(failed: "+result.error+" - resend to retry)"});persist(c);}  // background error must not vanish
  if(id===curId){
    if(result&&result.error){const b=document.getElementById("live");if(b)b.remove();errBubble(c,result.error);}
    else render();
  }
  syncControls();drawList();if(id===curId)inp.focus();
}
f.addEventListener("submit",e=>{e.preventDefault();
  if(jobs[curId]){jobs[curId].ctl.stop=true;return;}         // this chat is running -> Stop
  const q=inp.value.trim();if(!q)return;
  let c=cur();if(!c){newChat();c=cur();}
  const em=document.getElementById("empty");if(em)em.remove();
  [...log.querySelectorAll(".retry")].forEach(b=>{if(b.parentNode)b.parentNode.remove();});  // clear stale error+Retry bubbles
  add("me",q);c.messages.push({role:"user",content:q});persist(c);inp.value="";inp.style.height="auto";
  sendCurrent(c);
});
pollStats();setInterval(pollStats,5000);
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
                elif job:
                    snap = {"status": job["status"], "text": job.get("text", ""),
                            "elapsed": int(time.time() - job["born"]),
                            "tokens": job.get("tokens", 0), "activity": job.get("activity", "")}
            if not job:
                self._send(404, b'{"error":"no such job"}')
            elif job["status"] == "done":
                self._send(200, json.dumps({"status": "done", "content": job["text"],
                                            "tokens": job.get("tokens", 0)}).encode())
            elif snap["status"] == "streaming":          # hand over the partial answer as it forms
                self._send(200, json.dumps({"status": "streaming", "partial": snap["text"],
                                            "elapsed": snap["elapsed"], "tokens": snap["tokens"]}).encode())
            else:
                self._send(200, json.dumps({"status": "pending", "elapsed": snap["elapsed"],
                                            "tokens": snap["tokens"], "activity": snap["activity"]}).encode())
        elif self.path.startswith("/stats"):               # live GPU temp/util/VRAM + running token total
            try:
                self._send(200, json.dumps(_gpu_stats()).encode())
            except Exception as e:
                self._send(200, json.dumps({"error": str(e)[:80]}).encode())
        elif self.path.startswith("/cancel"):              # Stop button: abort the job at its next tool boundary
            jid = parse_qs(urlparse(self.path).query).get("id", [""])[0]
            with _JOBS_LOCK:
                job = _JOBS.get(jid)
                if job:
                    job["cancel"] = True
            self._send(200, json.dumps({"cancelled": bool(job)}).encode())
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


def _warm():
    """Keep the chat model (and embed model) resident so the FIRST phone query of the day isn't a
    cold ~60s load. Pings a load-only request at startup and every ~10 min, so if another job evicts
    the model it reloads while idle instead of on Jep's next question. Best-effort; never fatal."""
    embed_model = getattr(rag, "EMBED_MODEL", "nomic-embed-text") if rag else None
    while True:
        try:                                          # load-only: empty prompt returns once resident
            urllib.request.urlopen(urllib.request.Request(
                OLLAMA + "/api/generate",
                data=json.dumps({"model": MODEL, "keep_alive": KEEP_ALIVE, "prompt": ""}).encode(),
                headers={"Content-Type": "application/json"}, method="POST"), timeout=180).read()
        except Exception as e:
            print("[phone_agent] warm ping failed: %s" % e, file=sys.stderr)
        if embed_model:
            try:
                urllib.request.urlopen(urllib.request.Request(
                    OLLAMA + "/api/embed",
                    data=json.dumps({"model": embed_model, "keep_alive": KEEP_ALIVE, "input": ["warm"]}).encode(),
                    headers={"Content-Type": "application/json"}, method="POST"), timeout=120).read()
            except Exception:
                pass
        time.sleep(600)


def main():
    print("[phone_agent] :%d -> Ollama %s | model=%s | memory=%s | auth=%s | keep_alive=%s | vision=%s | tools=%s"
          % (PORT, OLLAMA, MODEL, "on" if build_context else "off",
             "token" if PHONE_AGENT_TOKEN else "OPEN", KEEP_ALIVE, VISION_MODEL or "off",
             ",".join(TOOLS)), file=sys.stderr)
    if not PHONE_AGENT_TOKEN:
        print("[phone_agent] WARNING: PHONE_AGENT_TOKEN is unset - host tools (PowerShell, file read) "
              "are exposed UNAUTHENTICATED on :%d. Set PHONE_AGENT_TOKEN to require a Bearer token." % PORT,
              file=sys.stderr)
    threading.Thread(target=_warm, daemon=True).start()   # pin the model warm in the background
    ThreadingHTTPServer(("0.0.0.0", PORT), Handler).serve_forever()


if __name__ == "__main__":
    main()
