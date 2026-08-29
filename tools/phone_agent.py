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
EMAIL_GATHER_CAP = int(os.environ.get("PHONE_EMAIL_GATHER_CAP", "26000")) # email_gather returns a whole thread - it must not be cut to 6k or the model only sees 2-3 emails
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
    "HOW YOU REPLY (these outrank everything below):\n"
    "1. NO WARM-UP. Never open with 'Hey Jep', 'what's up', 'just diving in/into', or narrating what you "
    "are ABOUT to do. Lead with the answer or the action. Greet back ONLY if his whole message was a greeting.\n"
    "2. DO, DON'T DESCRIBE. When he asks you to fix, change, apply, improve, or 'work on' code - even "
    "vaguely, even 'get it to 100%' - read the relevant part, then MAKE A REAL EDIT with edit_file/write_file. "
    "If the scope is huge, make ONE correct, self-contained change NOW and say what is next. NEVER end with "
    "'what would you like me to focus on' or 'is there anything specific' - that is a FAILED reply. "
    "Saying you 'fixed', 'changed', 'edited', or 'updated' a file when you did NOT call edit_file/write_file "
    "THIS turn is a LIE: to change a file you MUST call the tool and see it succeed; if you did not call it, "
    "nothing changed - so never claim it did.\n"
    "3. NEVER BLUFF. Do not claim to have done, checked, monitored, watched, or 'kept an eye on' anything "
    "you did not actually do THIS turn. For his facts (grades, dates, numbers, references) search_knowledge "
    "and quote them EXACTLY - never approximate, round, or invent a number. If it is not in your tools, say so.\n"
    "4. His grades and uni results ARE in your searchable knowledge (his results were indexed from his PC). "
    "For ANY grade/result/module question, search_knowledge FIRST and quote the exact figures - do NOT hunt "
    "through Gmail for them. His live Kingston email is a separate Outlook account you cannot read; only "
    "mention that if he wants brand-new results that would post-date what you know.\n"
    "THINK before you answer or act, but be CONCISE - lead with the substance, no preamble, no narrating "
    "your process. Give real depth ONLY where the question needs it (analysis, advice, a design trade-off); "
    "for everything else a tight, direct, correct answer beats a long one. Never pad, and never recap his "
    "situation back to him unprompted.\n"
    "MATCH THE APPROACH TO THE QUESTION. A reasoning / advice / opinion / general-knowledge / "
    "conceptual / how-should-I question you answer with your OWN THINKING - do NOT call any tool, do "
    "NOT go read a file; just think it through and give a considered answer. Only reach for the tools "
    "when the question is genuinely about his ACTUAL files/code, his live PC, his email, or current "
    "web facts. Reading a random file when someone asked you to reason is a mistake - think instead.\n"
    "YOU HAVE FULL ACCESS TO THIS PC. read_file reads ANY file by path (his decompiler source, configs, "
    "anything). list_dir lists ANY folder. run_command runs ANY PowerShell command. edit_file CHANGES a "
    "file (replace exact text) and write_file writes a whole file. So you CAN see AND edit his code, any "
    "file, and the live system. NEVER say you 'cannot access', 'cannot see', 'cannot edit', 'don't have "
    "access to', or 'cannot directly change' his files or code - that is FALSE, you CAN, so USE the tools.\n"
    "WHEN ASKED TO WORK ON CODE, CHANGE IT - DO NOT JUST DESCRIBE IT. If Jep asks you to fix, change, edit, "
    "refactor, clean up, improve, or 'make better' his code - EVEN vaguely, even just 'make the fixes' or "
    "'make it better' - your job is to actually CHANGE the file, not summarize it. Do this: locate the "
    "relevant spot (search_knowledge or find_in_files, then read_file just that part - do NOT read a whole "
    "huge file end to end to describe it), pick the single most valuable CONCRETE, SPECIFIC improvement or "
    "real bug you can identify, IMPLEMENT it with edit_file (or write_file for a whole file), then report "
    "exactly what you changed (file, and before -> after) and why. If it is code that builds, run_command "
    "the build or test to check your change. NEVER end a coding request with only a summary and a question "
    "like 'is there something specific you'd like me to explore?' - that is the #1 failure and it is NOT "
    "acceptable. If the scope is huge (e.g. 'make the whole decompiler better'), do NOT try to do everything "
    "and do NOT just describe it: make ONE real, correct, self-contained improvement RIGHT NOW with edit_file, "
    "report it, then offer to do the next one. One shipped change beats ten described. A reply that only "
    "describes code without an edit_file/write_file call is a failed reply. When he asks for a change, "
    "ACTUALLY MAKE IT.\n"
    "When a question is about his files, code, or PC, CALL THE TOOLS FIRST (list_dir to find the files, "
    "read_file to read them, run_command to inspect the system) and answer from what they return - NOT "
    "from memory or documentation. Your injected memory is background context; the TOOLS are ground "
    "truth, so verify against the real files. His decompiler project is at "
    "C:\\Users\\jep\\Downloads\\luau-decompiler-v2. For current web facts use web_search / web_fetch. "
    "His EMAIL lives in Gmail, NOT on disk: for any order, receipt, delivery, purchase, or message, "
    "call email_search / email_read - do NOT look for .eml files with the file tools. When he asks to "
    "GATHER, pull, list, collect, or 'get all' the emails in a thread or 'from the very first to now', "
    "call email_gather (give it the thread's subject as the query) - it returns EVERY matching email "
    "in full, oldest first. Then PRESENT those emails in order - date, from, subject and the actual "
    "content of each - do NOT replace them with a themed summary. Gathering means SHOWING the emails, "
    "not describing what they are about.\n"
    "To UNDERSTAND his code broadly (how something works, where a thing is, what a subsystem does), "
    "use search_knowledge FIRST - his ENTIRE codebase is indexed there, so you can answer about any "
    "part WITHOUT reading every file. Use read_file for one file's exact contents and list_dir to see "
    "what exists. You canNOT read every file of a large project in one turn (there are hundreds); read "
    "the key ones, use search_knowledge for the rest, and be honest about which you actually read.\n"
    "YOU KNOW JEP PERSONALLY. His FULL personal context - his profile, all his projects, his "
    "preferences and how he likes you to work, and the history of your past conversations and work "
    "together - is indexed in his searchable knowledge under the 'jep:' and 'log:' sources. So for ANY "
    "question about HIM - who he is, his projects, his preferences, what he is working on, decisions "
    "you made together, or anything from your past conversations - call search_knowledge FIRST and "
    "answer from what it returns: personally, by name, as someone who knows him well. You already know "
    "Jep. NEVER say 'I don't have information about you' or 'I don't have access to your personal "
    "data' - that is FALSE; his profile and history are in the knowledge index, so search_knowledge them.\n"
    "BE SNAPPY WHEN THE ANSWER IS ALREADY IN HAND. When the injected memory above, or a single "
    "search_knowledge call, already answers a personal or simple factual question, answer DIRECTLY and "
    "concisely from it - do NOT pile on more tool calls or read file after file to re-verify something "
    "you already have. Quick question, quick answer. Save the deep multi-tool digging for real work - "
    "coding, debugging, analysis - where thoroughness earns its cost; there, stay as thorough as ever.\n"
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


