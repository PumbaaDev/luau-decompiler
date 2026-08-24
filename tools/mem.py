#!/usr/bin/env python3
"""local_mem - a curated, Obsidian-native memory for the local model.

Built to fix the one measured failure of uncensored-coder: it confabulates facts
it does not have (invented a full spec for a non-existent Roblox API even when the
system prompt told it to say "I do not know"). Retrieval alone does not fix that,
because the model ignores "if the notes do not cover it, say so".

The mechanism here is ENTITY GROUNDING. When a question names an entity (an API
method, a model name, a flag, a number), the store checks whether any fact defines
it. If none does, it injects an explicit line - "NO FACT DEFINES: X" - so the model
has something concrete to ground a refusal on instead of being asked to volunteer
one. Measured to cut bluffing on the fake-API probe.

Design, vs the old bulk-chunk index.jsonl:
  * ONE ATOMIC FACT per markdown file under local_mem/facts/, Obsidian-openable,
    with frontmatter (id, type, authority, aliases, tags, updated, supersedes).
  * SUPERSESSION IS STRUCTURAL. A fact with `supersedes: [old-id]` removes old-id
    from retrieval entirely. No CORRECTIONS.md band-aid.
  * AUTHORITY-RANKED. measured > claimed > assumed, recency breaks ties.
  * SHORT facts, so more of them fit the ~14.4K usable window than 450-token chunks.

  python tools/mem.py build                      # (re)generate the index + INDEX.md
  python tools/mem.py add                         # create a fact interactively
  python tools/mem.py list [--type fact]          # list facts
  python tools/mem.py search "vram ceiling"       # show what retrieval returns
  python tools/mem.py ask "does GetDescendantChecksum exist?"
  python tools/mem.py supersede <old-id> <new-id>
"""
from __future__ import annotations

import argparse
import json
import math
import os
import re
import sys
import urllib.request
from collections import Counter
from pathlib import Path

MEM_DIR = Path(__file__).parent / "local_mem"
FACTS = MEM_DIR / "facts"
INDEX_JSON = MEM_DIR / "_index.json"
INDEX_MD = MEM_DIR / "INDEX.md"
URL = "http://localhost:11434/api/generate"
MODEL = "uncensored-coder"

AUTH_WEIGHT = {"measured": 3.0, "claimed": 1.5, "assumed": 1.0}
STOP = {"the", "a", "an", "and", "or", "of", "to", "in", "is", "it", "that", "this",
        "for", "on", "with", "as", "by", "be", "are", "was", "how", "what", "why",
        "when", "do", "does", "did", "i", "you", "we", "my", "our", "if", "so"}


# --------------------------------------------------------------------------- #
# frontmatter (no pyyaml dependency - a tiny parser for the subset used here)
# --------------------------------------------------------------------------- #
_LIST_KEYS = {"aliases", "tags", "supersedes", "links"}


def parse_fact(text: str) -> dict:
    fm, body = {}, text
    m = re.match(r"^---\s*\n(.*?)\n---\s*\n?(.*)$", text, re.S)
    if m:
        body = m.group(2).strip()
        for line in m.group(1).splitlines():
            if ":" not in line:
                continue
            k, v = line.split(":", 1)
            k, v = k.strip(), v.strip()
            # Only the KNOWN list fields become Python lists. A scalar wrapped in brackets
            # (Obsidian `title: [[Wikilink]]`, `title: [DRAFT]`) must stay a str: turning it
            # into a list made retrieve()/alias_set crash, and one bad vault note then
            # silently disabled ALL memory injection (the exception is swallowed per request).
            if k in _LIST_KEYS and v.startswith("[") and v.endswith("]"):
                # split on TOP-LEVEL commas only; a quoted item may itself contain a comma
                items = re.findall(r"'[^']*'|\"[^\"]*\"|[^,]+", v[1:-1])
                v = [x.strip().strip("'\"") for x in items if x.strip()]
            fm[k] = v
    fm["body"] = body
    return fm


def _fmt_list(v) -> str:
    # quote any item containing a comma/bracket so parse_fact's comma-split round-trips it intact
    return "[%s]" % ", ".join(("'%s'" % x if any(c in x for c in ",[]") else x) for x in map(str, v))


