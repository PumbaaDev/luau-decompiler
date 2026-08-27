#!/usr/bin/env python3
"""digest_codebase - read EVERY source file, compress each to a summary, and build a whole-project
digest the local model can hold even though the raw code never fits one context window (map-reduce /
hierarchical summarization).

Per file: the local model writes a tight technical summary (purpose, key funcs/structs, role). Files
too big for the context window are chunked, each chunk summarized, then combined. All summaries are
saved incrementally (RESUMABLE - a re-run skips files already done), written to CODEBASE_DIGEST.md,
reduced into per-crate + architecture summaries, and indexed into the RAG so search_knowledge can
retrieve the compressed understanding of the ENTIRE codebase.

    python tools/digest_codebase.py            # run / resume
    python tools/digest_codebase.py reduce     # just re-run the crate/architecture reduce + index
"""
from __future__ import annotations
import json, os, sys, time, urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OLLAMA = os.environ.get("OLLAMA_URL", "http://localhost:11434")
MODEL = os.environ.get("DIGEST_MODEL", "uncensored-coder")
OUT_JSONL = ROOT / "goal" / "codebase_digest.jsonl"
OUT_MD = ROOT / "CODEBASE_DIGEST.md"
ROOTS = [("crates", "**/*.rs"), ("tools", "**/*.py")]
SKIP_DIRS = {"target", "node_modules", ".git", ".venv", "__pycache__", ".serena", ".drive_scan"}
FIT_CHARS = 38000          # ~ fits num_ctx 16384 with room for the prompt + output
CHUNK_CHARS = 34000
PER_CALL_TIMEOUT = int(os.environ.get("DIGEST_TIMEOUT", "180"))
NUM_CTX = 16384


def _gen(prompt, num_predict=420):
    body = json.dumps({"model": MODEL, "prompt": prompt, "stream": False,
                       "options": {"num_ctx": NUM_CTX, "num_predict": num_predict, "temperature": 0}}).encode()
    req = urllib.request.Request(OLLAMA + "/api/generate", data=body,
                                 headers={"content-type": "application/json"})
    with urllib.request.urlopen(req, timeout=PER_CALL_TIMEOUT) as r:
        return json.load(r).get("response", "").strip()


def summarize_file(rel, text):
    """One tight summary for a file; big files are chunked then combined."""
    if len(text) <= FIT_CHARS:
        p = ("Summarize this source file for a codebase digest. 4-8 concise technical bullets: its "
             "PURPOSE, the key functions/structs/types it defines, the important logic/algorithms, and "
             "its ROLE in the project. No preamble, just the bullets.\n\nFILE: %s\n\n%s" % (rel, text))
        return _gen(p)
    # chunk -> summarize each -> combine
    parts = [text[i:i + CHUNK_CHARS] for i in range(0, len(text), CHUNK_CHARS)]
    chunk_summaries = []
    for j, ch in enumerate(parts):
        p = ("This is chunk %d/%d of the source file %s. Summarize what THIS chunk defines/does in "
             "4-6 technical bullets.\n\n%s" % (j + 1, len(parts), rel, ch))
        try:
            chunk_summaries.append(_gen(p, 320))
        except Exception as e:
            chunk_summaries.append("(chunk %d summary failed: %s)" % (j + 1, e))
    combine = ("These are summaries of consecutive chunks of one big source file (%s). Combine them "
               "into ONE concise file summary: 5-9 technical bullets covering purpose, key items, "
               "logic, and role.\n\n%s" % (rel, "\n\n".join(chunk_summaries)))
    return _gen(combine, 520)


def load_done():
    done = {}
    if OUT_JSONL.exists():
        for line in OUT_JSONL.read_text(encoding="utf-8").splitlines():
            if line.strip():
                try:
                    d = json.loads(line)
                    done[d["path"]] = d
                except Exception:
                    pass
    return done


def collect_files():
    out = []
    for base, pat in ROOTS:
        for p in sorted((ROOT / base).glob(pat)):
            if p.is_file() and not (SKIP_DIRS & set(p.parts)):
                out.append(p)
    return out