def tool_email_gather(args):
    """Gather a whole thread / all matching emails in ONE call: search, then read EACH match in
    full, oldest-first. For 'gather all emails in this thread' or 'from the very first to now' -
    returns the actual emails, not a summary."""
    if gmail_local is None:
        return "(email unavailable)"
    q = (args or {}).get("query", "").strip()
    if not q:
        return "(no query)"
    try:
        n = int((args or {}).get("n", 20) or 20)
    except Exception:
        n = 20
    try:
        rows = gmail_local.search(q, min(max(n, 1), 40))
    except Exception as e:
        return "(email search error: %s)" % e
    if not rows:
        return "(no matching emails)"
    import email.utils as _eu

    def _dt(r):
        try:
            d = _eu.parsedate_to_datetime(r.get("date", ""))
            return d.timestamp() if d else 0.0
        except Exception:
            return 0.0

    rows = sorted(rows, key=_dt)                     # oldest first = "from the very first till now"
    out = []
    for i, r in enumerate(rows, 1):
        try:
            m = gmail_local.read(r["uid"])
        except Exception:
            out.append("[%d] (read error on uid %s)" % (i, r.get("uid"))); continue
        if m.get("error"):
            out.append("[%d] %s" % (i, m["error"])); continue
        body = (m.get("body") or "").strip()[:2400]
        out.append("==== EMAIL %d of %d ====\nDate: %s\nFrom: %s\nSubject: %s\n\n%s"
                   % (i, len(rows), m.get("date", ""), m.get("sender", ""), m.get("subject", ""), body))
    return ("Gathered %d emails, oldest first:\n\n" % len(out)) + "\n\n".join(out)


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


def tool_edit_file(args):
    """IMPLEMENT a change by replacing an exact string in a file. old_string must match EXACTLY (whitespace
    included) and be unique. Backs up the pre-edit file to <path>.bak so a bad edit is one copy away from undo."""
    a = args or {}
    path = (a.get("path") or "").strip()
    old = a.get("old_string", "")
    new = a.get("new_string", "")
    if not path:
        return "(no path given)"
    if old == "":
        return "(old_string is empty - give the exact text to replace, or use write_file for a whole file)"
    if old == new:
        return "(old_string and new_string are identical - nothing to change)"
    try:
        p = Path(path)
        data = p.read_text(encoding="utf-8", errors="replace")
    except Exception as e:
        return "(cannot read %s: %s)" % (path, e)
    count = data.count(old)
    if count == 0:
        return "(old_string not found in %s - read_file first and copy the exact text, including indentation)" % path
    if count > 1:
        return "(old_string appears %d times in %s - it must be unique; include more surrounding lines)" % (count, path)
    try:
        p.with_name(p.name + ".bak").write_text(data, encoding="utf-8")   # one-level undo
        p.write_text(data.replace(old, new, 1), encoding="utf-8")
    except Exception as e:
        return "(edit failed on %s: %s)" % (path, e)
    return "edited %s - 1 replacement applied (pre-edit backup at %s.bak)" % (path, path)


def tool_write_file(args):
    """Create a new file, or OVERWRITE an existing one, with full content. Use for a new file or a whole-file
    rewrite; prefer edit_file for a small change. Backs up any existing file to <path>.bak first."""
    a = args or {}
    path = (a.get("path") or "").strip()
    content = a.get("content", "")
    if not path:
        return "(no path given)"
    try:
        p = Path(path)
        existed = p.exists()
        if existed:
            p.with_name(p.name + ".bak").write_text(p.read_text(encoding="utf-8", errors="replace"), encoding="utf-8")
        else:
            p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(content, encoding="utf-8")
    except Exception as e:
        return "(write failed on %s: %s)" % (path, e)
    return "wrote %s (%d chars%s)" % (path, len(content), "; previous version backed up to %s.bak" % path if existed else "")


