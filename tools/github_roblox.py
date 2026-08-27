#!/usr/bin/env python3
"""github_roblox - gather the open-source Roblox/Luau code on GitHub and index it into the local
AI's permanent knowledge, so the model can search/recall real Roblox source on demand.

Three stages:
  1. DISCOVER  - `gh search repos` across several Roblox/Luau queries (by stars) + a curated
                 must-have list (the language, Roact/Rodux/Fusion, Nevermore/Knit, promises,
                 style guides, popular libs). Dedup, drop asset-heavy giants by disk size.
  2. CLONE     - shallow `git clone --depth 1` into a corpus dir on E: (source only, no history).
  3. INDEX     - rag.ingest_dir() walks every .lua/.luau file. It applies a SOURCE-vs-COMPILED
                 guard (rag.looks_like_source): NUL bytes / non-UTF-8 / control chars = compiled
                 bytecode or a binary place file, and is rejected. Only real source is embedded.

    python tools/github_roblox.py discover          # just list what would be cloned
    python tools/github_roblox.py run               # discover + clone + index (the whole job)
    python tools/github_roblox.py index             # re-index whatever is already cloned

Env: ROBLOX_CORPUS (default E:\\roblox_corpus), ROBLOX_REPO_LIMIT (default 150),
     ROBLOX_MAX_DISK_KB (default 800000 = ~800 MB, skip bigger repos).
"""
from __future__ import annotations

import concurrent.futures as cf
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

CORPUS = Path(os.environ.get("ROBLOX_CORPUS", r"E:\roblox_corpus"))
REPO_LIMIT = int(os.environ.get("ROBLOX_REPO_LIMIT", "150"))
MAX_DISK_KB = int(os.environ.get("ROBLOX_MAX_DISK_KB", "800000"))   # skip repos bigger than ~800 MB
PER_QUERY = 60

# star-sorted searches - each returns the top repos for that angle
QUERIES = [
    ("roblox", "Lua"), ("roblox", "Luau"), ("luau", "Luau"), ("luau", "Lua"),
    ("roblox-lua", "Lua"), ("roblox framework", "Lua"), ("roblox library", "Lua"),
    ("roblox game", "Lua"), ("rojo roblox", "Lua"), ("roblox studio", "Lua"),
]
# curated must-haves - captured even if a search misses them (the ecosystem your decompiler references)
CURATED = [
    "Roblox/luau", "Roblox/roact", "Roblox/rodux", "Roblox/roact-rodux", "Roblox/Core-Scripts",
    "Roblox/roblox-dev-education", "Roblox/testez", "Roblox/lua-promise",
    "Quenty/NevermoreEngine", "Sleitnick/Knit", "Sleitnick/RbxUtil", "Sleitnick/rbxts-knit",
    "evaera/roblox-lua-promise", "evaera/Cmdr", "howmanysmall/Janitor",
    "Kampfkarren/Roblox", "Kampfkarren/selene", "osyrisrblx/t", "osyrisrblx/rbxts-object-to-tree",
    "rojo-rbx/rojo", "rojo-rbx/rbxlx-to-rojo", "rojo-rbx/remodel", "UpliftGames/wally",
    "1ForeverHD/TopbarPlus", "1ForeverHD/Nanoblox", "Elttob/Fusion", "matter-ecs/matter",
    "roblox-ts/roblox-ts", "red-blox/Signal", "sleitnick/RbxUtil", "richie0866/rbxts-transformer-fusion",
    "grilme99/RobloxSyncStudioPlugin", "NightrainsRbx/RobloxLsp", "JohnnyMorganz/StyLua",
    "JohnnyMorganz/luau-lsp", "Roblox/creator-docs", "digitalzfoundry/roblox-ts-flipper",
    "LPGhatguy/vanilla", "csqrl/pretty-react-hooks",
]


def _gh_json(args, timeout=90):
    try:
        out = subprocess.run(["gh", *args], capture_output=True, text=True, timeout=timeout)
        if out.returncode != 0:
            print("  gh error: %s" % (out.stderr or "").strip()[:200], file=sys.stderr)
            return []
        return json.loads(out.stdout or "[]")
    except Exception as e:
        print("  gh call failed: %s" % e, file=sys.stderr)
        return []