def build():
    OUT_JSONL.parent.mkdir(exist_ok=True)
    files = collect_files()
    done = load_done()
    todo = [p for p in files if str(p.relative_to(ROOT)).replace("\\", "/") not in done]
    print("digest: %d files total, %d already done, %d to do" % (len(files), len(done), len(todo)), file=sys.stderr)
    t0 = time.time()
    for n, p in enumerate(todo):
        rel = str(p.relative_to(ROOT)).replace("\\", "/")
        try:
            text = p.read_text(encoding="utf-8", errors="replace")
        except Exception as e:
            print("  read fail %s: %s" % (rel, e), file=sys.stderr)
            continue
        try:
            summary = summarize_file(rel, text)
        except Exception as e:
            summary = "(summary failed: %s: %s)" % (type(e).__name__, e)
        with OUT_JSONL.open("a", encoding="utf-8") as f:   # incremental + resumable
            f.write(json.dumps({"path": rel, "bytes": len(text), "summary": summary}) + "\n")
        el = time.time() - t0
        print("  [%d/%d] %-55s %.0fs (%.0fs elapsed)" % (n + 1, len(todo), rel[:55], el / (n + 1), el),
              file=sys.stderr)
    reduce_and_index()


def reduce_and_index():
    """Write the markdown digest, a per-crate + architecture reduce, and index it all into the RAG."""
    done = load_done()
    if not done:
        print("nothing to reduce", file=sys.stderr)
        return
    # group by top-level module (crate name or tools)
    groups = {}
    for rel, d in sorted(done.items()):
        parts = rel.split("/")
        grp = parts[1] if parts[0] == "crates" and len(parts) > 1 else parts[0]
        groups.setdefault(grp, []).append((rel, d["summary"]))

    lines = ["# Codebase Digest", "",
             "Model-written summary of every source file (%d files), for whole-project understanding." % len(done),
             "Generated by tools/digest_codebase.py.", ""]
    module_summaries = {}
    for grp, items in groups.items():
        lines.append("## %s  (%d files)" % (grp, len(items)))
        # reduce: a module-level summary from its file summaries
        joined = "\n".join("- %s: %s" % (r, s.replace("\n", " ")[:300]) for r, s in items)
        try:
            ms = _gen("These are per-file summaries for the '%s' module. Write a 4-8 bullet overview of "
                      "what this module does as a whole and how its files fit together.\n\n%s" % (grp, joined[:30000]), 480)
        except Exception as e:
            ms = "(module summary failed: %s)" % e
        module_summaries[grp] = ms
        lines.append("\n**Module overview:**\n%s\n" % ms)
        for r, s in items:
            lines.append("### %s\n%s\n" % (r, s))
    # architecture reduce
    try:
        arch = _gen("These are module overviews of a Luau decompiler project. Write a concise "
                    "ARCHITECTURE summary (6-12 bullets): the main components, how data flows "
                    "(bytecode -> parse -> lift -> emit), and where the key logic lives.\n\n%s"
                    % "\n\n".join("%s:\n%s" % (g, m) for g, m in module_summaries.items())[:30000], 700)
    except Exception as e:
        arch = "(architecture summary failed: %s)" % e
    lines[4:4] = ["## Architecture", arch, ""]
    OUT_MD.write_text("\n".join(lines), encoding="utf-8")
    print("wrote %s (%d files)" % (OUT_MD.name, len(done)), file=sys.stderr)

    # index the digest into the RAG so search_knowledge retrieves the compressed understanding
    try:
        sys.path.insert(0, str(ROOT / "tools"))
        import rag
        pages = [(rel, "digest", "SUMMARY of %s:\n%s" % (rel, d["summary"])) for rel, d in done.items()]
        pages.append(("ARCHITECTURE", "digest", "PROJECT ARCHITECTURE:\n" + arch))
        added = rag.append_pages(pages, source_prefix="digest:")
        print("indexed %d digest summaries into the RAG (searchable via search_knowledge)" % added, file=sys.stderr)
    except Exception as e:
        print("RAG index of digest failed: %s" % e, file=sys.stderr)


def main():
    if len(sys.argv) > 1 and sys.argv[1] == "reduce":
        reduce_and_index()
    else:
        build()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