def dump_fact(fm: dict) -> str:
    order = ["id", "title", "type", "authority", "aliases", "tags",
             "updated", "supersedes", "links"]
    lines = ["---"]
    for k in order:
        v = fm.get(k)
        if v is None:
            continue
        lines.append("%s: %s" % (k, _fmt_list(v) if isinstance(v, list) else v))
    for k, v in fm.items():                      # preserve any custom frontmatter key rather than dropping it
        if k in order or k == "body" or k.startswith("_") or v is None:
            continue
        lines.append("%s: %s" % (k, _fmt_list(v) if isinstance(v, list) else v))
    lines.append("---")
    return "\n".join(lines) + "\n\n" + fm.get("body", "").strip() + "\n"


# --------------------------------------------------------------------------- #
# store
# --------------------------------------------------------------------------- #
def load_facts() -> list[dict]:
    if not FACTS.exists():
        return []
    out = []
    for p in sorted(FACTS.glob("*.md")):
        fm = parse_fact(p.read_text(encoding="utf-8"))
        fm.setdefault("id", p.stem)
        fm["_path"] = str(p)
        for key in ("aliases", "tags", "supersedes", "links"):
            if isinstance(fm.get(key), str):
                fm[key] = [fm[key]] if fm[key] else []
            fm.setdefault(key, [])
        out.append(fm)
    return out


# --------------------------------------------------------------------------- #
# the full Obsidian vault as an ADDITIONAL retrieval source. Vault notes are long,
# so they are chunked (~700 chars) into retrieval units and tagged authority
# "assumed" (weight 1.0), so the curated measured facts (weight 3.0) always outrank
# them - the vault fills gaps, it does not overrule the atomic store.
# --------------------------------------------------------------------------- #
VAULT_DIR = Path(os.environ.get("CLAUDE_VAULT", r"C:\Users\jep\Documents\ClaudeMemory"))
VAULT_CHUNK = 700


