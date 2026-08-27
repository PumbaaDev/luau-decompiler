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

import json
import os
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

    ranked = sorted(found.items(), key=lambda kv: -kv[1])[:REPO_LIMIT]
    print("discover: %d unique repos, taking top %d by stars" % (len(found), len(ranked)), file=sys.stderr)
    return ranked


def _repo_size_kb(name):
    """Repo working-tree size in KB via the GitHub API (one cheap call). -1 if unknown."""
    try:
        out = subprocess.run(["gh", "api", "repos/%s" % name, "--jq", ".size"],
                             capture_output=True, text=True, timeout=30)
        return int((out.stdout or "-1").strip()) if out.returncode == 0 else -1
    except Exception:
        return -1


def clone(repos):
    CORPUS.mkdir(parents=True, exist_ok=True)
    cloned, skipped, failed, skipped_big = 0, 0, 0, 0
    for i, (name, stars) in enumerate(repos):
        dest = CORPUS / name.replace("/", "__")
        if dest.exists():
            skipped += 1
            continue
        size = _repo_size_kb(name)
        if size > MAX_DISK_KB:
            skipped_big += 1
            print("  [%d/%d] SKIP %s (%.0f MB > cap)" % (i + 1, len(repos), name, size / 1024),
                  file=sys.stderr)
            continue
        url = "https://github.com/%s" % name
        try:
            r = subprocess.run(["git", "clone", "--depth", "1", "--quiet", url, str(dest)],
                               capture_output=True, text=True, timeout=300)
            if r.returncode == 0:
                cloned += 1
                print("  [%d/%d] cloned %s (%d*)" % (i + 1, len(repos), name, stars), file=sys.stderr)
            else:
                failed += 1
                print("  [%d/%d] FAILED %s: %s" % (i + 1, len(repos), name,
                      (r.stderr or "").strip()[:120]), file=sys.stderr)
        except Exception as e:
            failed += 1
            print("  [%d/%d] FAILED %s: %s" % (i + 1, len(repos), name, e), file=sys.stderr)
    print("clone: %d new, %d already present, %d too big, %d failed"
          % (cloned, skipped, skipped_big, failed), file=sys.stderr)
    return cloned


def index():
    sys.path.insert(0, str(Path(__file__).resolve().parent))
    import rag
    lua = list(CORPUS.rglob("*.lua")) + list(CORPUS.rglob("*.luau"))
    print("index: %d .lua/.luau files on disk under %s" % (len(lua), CORPUS), file=sys.stderr)
    n = rag.ingest_dir(CORPUS, exts=(".lua", ".luau"), source_prefix="roblox:")
    return n


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
