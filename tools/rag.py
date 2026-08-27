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
LOCK_PATH = IDX_DIR / "rag_index.lock"     # guards against concurrent writers corrupting the store
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


def _embed_batch(texts, timeout: int = 180) -> list[list[float]]:
    """Embed many chunks in ONE call via Ollama's /api/embed (array input). ~15-28x faster than
    one HTTP round-trip per chunk: measured 39 embeds/s batched vs ~2/s single on the 5060 Ti.
    Applies the same 'search_document: ' prefix as the single path so vectors stay consistent."""
    body = json.dumps({"model": EMBED_MODEL, "input": ["search_document: " + t for t in texts]}).encode()
    req = urllib.request.Request(OLLAMA + "/api/embed", data=body,
                                 headers={"Content-Type": "application/json"}, method="POST")
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.load(r).get("embeddings", [])


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
    """Full rebuild from the corpus. Exclusive: holds the single-writer lock for the whole run so an
    ingest can't interleave with the from-scratch overwrite."""
    if not _acquire_lock():
        raise RuntimeError("another indexer holds %s - refusing to rebuild concurrently" % LOCK_PATH)
    try:
        return _build_index_locked()
    finally:
        _release_lock()


def _build_index_locked():
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

    if not vecs:                                   # nothing embedded (e.g. Ollama down) - don't wipe the store
        print("build_index: 0 chunks embedded, leaving existing store untouched", file=sys.stderr)
        return 0
    arr = np.asarray(vecs, dtype=np.float32)
    arr /= (np.linalg.norm(arr, axis=1, keepdims=True) + 1e-8)   # pre-normalise for cosine
    _atomic_save_vecs(arr)
    tmp = META_PATH.parent / (META_PATH.name + ".tmp")           # atomic meta swap to match
    with tmp.open("w", encoding="utf-8") as f:
        for m in meta:
            f.write(json.dumps(m) + "\n")
    os.replace(tmp, META_PATH)
    _CACHE["vecs"], _CACHE["meta"] = arr, meta                   # refresh cache to the rebuilt store
    print("indexed %d chunks -> %s (%.0fs)" % (len(meta), VEC_PATH.name, time.time() - t0), file=sys.stderr)
    return len(meta)


_CACHE = {"vecs": None, "meta": None}


def _load(force=False):
    """Load the store into the cache. force=True re-reads DISK (a long-running process must do this
    before writing, or it rewrites from a stale snapshot and clobbers another writer's flushes).
    Defensively truncates to the common prefix so a crash-torn flush self-heals instead of poisoning
    retrieval with vec[i] not matching meta[i]."""
    if force or _CACHE["vecs"] is None:
        if not VEC_PATH.exists():
            raise RuntimeError("no RAG index - run: python tools/rag.py index")
        vecs = np.load(VEC_PATH)
        meta = [json.loads(l) for l in META_PATH.read_text(encoding="utf-8").splitlines() if l.strip()]
        if len(vecs) != len(meta):                # crash-torn write: keep only the aligned prefix
            n = min(len(vecs), len(meta))
            print("rag: store length mismatch (vecs=%d meta=%d), aligning to %d" % (len(vecs), len(meta), n),
                  file=sys.stderr)
            vecs, meta = vecs[:n], meta[:n]
        _CACHE["vecs"], _CACHE["meta"] = vecs, meta
    return _CACHE["vecs"], _CACHE["meta"]


def _atomic_save_vecs(arr):
    """Write the vectors file atomically (temp + os.replace) so a crash never leaves a half-written
    .npy that fails to load. np.save to an open handle avoids its auto-.npy-suffix rewrite."""
    IDX_DIR.mkdir(exist_ok=True)
    tmp = VEC_PATH.parent / (VEC_PATH.name + ".tmp")
    with open(tmp, "wb") as f:
        np.save(f, arr)
    os.replace(tmp, VEC_PATH)


