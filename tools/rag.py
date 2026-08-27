#!/usr/bin/env python3
"""rag - a local semantic knowledge layer over Jep's codebase + Obsidian vault.

The curated `mem.py` store holds ~500 hand-written facts; this adds SEMANTIC search over the whole
corpus so the local model can actually pull the relevant Rust/Python source or vault note for a
question, in depth, instead of guessing. Fully local: embeds with Ollama's nomic-embed-text, stores
vectors in a plain .npy + .jsonl (numpy is the only dep), searches by cosine similarity.

    python tools/rag.py index                 # (re)build the index over the corpus
    python tools/rag.py search "how does the lifter handle DUPCLOSURE" [k]
    python tools/rag.py stats

Wired into tools/phone_agent.py as the `search_knowledge` tool, so the phone model searches Jep's
code + notes on demand.
"""
from __future__ import annotations

import json
import os
import re
import sys
import time
import urllib.request
from pathlib import Path

import numpy as np

ROOT = Path(__file__).resolve().parent.parent
OLLAMA = os.environ.get("OLLAMA_URL", "http://localhost:11434")
EMBED_MODEL = os.environ.get("RAG_EMBED_MODEL", "nomic-embed-text")
IDX_DIR = ROOT / "goal"
META_PATH = IDX_DIR / "rag_index.jsonl"
VEC_PATH = IDX_DIR / "rag_vectors.npy"
CHUNK_CHARS = int(os.environ.get("RAG_CHUNK_CHARS", "1500"))
CHUNK_OVERLAP = int(os.environ.get("RAG_CHUNK_OVERLAP", "200"))

# what to index: the codebase + docs + the Obsidian vault (NOT local_mem facts - mem.py covers those)
SOURCE_GLOBS = [
    ("crates", "**/*.rs"),
    ("tools", "**/*.py"),
    ("tools", "**/*.ps1"),
    (".", "CLAUDE.md"),
]
VAULT_DIR = Path(os.environ.get("VAULT_DIR", r"C:\Users\jep\Documents\ClaudeMemory"))
SKIP_DIRS = {"target", "node_modules", ".git", ".venv", "__pycache__", ".serena", ".drive_scan"}
MAX_FILE_BYTES = 400_000   # skip giant generated files


def _embed(text: str, timeout: int = 60) -> list[float]:
    body = json.dumps({"model": EMBED_MODEL, "prompt": text}).encode()
    req = urllib.request.Request(OLLAMA + "/api/embeddings", data=body,
                                 headers={"Content-Type": "application/json"}, method="POST")
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.load(r).get("embedding", [])


def _chunk(text: str, path: str):
    """Split into ~CHUNK_CHARS windows with overlap, on line boundaries where possible."""
    text = text.replace("\r\n", "\n")
    if len(text) <= CHUNK_CHARS:
        return [text] if text.strip() else []
    out, i = [], 0
    while i < len(text):
        end = min(i + CHUNK_CHARS, len(text))
        nl = text.rfind("\n", i + CHUNK_CHARS - 300, end)   # prefer to break on a newline
        if nl > i:
            end = nl
        piece = text[i:end].strip()
        if piece:
            out.append(piece)
        if end >= len(text):
            break
        i = max(end - CHUNK_OVERLAP, i + 1)
    return out


def _iter_files():
    for base, pat in SOURCE_GLOBS:
        for p in (ROOT / base).glob(pat):
            if p.is_file() and not (SKIP_DIRS & set(p.parts)):
                yield p
    if VAULT_DIR.exists():
        for p in VAULT_DIR.glob("**/*.md"):
            if p.is_file() and not (SKIP_DIRS & set(p.parts)):
                yield p


def build_index():
    IDX_DIR.mkdir(exist_ok=True)
    chunks = []   # (source, text)
    seen = set()
    for p in _iter_files():
        try:
            if p.stat().st_size > MAX_FILE_BYTES:
                continue
            txt = p.read_text(encoding="utf-8", errors="replace")
        except Exception:
            continue
        rel = str(p).replace(str(ROOT) + os.sep, "")
        for c in _chunk(txt, rel):
            key = (rel, c[:80])
            if key in seen:
                continue
            seen.add(key)
            chunks.append((rel, c))
    print("indexing %d chunks from the corpus..." % len(chunks), file=sys.stderr)

    vecs, meta = [], []
    t0 = time.time()
    for n, (src, c) in enumerate(chunks):
        try:
            e = _embed("search_document: " + c)
        except Exception as ex:
            print("  embed failed on %s: %s" % (src, ex), file=sys.stderr)
            continue
        if not e:
            continue
        vecs.append(e)
        meta.append({"source": src, "text": c})
        if (n + 1) % 200 == 0:
            print("  %d/%d (%.0fs)" % (n + 1, len(chunks), time.time() - t0), file=sys.stderr)

    arr = np.asarray(vecs, dtype=np.float32)
    arr /= (np.linalg.norm(arr, axis=1, keepdims=True) + 1e-8)   # pre-normalise for cosine
    np.save(VEC_PATH, arr)
    with META_PATH.open("w", encoding="utf-8") as f:
        for m in meta:
            f.write(json.dumps(m) + "\n")
    print("indexed %d chunks -> %s (%.0fs)" % (len(meta), VEC_PATH.name, time.time() - t0), file=sys.stderr)
    return len(meta)


