#!/usr/bin/env python3
"""websearch - web search from the terminal, no API key. For the local model's Bash tool.

    python tools/websearch.py <query> [num_results] [-n N] [--recent d|w|m|y]
                             [--site domain] [--filetype ext] [--intitle X] [--inurl X]
                             [--op "raw:operator"]

Fully local tooling (the query does hit the internet - that is the point): scrapes keyless search
engines, so no key and no MCP. Tries Bing, then DuckDuckGo HTML, then DuckDuckGo Lite, and keeps
the first engine that returns real results. Prints ranked TITLE / URL / snippet. The local model
uses this to DISCOVER pages, then `webget.py <url>` to READ one.

Recency: `--recent d|w|m|y` (or day/week/month/year) restricts to newer results on every engine
that supports it - SearXNG `time_range`, ddgs `timelimit`, DuckDuckGo `df=`, Bing `filters=`.
Operators: anything typed inside <query> (site:, filetype:, quotes, OR, -term) passes through to
every engine UNCHANGED; the --site/--filetype/--intitle/--inurl/--op flags are conveniences that
append the operator for you. `-n`/`--num` is an explicit result count (wins over the positional).

NOTE: keyless search is IP-reputation dependent - a datacenter IP may get bot-decoy pages, but a
home/residential connection normally returns real results. If every engine here is blocked, add a
free Brave Search API key and extend this file; `webget.py` still works on any known URL.
"""
import sys
import re
import base64
from urllib.parse import unquote

UA = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36"
HEADERS = {"User-Agent": UA, "Accept": "text/html,application/xhtml+xml",
           "Accept-Language": "en-US,en;q=0.9"}

# normalised recency codes: d/w/m/y (long names accepted on the CLI)
RECENCY = {"d": "d", "day": "d", "w": "w", "week": "w",
           "m": "m", "month": "m", "y": "y", "year": "y"}
DEFAULT_NUM_RESULTS = 8


def _soup(html):
    from bs4 import BeautifulSoup
    try:
        return BeautifulSoup(html, "lxml")
    except Exception:
        return BeautifulSoup(html, "html.parser")


def _decode_bing(href):
    m = re.search(r"[?&]u=a1([A-Za-z0-9_\-]+)", href)
    if not m:
        return href
    s = m.group(1).replace("-", "+").replace("_", "/")
    s += "=" * (-len(s) % 4)
    try:
        return base64.b64decode(s).decode("utf-8", "replace")
    except Exception:
        return href