def _write_load():
    """Fresh disk snapshot for a WRITER (called under the lock). If the store files exist but fail to
    load, ABORT rather than returning an empty base - overwriting with empty would destroy the store
    (the #9 corruption). Only an absent store yields the empty base of a first run."""
    try:
        return _load(force=True)
    except RuntimeError:
        if VEC_PATH.exists():                     # exists but unreadable -> do NOT clobber it
            raise
        return np.zeros((0, 768), dtype=np.float32), []


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
    # Embed first WITHOUT the lock (embedding is the slow part and touches no shared file); dedup
    # against a snapshot just to avoid obvious re-embeds.
    try:
        _, meta0 = _load()
    except Exception:
        meta0 = []
    seen = {(m["source"], m["text"][:80]) for m in meta0}
    pending = []   # (src, chunk, vec)
    for url, title, text in pages:
        src = source_prefix + url
        for c in _chunk(text, src):
            key = (src, c[:80])
            if key in seen:
                continue
            seen.add(key)
            try:
                e = _embed("search_document: " + c)
            except Exception:
                continue
            if not e:
                continue
            v = np.asarray(e, dtype=np.float32)
            v /= (np.linalg.norm(v) + 1e-8)
            pending.append((src, c, v))
    if not pending:
        return 0
    # WRITE under the single-writer lock, off a FRESH disk snapshot (never a stale cache), atomically.
    if not _acquire_lock():
        raise RuntimeError("another indexer holds %s - refusing concurrent write" % LOCK_PATH)
    try:
        vecs, meta = _write_load()
        have = {(m["source"], m["text"][:80]) for m in meta}
        add_v, add_m = [], []
        for src, c, v in pending:
            k = (src, c[:80])
            if k in have:                          # another writer added it between embed and write
                continue
            have.add(k)
            add_v.append(v)
            add_m.append({"source": src, "text": c})
        if not add_m:
            return 0
        stacked = np.asarray(add_v, dtype=np.float32)
        allv = np.vstack([vecs, stacked]) if len(vecs) else stacked
        _atomic_save_vecs(allv)
        with META_PATH.open("a", encoding="utf-8") as f:   # append keeps order aligned with allv
            for m in add_m:
                f.write(json.dumps(m) + "\n")
        _CACHE["vecs"], _CACHE["meta"] = allv, meta + add_m
        return len(add_m)
    finally:
        _release_lock()


def ingest_url(url, max_pages=15, max_depth=2):
    """Crawl a whole site and add all of it to the permanent knowledge index."""
    import webcrawl
    pages = webcrawl.crawl(url, max_pages, max_depth)
    return append_pages(pages)


def looks_like_source(path, sample=16384):
    """True only for real human-readable SOURCE text; rejects COMPILED/binary content.

    Roblox repos ship compiled artifacts too - binary .rbxl/.rbxm place files, and occasionally
    Luau BYTECODE (string.dump / luau-compile output) saved with a .lua extension. Bytecode is
    binary and useless to a code-knowledge index. Discriminator: source is clean UTF-8 text with
    no NUL bytes and almost no control chars; compiled/bytecode has NULs and fails UTF-8."""
    try:
        with open(path, "rb") as f:
            raw = f.read(sample)
    except Exception:
        return False
    if not raw:
        return False
    if b"\x00" in raw:                       # a NUL byte never appears in Lua source; = binary/bytecode
        return False
    try:
        text = raw.decode("utf-8")           # source is valid UTF-8; bytecode is not
    except UnicodeDecodeError:
        return False
    ctrl = sum(1 for c in text if ord(c) < 0x20 and c not in "\t\n\r\f")
    if ctrl > len(text) * 0.02:              # >2% control chars = binary, not source
        return False
    return True


def _pid_alive(pid):
    try:
        out = subprocess.run(["tasklist", "/FI", "PID eq %d" % pid, "/FO", "CSV", "/NH"],
                             capture_output=True, text=True, timeout=10)
        return ("python" in out.stdout.lower())
    except Exception:
        return True   # can't tell -> assume alive (safer: don't steal a live lock)


def _acquire_lock():
    """Exclusive single-writer lock. Two concurrent ingest runs would overwrite each other's
    vectors (the .npy is rewritten whole on each flush) and misalign it from the .jsonl - which is
    exactly the corruption seen on 27 Aug. Steal only a lock whose owning PID is gone (stale)."""
    IDX_DIR.mkdir(exist_ok=True)
    for _ in range(2):
        try:
            fd = os.open(str(LOCK_PATH), os.O_CREAT | os.O_EXCL | os.O_WRONLY)
            os.write(fd, str(os.getpid()).encode())
            os.close(fd)
            return True
        except FileExistsError:
            try:
                owner = int(LOCK_PATH.read_text().strip() or "0")
            except Exception:
                owner = 0
            if owner and owner != os.getpid() and _pid_alive(owner):
                return False                      # a live indexer holds it - refuse
            try:
                LOCK_PATH.unlink()                # stale (dead owner) - steal and retry
            except OSError:
                return False
    return False


def _release_lock():
    try:
        if LOCK_PATH.exists() and LOCK_PATH.read_text().strip() == str(os.getpid()):
            LOCK_PATH.unlink()
    except OSError:
        pass


import subprocess  # noqa: E402  (used by _pid_alive)


def _ensure_loaded():
    """Fresh disk snapshot for a writer (ingest holds the lock when this runs). Never fabricates an
    empty base over an existing-but-unreadable store - _write_load raises so the run aborts instead
    of clobbering the store on the next flush."""
    _write_load()


def _flush(buf_v, buf_m):
    """Append a batch to disk + live cache (keeps order aligned so retrieve stays correct)."""
    if not buf_v:
        return
    IDX_DIR.mkdir(exist_ok=True)
    stacked = np.asarray(buf_v, dtype=np.float32)
    allv = np.vstack([_CACHE["vecs"], stacked]) if len(_CACHE["vecs"]) else stacked
    _atomic_save_vecs(allv)
    with META_PATH.open("a", encoding="utf-8") as f:
        for m in buf_m:
            f.write(json.dumps(m) + "\n")
    _CACHE["vecs"], _CACHE["meta"] = allv, _CACHE["meta"] + buf_m