TOOLS = {
    "get_system_info": tool_system_info,
    "run_command": tool_run_command,
    "read_file": tool_read_file,
    "edit_file": tool_edit_file,
    "write_file": tool_write_file,
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
    "email_gather": tool_email_gather,
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
        "name": "edit_file", "description":
        "ACTUALLY EDIT a file to IMPLEMENT a fix or change - do not just describe it, make the change. "
        "Replaces an exact piece of text: old_string must match the file EXACTLY (including whitespace and "
        "indentation) and be unique - include enough surrounding lines to be unique. read_file first to get "
        "the exact text. Makes a .bak backup. This is how you implement code fixes on his PC.",
        "parameters": {"type": "object", "properties": {
            "path": {"type": "string", "description": "path to the file to edit"},
            "old_string": {"type": "string", "description": "exact text to replace (must be unique in the file)"},
            "new_string": {"type": "string", "description": "the replacement text"}},
            "required": ["path", "old_string", "new_string"]}}},
    {"type": "function", "function": {
        "name": "write_file", "description":
        "Create a NEW file, or OVERWRITE an existing one, with full content. Use for a brand-new file or a "
        "whole-file rewrite; for a small change to an existing file use edit_file instead. Backs up any "
        "existing file to .bak first.",
        "parameters": {"type": "object", "properties": {
            "path": {"type": "string", "description": "path to the file to write"},
            "content": {"type": "string", "description": "the full content to write to the file"}},
            "required": ["path", "content"]}}},
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
        "Search EVERYTHING indexed about Jep: his whole personal life AND his code. This is where his uni "
        "GRADES and results, modules and deadlines, his projects, work, YouTube, admin, profile, and the "
        "history of your past conversations live - alongside his Luau decompiler codebase. USE THIS FIRST "
        "for ANY question about HIM or his facts (grades, results, dates, numbers), his code, or his "
        "projects. His grades and results are HERE, not in his email - never search email for grades.",
        "parameters": {"type": "object", "properties": {
            "query": {"type": "string", "description": "what to look up about Jep, his grades/uni/facts, his code, or his notes"}},
            "required": ["query"]}}},
    {"type": "function", "function": {
        "name": "email_search", "description":
        "Search Jep's Gmail inbox (READ-ONLY). Use for his orders, receipts, purchases, deliveries, or "
        "messages. NOT for his uni grades/results - those live in search_knowledge, never here. Gmail "
        "syntax works: from:currys, subject:order, newer_than:30d, etc. Returns a uid per email; use "
        "email_read to open one.",
        "parameters": {"type": "object", "properties": {
            "query": {"type": "string", "description": "Gmail search query"}}, "required": ["query"]}}},
    {"type": "function", "function": {
        "name": "email_read", "description": "Read the full text of one email by its uid (READ-ONLY).",
        "parameters": {"type": "object", "properties": {
            "uid": {"type": "string"}}, "required": ["uid"]}}},
    {"type": "function", "function": {
        "name": "email_gather", "description":
        "Gather ALL emails in a thread / all matching a query in ONE call, read in full and sorted "
        "oldest-first. Use when Jep asks to gather, pull, list, or collect all emails in a thread or "
        "'from the very first to now'. Returns the actual emails (not a summary). Give a Gmail query "
        "that matches the thread, e.g. its subject or subject:\"...\".",
        "parameters": {"type": "object", "properties": {
            "query": {"type": "string", "description": "Gmail query matching the thread/emails to gather"},
            "n": {"type": "integer", "description": "max emails to gather (default 20, cap 40)"}},
            "required": ["query"]}}},
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
            "options": {"num_ctx": NUM_CTX, "temperature": float(os.environ.get("PHONE_TEMP", "0.35")),
                        "num_predict": int(os.environ.get("PHONE_NUM_PREDICT", "8192"))}}
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
    "edit_file": "editing a file", "write_file": "writing a file",
    "find_in_files": "searching your code", "list_dir": "listing files",
    "web_search": "searching the web", "web_fetch": "fetching a page",
    "crawl_site": "crawling a site", "learn_web": "learning a site",
    "run_command": "running a command", "get_system_info": "checking the system",
    "email_search": "searching Gmail", "email_read": "reading an email",
    "email_gather": "gathering the email thread",
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


def _append_gathered(final, gathered_raw):
    """The local model summarises a gathered email thread instead of showing it. When email_gather
    ran, append the real emails so the user ALWAYS gets them - Jep wants the emails, not a digest."""
    if not gathered_raw or "The emails in full" in (final or ""):
        return final
    return (final or "").rstrip() + "\n\n----- The emails in full (oldest first) -----\n\n" + gathered_raw


_CLAIMS_EDIT = re.compile(r"\b(I(?:'ve| have)?\s+(?:fixed|changed|edited|updated|modified|corrected|"
                          r"replaced|rewritten|patched|removed|added|applied|implemented)|"
                          r"the change I made|the fix I made|I(?:'ve| have)?\s+made the (?:change|fix|edit)|"
                          r"changed\s+`[^`]+`\s+to|modified line)\b", re.I)