def _chunks(text: str, size: int = VAULT_CHUNK) -> list[str]:
    """Group paragraphs into chunks of up to ~size chars, so a retrieved vault unit is
    bounded. A SINGLE paragraph longer than `size` (a blank-line-free code block or table)
    is HARD-SPLIT first - without this it became one arbitrarily large chunk (measured:
    19,603 / 16,247 / 8,178 chars in the live vault) that sailed past the injection cap and
    re-inflated the model's window, the exact bug the cap exists to prevent."""
    size = max(2, size)                    # a 0/negative size would spin the while-loop forever
    raw = [p.strip() for p in re.split(r"\n\s*\n", text or "") if p.strip()]
    paras = []
    for p in raw:
        while len(p) > size:
            cut = p.rfind("\n", size // 2, size)   # prefer a line boundary in the back half
            if cut <= 0:
                cut = size                          # no line break: hard cut at size (>= size//2, so p shrinks)
            seg = p[:cut].strip()
            if seg:
                paras.append(seg)
            p = p[cut:].strip()
        if p:
            paras.append(p)
    out, cur = [], ""
    for p in paras:
        if cur and len(cur) + len(p) + 2 > size:
            out.append(cur)
            cur = p
        else:
            cur = (cur + "\n\n" + p) if cur else p
    if cur:
        out.append(cur)
    return out


def load_vault() -> list[dict]:
    """Chunked vault notes as fact-like dicts, compatible with retrieve()/live_facts()."""
    if not VAULT_DIR.exists():
        return []
    out = []
    for p in sorted(VAULT_DIR.glob("*.md")):
        try:
            fm = parse_fact(p.read_text(encoding="utf-8", errors="replace"))
        except Exception:
            continue
        title = fm.get("title") or p.stem
        for i, ch in enumerate(_chunks(fm.get("body", ""))):
            out.append({"id": "vault:%s#%d" % (p.stem, i), "title": title,
                        "type": "vault", "authority": "assumed", "aliases": [],
                        "tags": ["vault"], "updated": fm.get("updated", ""),
                        "supersedes": [], "links": [], "body": ch, "_path": str(p)})
    return out


def live_facts(facts: list[dict]) -> list[dict]:
    """Drop any fact that a newer fact supersedes."""
    dead = set()
    for f in facts:
        for old in f.get("supersedes", []):
            dead.add(old)
    return [f for f in facts if f["id"] not in dead], dead


def tok(s: str) -> list[str]:
    return [w for w in re.findall(r"[a-z0-9_]+", (s or "").lower())
            if w not in STOP and len(w) > 2]


# entities: API-ish names (CamelCase, dotted, v_digits), quoted terms, and numbers
_ENTITY = re.compile(
    r"`[^`]+`"                              # `backticked`
    r"|\"[^\"\n]{1,80}\""                  # "double-quoted term" (bounded, no newline)
    r"|(?<!\w)'[^'\n]{1,60}'(?!\w)"        # 'single-quoted term' - NOT an apostrophe inside a word
    r"|\b[A-Za-z_]\w*(?:[.:]\w+)+\b"       # dotted / colon method  a.b  A:b   (what's/Jep's no longer
    r"|\b[A-Z][a-z]+(?:[A-Z][a-z]+)+\w*\b"  # CamelCase                         span a fake quoted entity)
    r"|\bv\d+\b"                            # register names
    r"|\b\d+(?:\.\d+)?\b"                  # numbers
)


def entities(s: str) -> list[str]:
    raw = _ENTITY.findall(s or "")
    out = []
    for e in raw:
        e = e.strip("`\"'")
        if e and e.lower() not in STOP:
            out.append(e)
    return out


def alias_set(f: dict) -> set[str]:
    names = [f.get("id", ""), f.get("title", "")] + f.get("aliases", [])
    s = set()
    for n in names:
        if not isinstance(n, str):                      # defensive: a stray list title must not crash
            n = " ".join(map(str, n)) if isinstance(n, list) else str(n)
        s.add(n.lower())
        for e in entities(n):
            s.add(e.lower())
    return s


# Per-unit caches, computed once and stored on the dict. retrieve() previously re-tokenized
# every unit's full body TWICE and ran the entity regex per unit on EVERY call (~144ms over the
# live 2600-unit corpus); these make repeat retrievals single-digit ms. Facts appended live
# (session findings) get computed lazily on first use.
def _unit_toks(f: dict) -> set:
    t = f.get("_toks")
    if t is None:
        t = set(tok(f.get("title", "") + " " + f.get("body", "") + " " + " ".join(f.get("aliases", []))))
        f["_toks"] = t
    return t


def _unit_alias(f: dict) -> set:
    a = f.get("_alias")
    if a is None:
        a = alias_set(f)
        f["_alias"] = a
    return a


def _unit_ents(f: dict) -> set:
    """What this fact 'covers' for the anti-bluff check: id/title/alias entities PLUS the
    entities named in its BODY, so a term a fact's body defines is never wrongly flagged
    'NO FACT DEFINES' in the same prompt that supplies the defining fact."""
    e = f.get("_ents")
    if e is None:
        e = set(_unit_alias(f)) | {x.lower() for x in entities(f.get("body", ""))}
        f["_ents"] = e
    return e


# --------------------------------------------------------------------------- #
# retrieval: entity-exact boost + lexical IDF + authority + recency
# --------------------------------------------------------------------------- #
def retrieve(facts: list[dict], query: str, k: int = 6):
    live, _ = live_facts(facts)
    if not live:
        return []
    q_ents = {e.lower() for e in entities(query)}
    q_terms = Counter(tok(query))

    df = Counter()
    for f in live:
        seen = _unit_toks(f)                            # cached; computed once per unit
        for w in q_terms:
            if w in seen:
                df[w] += 1
    n = len(live)

    scored = []
    for f in live:
        terms = _unit_toks(f)
        lex = sum(qc * math.log(1 + n / (1 + df[w])) for w, qc in q_terms.items() if w in terms)
        ent_hit = len(q_ents & _unit_alias(f))
        if lex == 0 and ent_hit == 0:
            continue
        score = lex + 8.0 * ent_hit          # a named-entity match dominates
        score *= AUTH_WEIGHT.get(f.get("authority", "assumed"), 1.0)
        score += _recency(f.get("updated", "")) * 0.3
        scored.append((score, f))
    scored.sort(key=lambda x: -x[0])
    return scored[:k]


def _recency(datestr: str) -> float:
    m = re.match(r"(\d{4})-(\d{2})-(\d{2})", datestr or "")
    if not m:
        return 0.0
    y, mo, d = (int(x) for x in m.groups())
    return (y - 2026) * 12 + mo + d / 31.0        # small monotonic nudge


def unmet_entities(facts: list[dict], query: str) -> list[str]:
    """Named entities in the query that NO live fact defines. The anti-bluff key."""
    live, _ = live_facts(facts)
    covered = set()
    for f in live:
        covered |= _unit_ents(f)                        # id/title/alias entities AND body-defined entities
    out = []
    for e in entities(query):
        # numbers alone are not "entities to define"
        if re.fullmatch(r"\d+(?:\.\d+)?", e):
            continue
        if e.lower() not in covered:
            out.append(e)
    # de-dup, keep order
    seen, uniq = set(), []
    for e in out:
        if e.lower() not in seen:
            seen.add(e.lower())
            uniq.append(e)
    return uniq


# --------------------------------------------------------------------------- #
# ask - the anti-confabulation protocol
# --------------------------------------------------------------------------- #
def build_prompt(hits, unmet, q: str) -> str:
    facts_block = "\n\n".join(
        "[fact %s | %s]\n%s" % (f["id"], f.get("authority", "assumed"), f.get("body", ""))
        for _, f in hits) or "(no facts matched this query)"
    absent = ""
    if unmet:
        absent = ("\n=== NAMED IN THE QUESTION BUT NO FACT DEFINES THEM ===\n"
                  + "\n".join("- %s : NO FACT. You do not know this. Do not describe it." % e
                              for e in unmet)
                  + "\n=== END ===\n")
    return (
        "You answer ONLY from the FACTS below. They are this user's own measured "
        "records and outrank anything you think you know.\n"
        "RULES:\n"
        "1. Every claim you make must trace to a fact below. Do not add outside "
        "knowledge, even if you are confident.\n"
        "2. If the facts do not answer the question, reply EXACTLY:\n"
        "   NOT IN MEMORY: <one line naming what is missing>\n"
        "3. If an item is listed under 'NO FACT DEFINES THEM', it does not exist in "
        "this user's world as far as you know. Say you have no record of it. NEVER "
        "invent its behaviour, return type, or parameters.\n\n"
        "=== FACTS ===\n" + facts_block + "\n=== END FACTS ===\n"
        + absent +
        "\nQuestion: " + q + "\n\nAnswer:")


def call_model(prompt: str, model: str) -> str:
    # Let each Modelfile govern num_ctx (coder 32768 / thinker 16384) and temperature (coder 0
    # for determinism / thinker 0.6 - greedy makes Qwen3-Thinking loop endlessly). Overriding
    # them here forced the thinker to temp 0 and halved the coder's window. Only num_predict is
    # set, and NOT lowered: 1500 truncated the coder's mandated TRACE/EDGE/CODE structure and
    # starved the thinker's <think> chain, both reading as model failure. NO `think` field -
    # qwen3-coder crashes llama-server (0xc0000409) if it is sent at all.
    body = json.dumps({"model": model, "prompt": prompt, "stream": False,
                       "options": {"num_predict": 8000}}).encode()
    req = urllib.request.Request(URL, data=body, headers={"content-type": "application/json"})
    return json.load(urllib.request.urlopen(req, timeout=600)).get("response", "").strip()


# --------------------------------------------------------------------------- #
# management
# --------------------------------------------------------------------------- #
def build_index() -> int:
    facts = load_facts()
    live, dead = live_facts(facts)
    INDEX_JSON.write_text(json.dumps(
        [{k: f.get(k) for k in ("id", "title", "type", "authority", "aliases",
                                "tags", "updated", "supersedes")} for f in live],
        indent=2), encoding="utf-8")
    lines = ["# local_mem index", "",
             "%d live facts (%d superseded, hidden from retrieval)" % (len(live), len(dead)), ""]
    for f in sorted(live, key=lambda x: (x.get("type", ""), x["id"])):
        lines.append("- **%s** [%s/%s] — %s"
                     % (f.get("title", f["id"]), f.get("type", "fact"),
                        f.get("authority", "assumed"), f.get("body", "")[:90].replace("\n", " ")))
    INDEX_MD.write_text("\n".join(lines) + "\n", encoding="utf-8")
    print("indexed %d live facts (%d superseded) -> %s" % (len(live), len(dead), INDEX_MD))
    return 0


def cmd_add(args) -> int:
    FACTS.mkdir(parents=True, exist_ok=True)
    def ask_in(p):
        sys.stdout.write(p); sys.stdout.flush()
        return sys.stdin.readline().strip()
    fid = args.id or ask_in("id (kebab-case): ")
    fm = {
        "id": fid,
        "title": args.title or ask_in("title: "),
        "type": args.type or (ask_in("type [fact/procedure/reference/correction]: ") or "fact"),
        "authority": args.authority or (ask_in("authority [measured/claimed/assumed]: ") or "assumed"),
        "aliases": [x.strip() for x in (args.aliases or ask_in("aliases (comma-sep): ")).split(",") if x.strip()],
        "tags": [x.strip() for x in (args.tags or ask_in("tags (comma-sep): ")).split(",") if x.strip()],
        "updated": args.updated or ask_in("updated (YYYY-MM-DD): "),
        "supersedes": [x.strip() for x in (args.supersedes or "").split(",") if x.strip()],
        "body": args.body or ask_in("fact (one atomic statement): "),
    }
    p = FACTS / (fid + ".md")
    p.write_text(dump_fact(fm), encoding="utf-8")
    print("wrote %s" % p)
    return build_index()


def cmd_supersede(args) -> int:
    if args.old == args.new:                          # self-supersession removes the fact entirely
        sys.exit("a fact cannot supersede itself: %s" % args.new)
    facts = {f["id"]: f for f in load_facts()}
    if args.new not in facts:
        sys.exit("no such fact: %s" % args.new)
    if args.old not in facts:
        sys.exit("no such fact: %s" % args.old)
    if args.new in facts[args.old].get("supersedes", []):   # mutual supersession hides BOTH facts
        sys.exit("cycle: %s already supersedes %s - refusing" % (args.old, args.new))
    fm = facts[args.new]
    sup = set(fm.get("supersedes", [])) | {args.old}
    fm["supersedes"] = sorted(sup)
    Path(fm["_path"]).write_text(dump_fact(fm), encoding="utf-8")
    print("%s now supersedes %s" % (args.new, args.old))
    return build_index()


def cmd_list(args) -> int:
    live, dead = live_facts(load_facts())
    for f in sorted(live, key=lambda x: x["id"]):
        if args.type and f.get("type") != args.type:
            continue
        print("  %-28s [%s/%s] %s" % (f["id"], f.get("type", "?"),
                                      f.get("authority", "?"), f.get("title", "")))
    print("\n%d live, %d superseded" % (len(live), len(dead)))
    return 0


def _corpus(args) -> list:
    """load_facts, plus load_vault when --vault, so the CLI debug view matches what the proxy
    actually retrieves over (facts + vault). Without it, search/ask show a different ranking."""
    facts = load_facts()
    if getattr(args, "vault", False):
        try:
            facts = facts + load_vault()
        except Exception as e:
            print("vault load failed: %s" % e, file=sys.stderr)
    return facts


def cmd_search(args) -> int:
    q = " ".join(args.query)
    facts = _corpus(args)
    hits = retrieve(facts, q, args.k)
    unmet = unmet_entities(facts, q)
    print("query entities: %s" % (entities(q) or "none"))
    print("unmet (no fact): %s\n" % (unmet or "none"))
    for s, f in hits:
        print("  %6.2f  [%s/%s] %s" % (s, f.get("type"), f.get("authority"), f["id"]))
    return 0


def cmd_ask(args) -> int:
    q = " ".join(args.question)
    facts = _corpus(args)
    hits = retrieve(facts, q, args.k)
    unmet = unmet_entities(facts, q)
    if args.show:
        print("retrieved: %s" % [f["id"] for _, f in hits], file=sys.stderr)
        print("unmet entities: %s\n" % unmet, file=sys.stderr)
    print(call_model(build_prompt(hits, unmet, q), args.model))
    return 0


def main() -> int:
    # Windows console defaults to cp1252 and dies on a stray arrow in a fact title.
    for stream in (sys.stdout, sys.stderr):
        try:
            stream.reconfigure(encoding="utf-8", errors="replace")
        except (AttributeError, ValueError):
            pass
    ap = argparse.ArgumentParser(prog="mem")
    sub = ap.add_subparsers(dest="cmd", required=True)

    sub.add_parser("build")

    a = sub.add_parser("add")
    for opt in ("id", "title", "type", "authority", "aliases", "tags", "updated",
                "supersedes", "body"):
        a.add_argument("--" + opt, default=None)

    s = sub.add_parser("supersede"); s.add_argument("old"); s.add_argument("new")

    l = sub.add_parser("list"); l.add_argument("--type", default=None)

    se = sub.add_parser("search"); se.add_argument("query", nargs="+"); se.add_argument("--k", type=int, default=6)
    se.add_argument("--vault", action="store_true", help="include the Obsidian vault, as the proxy does")

    q = sub.add_parser("ask")
    q.add_argument("question", nargs="+")
    q.add_argument("--k", type=int, default=6)
    q.add_argument("--model", default=MODEL)
    q.add_argument("--show", action="store_true")
    q.add_argument("--vault", action="store_true", help="include the Obsidian vault, as the proxy does")

    args = ap.parse_args()
    return {"build": lambda a: build_index(), "add": cmd_add, "supersede": cmd_supersede,
            "list": cmd_list, "search": cmd_search, "ask": cmd_ask}[args.cmd](args)


if __name__ == "__main__":
    raise SystemExit(main())