def discover():
    """Return [(fullName, stars)] deduped, star-sorted, limited. (Disk size isn't available from
    `gh search repos`, so the size cap is applied per-repo at clone time.)"""
    found = {}   # fullName -> stars
    for terms, lang in QUERIES:
        rows = _gh_json(["search", "repos", *terms.split(), "--language", lang,
                         "--sort", "stars", "--limit", str(PER_QUERY),
                         "--json", "fullName,stargazersCount"])
        for r in rows:
            found[r["fullName"]] = r.get("stargazersCount", 0)
        print("  query %-22s +%d repos (total %d)" % ("'%s/%s'" % (terms, lang), len(rows), len(found)),
              file=sys.stderr)
    for name in CURATED:
        found.setdefault(name, 0)               # keep even if a search missed it

    # Curated must-haves are ALWAYS kept (they carry 0 stars, so a plain star-sort + truncate would
    # drop the very ecosystem repos we care about). Reserve their slots, fill the rest by stars.
    cur = set(CURATED)
    curated = [(n, found[n]) for n in CURATED if n in found]
    searched = sorted(((n, s) for n, s in found.items() if n not in cur), key=lambda kv: -kv[1])
    ranked = curated + searched[:max(0, REPO_LIMIT - len(curated))]
    print("discover: %d unique repos, taking %d (%d curated + %d top-starred)"
          % (len(found), len(ranked), len(curated), len(ranked) - len(curated)), file=sys.stderr)
    return ranked


def _repo_size_kb(name):
    """Repo working-tree size in KB via the GitHub API (one cheap call). -1 if unknown."""
    try:
        out = subprocess.run(["gh", "api", "repos/%s" % name, "--jq", ".size"],
                             capture_output=True, text=True, timeout=30)
        return int((out.stdout or "-1").strip()) if out.returncode == 0 else -1
    except Exception:
        return -1


def _clone_one(name, stars):
    """Clone one repo. Returns ('clone'|'skip'|'big'|'fail', name). Network-bound, so callers run
    several of these concurrently."""
    dest = CORPUS / name.replace("/", "__")
    if dest.exists():
        if (dest / ".git").exists():            # a complete clone -> skip
            return ("skip", name)
        shutil.rmtree(dest, ignore_errors=True)   # a partial/failed clone -> discard and retry
    if _repo_size_kb(name) > MAX_DISK_KB:
        print("  SKIP %s (> %d MB cap)" % (name, MAX_DISK_KB // 1024), file=sys.stderr)
        return ("big", name)
    url = "https://github.com/%s" % name
    try:
        # core.longpaths=true so deep repos don't fail checkout with 'Filename too long' on Windows
        r = subprocess.run(["git", "-c", "core.longpaths=true", "clone", "--depth", "1", "--quiet",
                            url, str(dest)], capture_output=True, text=True, timeout=300)
        if r.returncode == 0:
            print("  cloned %s (%d*)" % (name, stars), file=sys.stderr)
            return ("clone", name)
        shutil.rmtree(dest, ignore_errors=True)   # don't leave a partial dir counted as 'present'
        print("  FAILED %s: %s" % (name, (r.stderr or "").strip()[:100]), file=sys.stderr)
        return ("fail", name)
    except Exception as e:
        shutil.rmtree(dest, ignore_errors=True)
        print("  FAILED %s: %s" % (name, e), file=sys.stderr)
        return ("fail", name)


def clone(repos):
    """Clone repos concurrently - the stage is network-bound, so a small pool is a 4-8x wall-clock
    win over the old strictly-sequential loop (and the per-repo size checks parallelise too)."""
    CORPUS.mkdir(parents=True, exist_ok=True)
    workers = int(os.environ.get("ROBLOX_CLONE_WORKERS", "8"))
    counts = {"clone": 0, "skip": 0, "big": 0, "fail": 0}
    with cf.ThreadPoolExecutor(max_workers=workers) as ex:
        for kind, _name in ex.map(lambda r: _clone_one(*r), repos):
            counts[kind] += 1
    print("clone: %d new, %d already present, %d too big, %d failed"
          % (counts["clone"], counts["skip"], counts["big"], counts["fail"]), file=sys.stderr)
    return counts["clone"]


def index():
    sys.path.insert(0, str(Path(__file__).resolve().parent))
    import rag
    # ingest_dir does its own walk + prints its own file count; no need for a second pre-count walk.
    return rag.ingest_dir(CORPUS, exts=(".lua", ".luau"), source_prefix="roblox:")


def main():
    cmd = sys.argv[1] if len(sys.argv) > 1 else "run"
    if cmd == "discover":
        for name, stars in discover():
            print("%6d*  %s" % (stars, name))
    elif cmd == "index":
        n = index()
        print("indexed %d new source chunks" % n, file=sys.stderr)
    elif cmd == "run":
        repos = discover()
        clone(repos)
        n = index()
        print("DONE: indexed %d new Roblox source chunks from %d repos into permanent knowledge"
              % (n, len(repos)), file=sys.stderr)
    else:
        print("usage: github_roblox.py discover | run | index"); return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