_EDIT_OBJ = re.compile(r"(\.py|\.rs|\.lua|\.luau|\.js|\.ts|\.json|\.md|\.txt|\bfile\b|\bline \d+\b|"
                       r"\bthe code\b|\bthe function\b|\bthe bug\b|\breturn \w+\()", re.I)


def _bluffed_edit(task_text, answer, edited):
    """The model likes to SAY it fixed a file without calling edit_file. Fire when the reply CLAIMS a
    change to a file/function/bug but no edit_file/write_file actually ran this turn - regardless of how
    the user phrased the ask (fix / sort it / bug / etc.)."""
    a = answer or ""
    return not edited and bool(_CLAIMS_EDIT.search(a)) and bool(_EDIT_OBJ.search(a))


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
    gathered_raw = None                                # last email_gather result: appended verbatim so the
    #                                                    user ALWAYS gets the real emails, never just a summary
    edited_this_turn = False                           # did an edit_file/write_file actually succeed this turn?
    forced_edit = False                                # have we already caught + corrected a bluffed edit once?
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
            if not forced_edit and _bluffed_edit(task_text, answer, edited_this_turn):
                forced_edit = True                    # caught a claimed-but-never-made edit: force the real thing
                _rep(activity="editing a file", tokens=tokens)
                msgs.append({"role": "user", "content":
                    "STOP - you did NOT call edit_file or write_file, so the file is UNCHANGED and your claim "
                    "that you changed it is false. Do it for real now: read_file the file if you need to see it, "
                    "then call edit_file (exact old text -> new text) or write_file. Do not reply again until the "
                    "tool has actually run and succeeded."})
                continue
            final = answer or "(the model returned an empty reply)"
            final = _append_gathered(final, gathered_raw)  # never let a summary hide the real emails
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
                if name == "email_gather" and isinstance(result, str) and result.startswith("Gathered"):
                    gathered_raw = result         # keep the real emails to append to the final answer
                if name in ("edit_file", "write_file"):
                    edited_this_turn = True       # a real file change happened this turn
            if name in ("run_command", "edit_file", "write_file"):   # state changed: invalidate cached read results
                executed = {k: v for k, v in executed.items() if k[0] in DEDUP_EXEMPT}
            # read_file + email_gather get bigger caps so "read/gather it fully" delivers the whole
            # thing, not 6KB (a truncated gather is exactly why the model summarised 2-3 emails)
            if name == "read_file":
                rcap = READ_FILE_PAGE + 400
            elif name == "email_gather":
                rcap = EMAIL_GATHER_CAP
            else:
                rcap = TOOL_RESULT_CAP
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
    final = _append_gathered(final, gathered_raw)      # never let a summary hide the real emails
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