_CACHE = {"vecs": None, "meta": None}


def _load():
    if _CACHE["vecs"] is None:
        if not VEC_PATH.exists():
            raise RuntimeError("no RAG index - run: python tools/rag.py index")
        _CACHE["vecs"] = np.load(VEC_PATH)
        _CACHE["meta"] = [json.loads(l) for l in META_PATH.read_text(encoding="utf-8").splitlines() if l.strip()]
    return _CACHE["vecs"], _CACHE["meta"]


def retrieve(query: str, k: int = 5):
    vecs, meta = _load()
    q = np.asarray(_embed("search_query: " + query), dtype=np.float32)
    if q.size == 0:
        return []
    q /= (np.linalg.norm(q) + 1e-8)
    sims = vecs @ q
    idx = np.argsort(-sims)[:k]
    return [{"source": meta[i]["source"], "score": float(sims[i]), "text": meta[i]["text"]} for i in idx]


def append_pages(pages, source_prefix="web:"):
    """Chunk + embed crawled pages and APPEND them to the index PERMANENTLY (dedup'd). Updates disk
    AND the in-memory cache, so a long-running agent can learn a site then immediately recall it."""
    try:
        vecs, meta = _load()
    except Exception:
        vecs, meta = np.zeros((0, 768), dtype=np.float32), []
    keys = {(m["source"], m["text"][:80]) for m in meta}
    new_vecs, new_meta = [], []
    for url, title, text in pages:
        src = source_prefix + url
        for c in _chunk(text, src):
            key = (src, c[:80])
            if key in keys:
                continue
            keys.add(key)
            try:
                e = _embed("search_document: " + c)
            except Exception:
                continue
            if not e:
                continue
            v = np.asarray(e, dtype=np.float32)
            v /= (np.linalg.norm(v) + 1e-8)
            new_vecs.append(v)
            new_meta.append({"source": src, "text": c})
    if not new_vecs:
        return 0
    stacked = np.asarray(new_vecs, dtype=np.float32)
    allv = np.vstack([vecs, stacked]) if len(vecs) else stacked
    IDX_DIR.mkdir(exist_ok=True)
    np.save(VEC_PATH, allv)
    with META_PATH.open("a", encoding="utf-8") as f:      # append keeps order aligned with allv
        for m in new_meta:
            f.write(json.dumps(m) + "\n")
    _CACHE["vecs"], _CACHE["meta"] = allv, meta + new_meta
    return len(new_meta)


def ingest_url(url, max_pages=15, max_depth=2):
    """Crawl a whole site and add all of it to the permanent knowledge index."""
    import webcrawl
    pages = webcrawl.crawl(url, max_pages, max_depth)
    return append_pages(pages)


def format_hits(hits) -> str:
    if not hits:
        return "(nothing relevant found in the codebase/notes)"
    out = []
    for h in hits:
        out.append("--- %s  (relevance %.2f)\n%s" % (h["source"], h["score"], h["text"]))
    return "\n\n".join(out)


def main():
    if len(sys.argv) < 2:
        print("usage: rag.py index | search <query> [k] | stats"); return 2
    cmd = sys.argv[1]
    if cmd == "index":
        build_index()
    elif cmd == "search":
        if len(sys.argv) < 3:
            print("usage: rag.py search <query> [k]"); return 2
        k = int(sys.argv[-1]) if sys.argv[-1].isdigit() else 5
        q = " ".join(sys.argv[2:2 + (len(sys.argv) - 2 - (1 if sys.argv[-1].isdigit() else 0))])
        print(format_hits(retrieve(q, k)))
    elif cmd == "ingest":
        if len(sys.argv) < 3:
            print("usage: rag.py ingest <url> [max_pages] [max_depth]"); return 2
        mp = int(sys.argv[3]) if len(sys.argv) > 3 else 15
        md = int(sys.argv[4]) if len(sys.argv) > 4 else 2
        n = ingest_url(sys.argv[2], mp, md)
        print("ingested %d new chunks from %s into permanent knowledge" % (n, sys.argv[2]), file=sys.stderr)
    elif cmd == "stats":
        try:
            v, m = _load()
            web = sum(1 for x in m if str(x["source"]).startswith("web:"))
            print("index: %d chunks (%d from crawled web), %d-dim vectors, %.1f MB"
                  % (len(m), web, v.shape[1], VEC_PATH.stat().st_size / 1e6))
        except Exception as e:
            print("no index:", e)
    else:
        print("unknown command:", cmd); return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