def _bing_freshness(recent):
    """Bing freshness filter: ez1=24h, ez2=week, ez3=month. A year has no ez4 - it needs an
    explicit day range, ez5_<start>_<end> in days since 1970-01-01."""
    if recent in ("d", "w", "m"):
        return 'ex1:"ez%d"' % (("d", "w", "m").index(recent) + 1)
    import time
    today = int(time.time() // 86400)
    return 'ex1:"ez5_%d_%d"' % (today - 365, today)


def _bing(query, n, requests, recent=None):
    params = {"q": query}
    if recent:
        params["filters"] = _bing_freshness(recent)
    r = requests.get("https://www.bing.com/search", params=params,
                     headers=HEADERS, timeout=20)
    soup = _soup(r.text)
    out = []
    for li in soup.select("li.b_algo"):
        h2 = li.find("h2")
        a = h2.find("a", href=True) if h2 else None
        if not a:
            continue
        url = _decode_bing(a["href"])
        if not url.startswith("http"):
            continue
        snip = ""
        cap = li.find(class_=re.compile("b_caption"))
        if cap and cap.find("p"):
            snip = cap.find("p").get_text(" ", strip=True)
        out.append((a.get_text(" ", strip=True), url, snip))
        if len(out) >= n:
            break
    return out


def _ddg(query, n, requests, recent=None):
    data = {"q": query, "kl": "us-en"}
    if recent:
        data["df"] = recent          # DuckDuckGo's time filter takes the same d/w/m/y codes
    r = requests.post("https://html.duckduckgo.com/html/", data=data,
                      headers=HEADERS, timeout=20)
    soup = _soup(r.text)
    out = []
    for a in soup.select("a.result__a"):
        href = a.get("href", "")
        m = re.search(r"[?&]uddg=([^&]+)", href)
        url = unquote(m.group(1)) if m else href
        if not url.startswith("http"):
            continue
        row = a.find_parent(class_=re.compile("result"))
        snip = ""
        if row and row.find(class_=re.compile("snippet")):
            snip = row.find(class_=re.compile("snippet")).get_text(" ", strip=True)
        out.append((a.get_text(" ", strip=True), url, snip))
        if len(out) >= n:
            break
    return out


def _relevant(query, results):
    """Guard against bot-decoy pages: at least one result should share a query word."""
    words = {w.lower() for w in re.findall(r"\w{4,}", query)}
    if not words:
        return bool(results)
    blob = " ".join((t + " " + u + " " + (s or "")).lower() for t, u, s in results)  # include the SNIPPET,
    return any(w in blob for w in words)                                             # not just title+url


def _ddgs_lib(query, n, recent=None):
    """The maintained `ddgs` library rotates across ~10 real search backends (bing/brave/google/
    mojeek/...) and returns clean dicts - far more reliable than scraping one engine's HTML."""
    from ddgs import DDGS
    kw = {"max_results": n, "backend": "auto"}
    if recent:
        kw["timelimit"] = recent     # d/w/m/y - only send the field when set
    rows = DDGS().text(query, **kw)
    out = []
    for r in rows:
        title = r.get("title") or ""
        url = r.get("href") or r.get("url") or ""
        if url.startswith("http"):
            out.append((title, url, r.get("body") or ""))
    return out


def _searxng(query, n, recent=None):
    """A local self-hosted SearXNG instance (tools/searxng/, http://localhost:8888) - fully local,
    keyless, unlimited, aggregates ~100 engines. Best option when it is running."""
    import os
    import json as _json
    import urllib.parse
    import urllib.request
    base = os.environ.get("SEARXNG_URL", "http://localhost:8888")
    params = {"q": query, "format": "json"}
    if recent:
        params["time_range"] = {"d": "day", "w": "week", "m": "month", "y": "year"}[recent]
    url = base.rstrip("/") + "/search?" + urllib.parse.urlencode(params)
    req = urllib.request.Request(url, headers={"User-Agent": "webresearch/1.0"})
    with urllib.request.urlopen(req, timeout=20) as r:
        data = _json.load(r)
    out = []
    for row in (data.get("results") or [])[:n]:
        u = row.get("url") or ""
        if u.startswith("http"):
            out.append((row.get("title") or "", u, row.get("content") or ""))
    return out


def search(query, n, recent=None):
    errors, ran = [], False   # ran = at least one engine returned WITHOUT erroring (maybe irrelevant)
    # 1) local self-hosted SearXNG if it's up (fully local, unlimited)
    try:
        res = _searxng(query, n, recent)
        ran = True
        if res and _relevant(query, res):
            return "searxng", res
    except Exception as e:
        errors.append("searxng: %s" % e)
    # 2) the ddgs library (installed, multi-backend, proven on this machine)
    try:
        res = _ddgs_lib(query, n, recent)
        ran = True
        if res and _relevant(query, res):
            return "ddgs", res
    except Exception as e:
        errors.append("ddgs: %s" % e)
    # 2) fall back to scraping Bing then DuckDuckGo HTML directly (no extra deps)
    try:
        import requests
    except Exception as e:
        raise RuntimeError("no requests and " + "; ".join(errors) + " %s" % e)
    for name, fn in (("bing", _bing), ("ddg-html", _ddg)):
        try:
            res = fn(query, n, requests, recent)
            ran = True
            if res and _relevant(query, res):
                return name, res
        except Exception as e:
            errors.append("%s: %s" % (name, e))
    # raise "all engines failed" ONLY if every engine threw; if an engine RAN but returned nothing
    # relevant, that is "no relevant results" (None, []), not a failure.
    if errors and not ran:
        raise RuntimeError("; ".join(errors))
    return None, []


def _parse_args(argv):
    """Hand-rolled so the old `websearch.py <query> [num_results]` shape keeps working exactly
    as before. Everything else is additive: -n/--num N, --recent d|w|m|y (long names OK),
    --site/--filetype/--intitle/--inurl X (appended as operator:X), --op "anything:verbatim".
    Returns (query, n, recent) with recent normalised to d/w/m/y or None."""
    norm = []                        # --flag=value -> --flag value, one code path for both forms
    for a in argv:
        if a.startswith("--") and "=" in a:
            k, v = a.split("=", 1)
            norm.extend([k, v])
        else:
            norm.append(a)
    positional, n_flag, recent, ops = [], None, None, []
    i = 0
    while i < len(norm):
        a = norm[i]
        if a in ("-n", "--num", "--num-results"):
            i += 1; n_flag = int(norm[i])
        elif a in ("--recent", "--time"):
            i += 1; recent = norm[i]
        elif a in ("--site", "--filetype", "--intitle", "--inurl"):
            i += 1; ops.append("%s:%s" % (a.lstrip("-"), norm[i]))
        elif a == "--op":
            i += 1; ops.append(norm[i])
        else:
            positional.append(a)
        i += 1
    if not positional:
        raise ValueError("missing query")
    n = None
    if len(positional) >= 2 and positional[-1].isdigit():    # old-style trailing count; a lone
        n = int(positional.pop())                            # numeric query is still a query
    query = " ".join(positional)                             # unquoted multi-word queries work too
    if ops:
        query += " " + " ".join(ops)                         # operators ride inside the query string
    if n_flag is not None:                                   # explicit flag wins over positional
        n = n_flag
    if n is None:
        n = DEFAULT_NUM_RESULTS
    if recent is not None:
        key = recent.lower()
        if key not in RECENCY:
            raise ValueError("--recent must be one of d/day, w/week, m/month, y/year")
        recent = RECENCY[key]
    return query, n, recent


def main():
    try:                # Windows cp1252 console: an emoji in a snippet must not crash the print
        sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    except Exception:
        pass
    try:
        query, n, recent = _parse_args(sys.argv[1:])
    except (ValueError, IndexError) as e:
        print("usage: websearch.py <query> [num_results] [-n N] [--recent d|w|m|y]")
        print("       [--site domain] [--filetype ext] [--intitle X] [--inurl X] [--op 'raw:op']")
        print("       (operators typed inside <query> pass through to every engine unchanged)")
        print("error: %s" % e)
        return 2
    try:
        engine, results = search(query, n, recent)
    except Exception as e:
        print("SEARCH ERROR (all engines): %s" % e); return 1
    if not results:
        print("(no relevant results - the IP may be rate-limited; try webget.py on a known URL)")
        return 0
    print("[via %s]" % engine)
    for i, (title, url, snip) in enumerate(results, 1):
        print("%d. %s" % (i, title))
        print("   %s" % url)
        if snip:
            print("   %s" % snip[:220])
        print()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
