"""ingest_personal.py - index Jep's own Claude Code chat history into the local knowledge store.

Jep asked the local model to "know me and all the chats and logs we have". The distilled
memory files are already indexed under `jep:` (via rag.ingest_dir). This adds the RAW
conversation history under `log:` so the model can recall what Jep actually said and what
was decided, in his own words.

What it does, and the safety rails on it:
- Reads the session transcripts at SESS_DIR/*.jsonl (Claude Code's own logs).
- Keeps ONLY conversational text: Jep's typed messages IN FULL (highest signal about him)
  and a capped gist of each assistant reply. Skips thinking / tool_use / tool_result /
  images / system-injected reminders - none of which say anything about Jep.
- SCRUBS secrets before anything is embedded: exact values of known env-var secrets read at
  runtime (never printed), plus generic shapes (32+ hex tokens, Bearer/sk-/gh_/xox tokens,
  `password:`/`token:`/`api_key:` assignments). Proven by `--selftest` on planted secrets,
  which never touches the real store.
- Writes one scrubbed .md per session to a temp dir, then hands them to rag.ingest_dir with
  source_prefix='log:' - so it reuses the store's single-writer lock, per-chunk dedup, and
  NON-DESTRUCTIVE flush (existing roblox:/jep:/digest: rows are untouched).

Usage:
    python tools/ingest_personal.py --selftest        # prove the scrub, write nothing
    python tools/ingest_personal.py --dry-run          # extract+scrub, report counts, no embed
    python tools/ingest_personal.py                    # ingest for real (needs GPU free for nomic)
"""
from __future__ import annotations
import json, os, re, sys, glob, tempfile, shutil, time, pathlib

SESS_DIR = r"C:\Users\jep\.claude\projects\C--Users-jep-Downloads-luau-decompiler-v2"
ASSISTANT_GIST_CHARS = 500      # cap each assistant reply to its gist; Jep's messages are kept whole
MIN_MSG_CHARS = 8               # drop empty / trivial fragments
MAX_FILE = 10_000_000           # a whole session doc; rag.ingest_dir's 300KB default would skip big ones

# --- user messages that are injected context, not something Jep typed ---
_INJECTED_MARKERS = (
    "<system-reminder>", "<command-name>", "<local-command-stdout>", "<task-notification>",
    "[SYSTEM NOTIFICATION", "This session is being continued", "Codebase and user instructions",
    "[Request interrupted", "Caveat: The messages below", "<user-memory-input>",
    "SessionStart", "DO NOT respond to these", "# claudeMd",
)


def build_scrubbers():
    """Regex list applied to every text before it is written/embedded. Order matters:
    exact env values first, then generic shapes."""
    pats = []
    # Exact values of secrets this project is known to use. Read at runtime; the VALUE is never
    # logged or printed - only the redaction marker is. If a var is unset we simply skip it.
    for var in ("PHONE_AGENT_TOKEN", "GMAIL_APP_PASSWORD", "GMAIL_APP_PW", "ELEVENLABS_API_KEY",
                "OPENAI_API_KEY", "ANTHROPIC_API_KEY", "OLLAMA_API_KEY", "HF_TOKEN"):
        v = (os.environ.get(var) or "").strip()
        if len(v) >= 8:
            pats.append((re.compile(re.escape(v)), f"[REDACTED:{var}]"))
    # Generic secret shapes.
    pats += [
        (re.compile(r"\bBearer\s+[A-Za-z0-9._\-]{8,}"), "Bearer [REDACTED]"),
        (re.compile(r"\bsk-[A-Za-z0-9]{16,}"), "[REDACTED_KEY]"),
        (re.compile(r"\bgh[pousr]_[A-Za-z0-9]{20,}"), "[REDACTED_GH]"),
        (re.compile(r"\bxox[baprs]-[A-Za-z0-9-]{10,}"), "[REDACTED_SLACK]"),
        (re.compile(r"\bAKIA[0-9A-Z]{16}\b"), "[REDACTED_AWS]"),
        # 32+ hex run: session/API tokens, PHONE_AGENT_TOKEN, full git SHAs. Losing a SHA is fine.
        (re.compile(r"\b[0-9a-fA-F]{32,}\b"), "[REDACTED_HEX]"),
        # key: value / password = value assignments. The key may carry a prefix like
        # PHONE_AGENT_ so "PHONE_AGENT_TOKEN=..." is caught; the required :/= right after the
        # keyword stops it firing inside longer words ("tokenizer:").
        (re.compile(r"(?i)([A-Za-z0-9]*(?:password|passwd|pwd|api[_-]?key|secret|token|access[_-]?key))"
                    r"\s*[:=]\s*[\"']?([^\s\"'<>]{6,})"), r"\1: [REDACTED]"),
    ]
    return pats


def scrub(text: str, pats) -> str:
    for rx, repl in pats:
        text = rx.sub(repl, text)
    return text


def _looks_injected(text: str) -> bool:
    head = text.lstrip()[:400]
    return any(mk in head for mk in _INJECTED_MARKERS) or head.startswith("<")


def _text_of(content) -> str:
    """Pull plain text out of a message.content (str or list of blocks). Only 'text' blocks;
    thinking / tool_use / tool_result / image are dropped."""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        out = []
        for b in content:
            if isinstance(b, dict) and b.get("type") == "text":
                t = b.get("text")
                if isinstance(t, str):
                    out.append(t)
        return "\n".join(out)
    return ""


