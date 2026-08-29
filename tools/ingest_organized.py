"""ingest_organized.py - ingest the opus+fable organised knowledge docs into the local store.

The organise-pc-knowledge workflow reads Jep's staged PC files and returns one clean markdown
knowledge doc per topic. Those docs are written to a folder; this ingests them under 'pcknow:'
(high-signal, preserved across refreshes) using the same non-destructive rag.ingest_dir path.

Usage:
    python tools/ingest_organized.py <organized_docs_dir>
"""
from __future__ import annotations
import os, sys, time
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))


def main() -> int:
    if len(sys.argv) < 2:
        print("usage: python tools/ingest_organized.py <organized_docs_dir>")
        return 2
    root = sys.argv[1]
    if not os.path.isdir(root):
        print("no such dir:", root)
        return 2
    import tools.rag as rag
    t0 = time.time()
    n = rag.ingest_dir(root, exts=(".md",), source_prefix="pcknow:", max_file=5_000_000)
    print(f"ingest_organized: +{n} chunks under 'pcknow:' in {time.time()-t0:.0f}s")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