def ingest_dir(root, exts=(".lua", ".luau"), source_prefix="", flush_every=800, max_file=300_000):
    """Walk a directory of CODE files and index them into permanent knowledge, saving incrementally
    (a crash mid-run keeps everything already flushed). Dedup'd against what's already indexed.
    Holds a single-writer lock for the whole run so a second indexer can't corrupt the store."""
    if not _acquire_lock():
        raise RuntimeError("another indexer holds %s - refusing to run concurrently (a second writer "
                           "corrupts the vector store). If it is stale, delete that file." % LOCK_PATH)
    try:
        return _ingest_dir_locked(root, exts, source_prefix, flush_every, max_file)
    finally:
        _release_lock()


def _ingest_dir_locked(root, exts, source_prefix, flush_every, max_file, batch=64):
    root = Path(root)
    _ensure_loaded()
    keys = {(m["source"], m["text"][:80]) for m in _CACHE["meta"]}
    files = [p for p in root.rglob("*") if p.is_file() and p.suffix.lower() in exts
             and not (SKIP_DIRS & set(p.parts))]
    print("ingest_dir: %d %s files under %s" % (len(files), "/".join(exts), root), file=sys.stderr)

    buf_v, buf_m = [], []                     # embedded, awaiting disk flush
    pend_src, pend_txt = [], []               # chunks awaiting a batch embed call
    state = {"added": 0, "skipped_compiled": 0, "fi": 0}
    t0 = time.time()

    def do_embed():                           # embed the pending batch in one HTTP call
        if not pend_txt:
            return
        try:
            embs = _embed_batch(pend_txt)
        except Exception as ex:
            print("  batch embed failed (%s), skipping %d chunks" % (ex, len(pend_txt)), file=sys.stderr)
            embs = []
        for c, src, e in zip(pend_txt, pend_src, embs):
            if not e:
                continue
            v = np.asarray(e, dtype=np.float32)
            v /= (np.linalg.norm(v) + 1e-8)
            buf_v.append(v)
            buf_m.append({"source": src, "text": c})
        pend_src.clear()
        pend_txt.clear()

    def maybe_flush():
        if len(buf_v) >= flush_every:
            _flush(buf_v, buf_m)
            state["added"] += len(buf_m)
            print("  indexed %d chunks (%d/%d files, %.0fs, %.1f/s)"
                  % (state["added"], state["fi"], len(files), time.time() - t0,
                     state["added"] / max(time.time() - t0, 1)), file=sys.stderr)
            buf_v.clear()
            buf_m.clear()

    for n, p in enumerate(files):
        state["fi"] = n + 1
        try:
            if p.stat().st_size > max_file:
                continue
            if not looks_like_source(p):      # reject compiled/bytecode masquerading as .lua
                state["skipped_compiled"] += 1
                continue
            txt = p.read_text(encoding="utf-8", errors="replace")
        except Exception:
            continue
        src = source_prefix + str(p.relative_to(root)).replace("\\", "/")
        for c in _chunk(txt, src):
            key = (src, c[:80])
            if key in keys:
                continue
            keys.add(key)
            pend_src.append(src)
            pend_txt.append(c)
            if len(pend_txt) >= batch:
                do_embed()
                maybe_flush()
    do_embed()                                # final partial batch
    _flush(buf_v, buf_m)
    state["added"] += len(buf_m)
    print("ingest_dir done: +%d chunks from source, %d compiled/binary files skipped (%.0fs, %.1f/s)"
          % (state["added"], state["skipped_compiled"], time.time() - t0,
             state["added"] / max(time.time() - t0, 1)), file=sys.stderr)
    return state["added"]


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
    elif cmd == "ingest-dir":
        if len(sys.argv) < 3:
            print("usage: rag.py ingest-dir <dir> [source_prefix]"); return 2
        prefix = sys.argv[3] if len(sys.argv) > 3 else ""
        n = ingest_dir(sys.argv[2], source_prefix=prefix)
        print("ingested %d new source chunks from %s into permanent knowledge" % (n, sys.argv[2]), file=sys.stderr)
    elif cmd == "stats":
        try:
            v, m = _load()
            web = sum(1 for x in m if str(x["source"]).startswith("web:"))
            rbx = sum(1 for x in m if str(x["source"]).startswith("roblox:"))
            print("index: %d chunks (%d web, %d roblox), %d-dim vectors, %.1f MB"
                  % (len(m), web, rbx, v.shape[1], VEC_PATH.stat().st_size / 1e6))
        except Exception as e:
            print("no index:", e)
    else:
        print("unknown command:", cmd); return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