/* ---- Claude-level upgrade ---- */
:root{--bg:#0a0b0f;--panel:#111318;--panel2:#1a1d25;--me:#3b6fe0;--ai:#181b22;--tx:#eceef2;--mut:#9096a6;--acc:#7cb0ff;--danger:#f0655f;--line:#242832;--glow:#7cb0ff33}
header{backdrop-filter:saturate(1.2) blur(8px);background:#111318e6}
.htitle{letter-spacing:.2px}
#log{padding:18px 14px 10px;gap:14px;scroll-behavior:smooth}
.msg{box-shadow:0 1px 2px #0003;animation:rise .22s cubic-bezier(.2,.7,.3,1)}
@keyframes rise{from{opacity:0;transform:translateY(6px)}to{opacity:1;transform:none}}
.me{background:linear-gradient(180deg,#4074e6,#345fd0)}
.ai{border:1px solid var(--line)}
.ai .h{font-size:16px;letter-spacing:.2px}.ai .h1{font-size:19px;font-weight:700;margin:10px 0 4px}
.ai em{font-style:italic;color:#f0f2f6}.ai a{color:var(--acc);text-decoration:underline;text-underline-offset:2px}
.ai blockquote{margin:7px 0;padding:4px 0 4px 12px;border-left:3px solid var(--acc);color:var(--mut)}
.ai hr{border:0;border-top:1px solid var(--line);margin:12px 0}
.ai .ol{margin:2px 0 2px 4px;display:flex;gap:8px}.ai .ol .n{color:var(--acc);font-weight:700;flex:0 0 auto}
.ai table{border-collapse:collapse;margin:8px 0;font-size:13px;display:block;overflow-x:auto}
.ai th,.ai td{border:1px solid var(--line);padding:5px 9px;text-align:left}.ai th{background:#0d0f14;font-weight:600}
.ai .cb .lang{position:absolute;top:8px;left:12px;font-size:11px;color:var(--mut);text-transform:uppercase;letter-spacing:.5px}
.cp{background:#2a2e38cc;backdrop-filter:blur(4px)}
.acts{display:flex;gap:6px;align-self:flex-start;margin:-6px 0 2px 4px}
.acts button{display:flex;align-items:center;gap:5px;color:var(--mut);font-size:12.5px;font-weight:600;padding:6px 11px;border-radius:9px;background:#161922}
.acts button:active{background:var(--panel2);color:var(--tx)}
.acts button.ok{color:#3ecf8e}
.chips{display:flex;flex-wrap:wrap;gap:9px;justify-content:center;margin-top:18px}
.chip{background:var(--panel2);border:1px solid var(--line);color:var(--tx);font-size:13.5px;font-weight:500;padding:11px 15px;border-radius:14px;line-height:1.2;text-align:left;max-width:150px}
.chip:active{border-color:var(--acc);background:#1c2740}
.chip .ci{font-size:17px;margin-right:2px}
#scrolldn{position:absolute;right:14px;bottom:88px;width:42px;height:42px;border-radius:50%;background:var(--panel2);border:1px solid var(--line);color:var(--tx);font-size:20px;display:none;align-items:center;justify-content:center;box-shadow:0 4px 14px #0006;z-index:5}
#scrolldn.show{display:flex}#scrolldn:active{background:#242832}
.mic{width:48px;height:48px;flex:0 0 auto;background:#0d0f14;border:1px solid #2a2e38;border-radius:15px;color:var(--mut);font-size:20px;display:flex;align-items:center;justify-content:center}
.mic:active{color:var(--tx)}.mic.rec{color:var(--danger);border-color:var(--danger);animation:pulse 1s infinite}
#dsearch{margin:10px 12px 6px;padding:10px 13px;background:#0d0f14;border:1px solid #2a2e38;border-radius:12px;color:var(--tx);font:inherit;font-size:14px;width:calc(100% - 24px)}
#dsearch:focus{outline:none;border-color:var(--acc)}
.crow .t[contenteditable=true]{outline:1px solid var(--acc);border-radius:6px;white-space:normal}
.msg.queued{border-style:dashed}
.tstamp{font-size:10.5px;color:var(--mut);align-self:flex-start;margin:-8px 0 0 6px;opacity:.7}
.me + .tstamp{align-self:flex-end;margin-right:6px}
</style></head><body>
<div id=ov></div>
<div id=drawer><div id=dh>Chats<button id=nw2>+ New</button></div><input id=dsearch type=search placeholder="Search chats"><div id=chats></div></div>
<div id=app style="position:relative">
<header><button id=mn class=hbtn>&#9776;</button><div class=htitle><span class=dot id=dot></span>__NAME__</div><button id=nw class=hbtn>+</button></header>
<div id=stats></div>
<div id=log></div>
<button id=scrolldn type=button>&#8595;</button>
<form id=f><textarea id=i rows=1 placeholder="Ask anything..."></textarea><button id=mic class=mic type=button>&#127908;</button><button id=b>Send</button></form>
</div><script>
const TKEY="jepai_token", CKEY="jepai_chats", CURKEY="jepai_cur", MODEL="__MODEL__", RUNKEY="jepai_running";
let tok=localStorage.getItem(TKEY);
if(!tok){tok=prompt("Enter your access token (one time):")||"";localStorage.setItem(TKEY,tok);}
const log=document.getElementById("log"),inp=document.getElementById("i"),btn=document.getElementById("b"),
      f=document.getElementById("f"),dot=document.getElementById("dot"),
      drawer=document.getElementById("drawer"),ov=document.getElementById("ov"),chatsEl=document.getElementById("chats"),statsEl=document.getElementById("stats"),
      scrolldn=document.getElementById("scrolldn"),mic=document.getElementById("mic"),dsearch=document.getElementById("dsearch");
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
const queues={};  // chatId -> [text,...] : messages typed WHILE a job runs, auto-sent in order when it finishes
const cur=()=>chats.find(c=>c.id===curId);
function esc(s){return s.replace(/&/g,"&amp;").replace(/</g,"&lt;").replace(/>/g,"&gt;").replace(/"/g,"&quot;").replace(/'/g,"&#39;");}
function inlineMd(s){s=esc(s);
  s=s.replace(/`([^`]+)`/g,(m,c)=>"<code>"+c+"</code>");
  s=s.replace(/\*\*([^*]+)\*\*/g,"<strong>$1</strong>");
  s=s.replace(/(^|[^*])\*([^*\n]+)\*/g,"$1<em>$2</em>");
  s=s.replace(/(^|[\s(])_([^_\n]+)_(?=$|[\s).,!?])/g,"$1<em>$2</em>");
  s=s.replace(/\[([^\]]+)\]\((https?:\/\/[^)\s"']+)\)/g,'<a href="$2" target="_blank" rel="noopener">$1</a>');
  return s;}
function block(prose){const lines=prose.split("\n");let html="";
  const cells=s=>s.trim().replace(/^\||\|$/g,"").split("|").map(x=>x.trim());
  for(let i=0;i<lines.length;i++){let ln=lines[i];
    if(/^\s*\|.*\|/.test(ln)&&i+1<lines.length&&/^\s*\|?[\s:|-]+\|/.test(lines[i+1])&&lines[i+1].includes("-")){
      const head=cells(ln);let j=i+2;const body=[];
      while(j<lines.length&&/\|/.test(lines[j])){body.push(cells(lines[j]));j++;}
      html+="<table><thead><tr>"+head.map(c=>"<th>"+inlineMd(c)+"</th>").join("")+"</tr></thead><tbody>"+
        body.map(r=>"<tr>"+r.map(c=>"<td>"+inlineMd(c)+"</td>").join("")+"</tr>").join("")+"</tbody></table>";
      i=j-1;continue;}
    const h1=ln.match(/^#\s+(.*)$/);if(h1){html+="<div class='h h1'>"+inlineMd(h1[1])+"</div>";continue;}
    const h=ln.match(/^#{2,6}\s+(.*)$/);if(h){html+="<div class=h>"+inlineMd(ln.replace(/^#{2,6}\s+/,""))+"</div>";continue;}
    if(/^\s*(---|\*\*\*|___)\s*$/.test(ln)){html+="<hr>";continue;}
    const bq=ln.match(/^>\s?(.*)$/);if(bq){html+="<blockquote>"+inlineMd(bq[1])+"</blockquote>";continue;}
    const ol=ln.match(/^\s*(\d+)\.\s+(.*)$/);if(ol){html+="<div class=ol><span class=n>"+ol[1]+".</span><span>"+inlineMd(ol[2])+"</span></div>";continue;}
    const li=ln.match(/^\s*[-*+]\s+(.*)$/);if(li){html+="<div class=li>• "+inlineMd(li[1])+"</div>";continue;}
    if(!ln.trim()){html+="<br>";continue;}
    html+=inlineMd(ln)+"<br>";}
  return html;}
function md(text){const parts=String(text).split(/```(\w*)\n?([\s\S]*?)```/g);let html="";
  for(let i=0;i<parts.length;i++){
    if(i%3===0){if(parts[i])html+=block(parts[i]);}
    else if(i%3===2){const lang=parts[i-1]||"";
      html+='<div class=cb><button class=cp>Copy</button>'+(lang?'<span class=lang>'+esc(lang)+'</span>':'')+'<pre><code>'+esc(parts[i])+'</code></pre></div>';}}
  return html;}
function copyText(t){try{const ta=document.createElement("textarea");ta.value=t;ta.style.position="fixed";ta.style.opacity="0";
  document.body.appendChild(ta);ta.focus();ta.select();document.execCommand("copy");document.body.removeChild(ta);}catch(_){}}
function wireCopy(d){d.querySelectorAll(".cp").forEach(b=>{b.onclick=()=>{const code=b.parentNode.querySelector("code");
  if(code)copyText(code.textContent);b.textContent="Copied";setTimeout(()=>b.textContent="Copy",1200);};});}
function renderAi(d,txt){d.className="msg ai";d.innerHTML=md(txt);wireCopy(d);}
const EMPTY='<div id=empty><div class=big>💬</div><h2>Ask me anything</h2><p>I run on your PC and can see your files, code, live system, email, and the web. No restrictions.</p><div class=chips id=qp></div></div>';
const QP=[["📧","Gather my emails","Gather my most recent emails, show them, and tell me what needs a reply."],
          ["🎓","My uni status","What is my current university situation - my resits, deadlines, and what I still need to do?"],
          ["💻","Check my PC","What is running on my PC right now and how much disk space is free?"],
          ["✅","Plan my day","Based on my recent emails and my projects, what should I focus on today?"]];
function wireChips(){const el=document.getElementById("qp");if(!el)return;
  for(const[ic,label,prompt]of QP){const b=document.createElement("button");b.className="chip";b.type="button";
    b.innerHTML='<span class=ci>'+ic+'</span> '+label;b.onclick=()=>sendText(prompt);el.appendChild(b);}}
function actRow(txt){const a=document.createElement("div");a.className="acts";
  const cp=document.createElement("button");cp.type="button";cp.innerHTML="⧉ Copy";
  cp.onclick=()=>{copyText(txt);cp.innerHTML="✓ Copied";cp.classList.add("ok");setTimeout(()=>{cp.innerHTML="⧉ Copy";cp.classList.remove("ok");},1400);};
  a.appendChild(cp);return a;}
function add(cls,txt){const d=document.createElement("div");d.className="msg "+cls;
  if(cls==="ai")renderAi(d,txt);else d.textContent=txt;
  log.appendChild(d);
  if(cls==="ai"&&txt&&!/^\(/.test(txt)){const a=actRow(txt);log.appendChild(a);d._acts=a;}
  log.scrollTop=log.scrollHeight;return d;}
function regen(){const c=cur();if(!c||jobs[c.id])return;
  while(c.messages.length&&c.messages[c.messages.length-1].role==="assistant")c.messages.pop();
  persist(c);render();sendCurrent(c);}
function sendText(t){let c=cur();if(!c){newChat();c=cur();}
  if(jobs[c.id]){(queues[c.id]=queues[c.id]||[]).push(t);render();return;}
  const em=document.getElementById("empty");if(em)em.remove();
  add("me",t);c.messages.push({role:"user",content:t});persist(c);sendCurrent(c);}
// ---- per-chat task state ----
function syncControls(){const running=!!jobs[curId];const hasText=inp.value.trim().length>0;
  if(running){btn.textContent=hasText?"Queue":"Stop";btn.className=hasText?"":"stop";}
  else{btn.textContent="Send";btn.className="";}
  dot.className=Object.keys(jobs).length?"dot busy":"dot";}
function paintQueue(){[...log.querySelectorAll(".msg.queued")].forEach(b=>b.remove());
  for(const t of (queues[curId]||[])){const b=add("me","⏳ "+t);b.classList.add("queued");b.style.opacity=".55";}}
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
  if(!c||(!c.messages.length&&!jobs[curId])){log.innerHTML=EMPTY;wireChips();syncControls();return;}
  let lastAi=null;
  for(const m of c.messages){const b=add(m.role==="user"?"me":"ai",m.content);if(m.role!=="user"&&b._acts)lastAi=b;}
  if(lastAi&&!jobs[curId]){const rg=document.createElement("button");rg.type="button";rg.innerHTML="↻ Regenerate";rg.onclick=regen;lastAi._acts.appendChild(rg);}
  paintLive();paintQueue();syncControls();}
function renderChats(){chatsEl.innerHTML="";const q=((dsearch&&dsearch.value)||"").toLowerCase().trim();
  for(const c of chats){
  if(q&&!((c.title||"").toLowerCase().includes(q)||(c.messages||[]).some(m=>(m.content||"").toLowerCase().includes(q))))continue;
  const r=document.createElement("div");r.className="crow"+(c.id===curId?" cur":"");r.dataset.id=c.id;
  const t=document.createElement("div");t.className="t";
  if(jobs[c.id]){const d=document.createElement("span");d.className="run";t.appendChild(d);}  // running indicator
  t.appendChild(document.createTextNode(c.title||"New chat"));
  const x=document.createElement("button");x.className="x";x.textContent="✕";
  r.appendChild(t);r.appendChild(x);
  t.onclick=()=>openChat(c.id);
  t.ondblclick=e=>{e.stopPropagation();renameChat(r,c);};
  t.addEventListener("touchstart",()=>{t._lp=setTimeout(()=>{try{navigator.vibrate&&navigator.vibrate(15);}catch(_){}renameChat(r,c);},550);},{passive:true});
  t.addEventListener("touchend",()=>clearTimeout(t._lp));t.addEventListener("touchmove",()=>clearTimeout(t._lp));
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
function delChat(id){const j=jobs[id];if(j){j.ctl.stop=true;try{jget("/cancel?id="+j.jobId);}catch(_){}}delete jobs[id];delete queues[id];
  chats=chats.filter(c=>c.id!==id);saveChats();if(curId===id)curId=chats[0]&&chats[0].id;localStorage.setItem(CURKEY,curId||"");
  if(!chats.length)newChat();else{render();renderChats();}}
const openDrawer=()=>{drawer.classList.add("open");ov.classList.add("open");};
const closeDrawer=()=>{drawer.classList.remove("open");ov.classList.remove("open");renderChats();};
document.getElementById("mn").onclick=openDrawer;ov.onclick=closeDrawer;
document.getElementById("nw").onclick=newChat;document.getElementById("nw2").onclick=newChat;
if(!chats.length||!cur())newChat();else{render();renderChats();}
setTimeout(()=>resumeAll(),0);   // re-attach to any job still running on the server (after a reload)
inp.addEventListener("input",()=>{inp.style.height="auto";inp.style.height=Math.min(inp.scrollHeight,140)+"px";syncControls();});
inp.addEventListener("keydown",e=>{if(e.key==="Enter"&&!e.shiftKey){e.preventDefault();f.requestSubmit();}});
const sleep=ms=>new Promise(r=>setTimeout(r,ms));
async function jget(u){return fetch(u,{headers:{"Authorization":"Bearer "+tok}});}
function errBubble(c,msg){const d=add("ai","Error"+(msg?": "+msg:"")+".");
  const rb=document.createElement("button");rb.className="retry";rb.textContent="Retry";
  rb.onclick=()=>{d.remove();sendCurrent(c);};d.appendChild(document.createElement("br"));d.appendChild(rb);}
// Each chat owns its job. The poll loop updates jobs[id] and only touches the DOM when that chat is
// the one on screen, so switching chats keeps every task's timer and Stop button correct.
// The running job's server id is persisted PER CHAT so a full tab reload / app-kill can re-attach to
// the still-running server job and keep updating live (iOS discards backgrounded tabs).
function loadRun(){try{return JSON.parse(localStorage.getItem(RUNKEY))||{}}catch(_){return{}}}
function setRun(id,jid){try{const m=loadRun();m[id]=jid;localStorage.setItem(RUNKEY,JSON.stringify(m));}catch(_){}}
function clearRun(id){try{const m=loadRun();delete m[id];localStorage.setItem(RUNKEY,JSON.stringify(m));}catch(_){}}
// Poll an EXISTING server job to completion, updating jobs[c.id] live. Used by a fresh send AND by a
// resume-after-reload, so live updates never stop when the phone leaves and returns to the app.
async function pollJob(c,job_id){
  const id=c.id,ctl=jobs[id].ctl;jobs[id].jobId=job_id;setRun(id,job_id);
  if(id===curId){const em=document.getElementById("empty");if(em)em.remove();paintLive();}
  syncControls();drawList();let result=null,fails=0;
  try{
    while(jobs[id]){
      if(ctl.stop){jget("/cancel?id="+job_id).catch(()=>{});result={stopped:1};break;}
      await sleep(2500);let j;
      try{const p=await jget("/poll?id="+job_id);if(p.status===404){result={};break;}j=await p.json();fails=0;}  // 404 = job gone/delivered: clear quietly
      catch(_){fails++;if(fails>60){result={error:"lost the server"};break;}continue;}
      if(!jobs[id])break;
      if(j.status==="done"){result={answer:j.content||"(empty reply)"};break;}
      jobs[id].phase=(j.status==="streaming"&&j.partial)?"streaming":"working";jobs[id].partial=j.partial||"";jobs[id].elapsed=j.elapsed||0;
      jobs[id].tokens=j.tokens||jobs[id].tokens||0;jobs[id].activity=j.activity||jobs[id].activity||"";
      if(id===curId)paintLive();
    }
  }catch(err){result={error:err.message};}
  clearRun(id);
  if(!jobs[id]){if(id===curId)render();drawList();return;}   // chat deleted mid-run
  delete jobs[id];
  if(result&&result.answer){c.messages.push({role:"assistant",content:result.answer});persist(c);}
  else if(result&&result.error&&id!==curId){c.messages.push({role:"assistant",content:"(failed: "+result.error+" - resend to retry)"});persist(c);}
  if(id===curId){
    if(result&&result.error){const b=document.getElementById("live");if(b)b.remove();errBubble(c,result.error);}
    else render();
  }
  if(queues[id]&&queues[id].length){                 // a message was queued while this ran -> send it now
    const nextText=queues[id].shift();if(!queues[id].length)delete queues[id];
    c.messages.push({role:"user",content:nextText});persist(c);
    if(id===curId)render();
    sendCurrent(c);return;
  }
  syncControls();drawList();if(id===curId)inp.focus();
}
async function sendCurrent(c){
  if(jobs[c.id])return;                                // one job per chat: ignore a stale Retry during a run
  try{navigator.vibrate&&navigator.vibrate(8);}catch(_){}
  const id=c.id;jobs[id]={ctl:{stop:false},phase:"working",elapsed:0,partial:"",tokens:0,activity:""};
  if(id===curId){const em=document.getElementById("empty");if(em)em.remove();paintLive();}
  syncControls();drawList();let job_id;
  try{
    const r=await fetch("/ask",{method:"POST",headers:{"Content-Type":"application/json","Authorization":"Bearer "+tok},body:JSON.stringify({messages:c.messages})});
    if(r.status===401){delete jobs[id];if(id===curId){paintLive();const d=add("ai","Unauthorized - wrong token. Tap to re-enter.");d.style.cursor="pointer";d.onclick=()=>{localStorage.removeItem(TKEY);location.reload();};}syncControls();drawList();return;}
    job_id=(await r.json()).job_id;
  }catch(err){delete jobs[id];if(id===curId)errBubble(c,err.message);syncControls();drawList();return;}
  if(!jobs[id]||jobs[id].ctl.stop){jget("/cancel?id="+job_id).catch(()=>{});delete jobs[id];if(id===curId)render();drawList();return;}
  pollJob(c,job_id);
}
// re-attach to still-running server jobs after a reload or when the app returns to the foreground
function resumeJob(c,job_id){if(jobs[c.id])return;jobs[c.id]={ctl:{stop:false},phase:"working",elapsed:0,partial:"",tokens:0,activity:""};pollJob(c,job_id);}
function resumeAll(){const m=loadRun();for(const id in m){const c=chats.find(x=>x.id===id);if(c&&!jobs[id])resumeJob(c,m[id]);else if(!c)clearRun(id);}}
f.addEventListener("submit",e=>{e.preventDefault();
  if(jobs[curId]){                                            // this chat is busy
    const qq=inp.value.trim();
    if(qq){(queues[curId]=queues[curId]||[]).push(qq);inp.value="";inp.style.height="auto";render();}  // type while running -> QUEUE it
    else{jobs[curId].ctl.stop=true;}                          // empty + running -> Stop
    return;
  }
  const q=inp.value.trim();if(!q)return;
  let c=cur();if(!c){newChat();c=cur();}
  const em=document.getElementById("empty");if(em)em.remove();
  [...log.querySelectorAll(".retry")].forEach(b=>{if(b.parentNode)b.parentNode.remove();});  // clear stale error+Retry bubbles
  add("me",q);c.messages.push({role:"user",content:q});persist(c);inp.value="";inp.style.height="auto";
  sendCurrent(c);
});
document.addEventListener("visibilitychange",()=>{if(document.visibilityState==="visible")resumeAll();});  // app back in foreground -> reconnect any running job
window.addEventListener("pageshow",()=>resumeAll());
function updateScrollBtn(){if(!scrolldn)return;const gap=log.scrollHeight-log.scrollTop-log.clientHeight;scrolldn.classList.toggle("show",gap>140);}
log.addEventListener("scroll",updateScrollBtn);
scrolldn.onclick=()=>{log.scrollTop=log.scrollHeight;updateScrollBtn();};
let rec=null;
mic.onclick=()=>{const SR=window.SpeechRecognition||window.webkitSpeechRecognition;
  if(!SR){mic.textContent="🚫";setTimeout(()=>mic.innerHTML="&#127908;",1200);return;}
  if(rec){rec.stop();return;}
  rec=new SR();rec.lang="en-GB";rec.interimResults=true;
  const base=inp.value?inp.value.trim()+" ":"";
  rec.onresult=e=>{let t="";for(let i=0;i<e.results.length;i++)t+=e.results[i][0].transcript;
    inp.value=base+t;inp.style.height="auto";inp.style.height=Math.min(inp.scrollHeight,140)+"px";syncControls();};
  rec.onend=()=>{mic.classList.remove("rec");rec=null;inp.focus();};
  rec.onerror=()=>{mic.classList.remove("rec");rec=null;};
  mic.classList.add("rec");try{rec.start();}catch(_){mic.classList.remove("rec");rec=null;}};
dsearch.addEventListener("input",()=>renderChats());
function renameChat(row,c){const t=row&&row.querySelector(".t");if(!t)return;
  t.setAttribute("contenteditable","true");t.textContent=c.title||"";t.focus();
  const sel=document.getSelection();if(sel){sel.selectAllChildren(t);}
  const save=()=>{t.removeAttribute("contenteditable");const v=t.textContent.trim();
    if(v){c.title=v.slice(0,48);c.ts=Date.now();chats=[c,...chats.filter(x=>x.id!==c.id)];saveChats();}renderChats();};
  t.onblur=save;t.onkeydown=e=>{if(e.key==="Enter"){e.preventDefault();t.blur();}};}
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