def extract_session(path: str, pats) -> tuple[str, int, int]:
    """Return (scrubbed_markdown, n_jep_msgs, n_claude_gists) for one transcript, or ('',0,0).

    One assistant gist per Jep turn: the assistant's text between two Jep messages is
    concatenated and capped, so a multi-step tool loop collapses to a single reply gist
    instead of dozens of intermediate 'let me check...' fragments.
    """
    jep_n = claude_n = 0
    parts = []
    pending = []                                  # assistant text awaiting the next Jep turn

    def flush_pending():
        nonlocal claude_n
        if not pending:
            return
        joined = "\n".join(pending).strip()
        pending.clear()
        if len(joined) < MIN_MSG_CHARS:
            return
        gist = joined[:ASSISTANT_GIST_CHARS]
        if len(joined) > ASSISTANT_GIST_CHARS:
            gist += " ..."
        parts.append("Claude: " + gist)
        claude_n += 1

    with open(path, encoding="utf-8", errors="replace") as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                o = json.loads(line)
            except Exception:
                continue
            if o.get("type") not in ("user", "assistant"):
                continue
            m = o.get("message")
            if not isinstance(m, dict):
                continue
            role = m.get("role")
            text = _text_of(m.get("content")).strip()
            if len(text) < MIN_MSG_CHARS:
                continue
            if role == "user":
                if _looks_injected(text):
                    continue
                flush_pending()                   # reply to the PREVIOUS Jep turn
                parts.append("Jep: " + text)
                jep_n += 1
            elif role == "assistant":
                pending.append(text)
    flush_pending()
    if not parts:
        return "", 0, 0
    doc = scrub("\n\n".join(parts), pats)
    return doc, jep_n, claude_n


def selftest() -> int:
    """Prove the scrub redacts secrets. Writes NOTHING to the store."""
    pats = build_scrubbers()
    planted = [
        ("bearer", "Authorization: Bearer abcDEF123456789xyz token", "Bearer [REDACTED]"),
        ("hex32", "the token is d31e0bff" + "a" * 26 + " ok", "[REDACTED_HEX]"),
        ("openai", "key sk-" + "A" * 40, "[REDACTED_KEY]"),
        ("github", "clone with ghp_" + "B" * 36, "[REDACTED_GH]"),
        ("assign", 'PHONE_AGENT_TOKEN="hunter2secret"', "[REDACTED]"),
        ("password", "password: swordfish99", "[REDACTED]"),
    ]
    ok = True
    for name, raw, must in planted:
        out = scrub(raw, pats)
        leaked = must not in out
        # also assert the raw secret body is gone (not just that a marker exists)
        bad = any(s in out for s in ("hunter2secret", "swordfish99", "AAAAAAAA"))
        status = "PASS" if (not leaked and not bad) else "FAIL"
        if status == "FAIL":
            ok = False
        print(f"  [{status}] {name}: {out}")
    # env-value redaction, if a real secret is present in the environment
    for var in ("PHONE_AGENT_TOKEN", "GMAIL_APP_PASSWORD"):
        v = (os.environ.get(var) or "").strip()
        if len(v) >= 8:
            out = scrub(f"here is the value {v} inline", pats)
            leaked = v in out
            print(f"  [{'PASS' if not leaked else 'FAIL'}] env {var} redacted (value not printed)")
            if leaked:
                ok = False
    print("SELFTEST:", "OK" if ok else "FAILED")
    return 0 if ok else 1


def main() -> int:
    args = set(sys.argv[1:])
    if "--selftest" in args:
        return selftest()
    dry = "--dry-run" in args

    pats = build_scrubbers()
    files = sorted(glob.glob(os.path.join(SESS_DIR, "*.jsonl")))
    print(f"ingest_personal: {len(files)} session transcripts under {SESS_DIR}")
    tmp = tempfile.mkdtemp(prefix="jeplogs_")
    total_jep = total_claude = written = 0
    try:
        for f in files:
            doc, jn, cn = extract_session(f, pats)
            if not doc:
                continue
            sid = pathlib.Path(f).stem[:8]
            out = os.path.join(tmp, f"log_{sid}.md")
            with open(out, "w", encoding="utf-8") as w:
                w.write(doc)
            total_jep += jn
            total_claude += cn
            written += 1
        print(f"  extracted {written} sessions: {total_jep} Jep messages, "
              f"{total_claude} assistant gists (scrubbed)")
        if dry:
            # show a sample so the scrub can be eyeballed, then stop
            sample = sorted(glob.glob(os.path.join(tmp, "*.md")))
            if sample:
                head = open(sample[0], encoding="utf-8").read()[:800]
                print("  --- sample (first doc, first 800 chars) ---")
                print("  " + head.replace("\n", "\n  "))
            print("DRY RUN: nothing embedded or written to the store.")
            return 0
        sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
        import tools.rag as rag
        t0 = time.time()
        n = rag.ingest_dir(tmp, exts=(".md",), source_prefix="log:", max_file=MAX_FILE)
        print(f"ingest_dir: +{n} chunks under 'log:' in {time.time()-t0:.0f}s")
        return 0
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    raise SystemExit(main())
