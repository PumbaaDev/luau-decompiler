#!/usr/bin/env python3
"""websearch - web search from the terminal, no API key. For the local model's Bash tool.

    python tools/websearch.py <query> [num_results]

Fully local tooling (the query does hit the internet - that is the point): scrapes keyless search
engines, so no key and no MCP. Tries Bing, then DuckDuckGo HTML, then DuckDuckGo Lite, and keeps
the first engine that returns real results. Prints ranked TITLE / URL / snippet. The local model
uses this to DISCOVER pages, then `webget.py <url>` to READ one.

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


def _bing(query, n, requests):
    r = requests.get("https://www.bing.com/search", params={"q": query},
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


def _ddg(query, n, requests):
    r = requests.post("https://html.duckduckgo.com/html/", data={"q": query, "kl": "us-en"},
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
    blob = " ".join((t + " " + u).lower() for t, u, _ in results)
    return any(w in blob for w in words)


def _ddgs_lib(query, n):
    """The maintained `ddgs` library rotates across ~10 real search backends (bing/brave/google/
    mojeek/...) and returns clean dicts - far more reliable than scraping one engine's HTML."""
    from ddgs import DDGS
    rows = DDGS().text(query, max_results=n, backend="auto")
    out = []
    for r in rows:
        title = r.get("title") or ""
        url = r.get("href") or r.get("url") or ""
        if url.startswith("http"):
            out.append((title, url, r.get("body") or ""))
    return out


def _searxng(query, n):
    """A local self-hosted SearXNG instance (tools/searxng/, http://localhost:8888) - fully local,
    keyless, unlimited, aggregates ~100 engines. Best option when it is running."""
    import json as _json
    import urllib.parse
    import urllib.request
    base = os.environ.get("SEARXNG_URL", "http://localhost:8888")
    url = base.rstrip("/") + "/search?" + urllib.parse.urlencode({"q": query, "format": "json"})
    req = urllib.request.Request(url, headers={"User-Agent": "webresearch/1.0"})
    with urllib.request.urlopen(req, timeout=20) as r:
        data = _json.load(r)
    out = []
    for row in (data.get("results") or [])[:n]:
        u = row.get("url") or ""
        if u.startswith("http"):
            out.append((row.get("title") or "", u, row.get("content") or ""))
    return out


def search(query, n):
    errors = []
    # 1) local self-hosted SearXNG if it's up (fully local, unlimited)
    try:
        res = _searxng(query, n)
        if res and _relevant(query, res):
            return "searxng", res
    except Exception as e:
        errors.append("searxng: %s" % e)
    # 2) the ddgs library (installed, multi-backend, proven on this machine)
    try:
        res = _ddgs_lib(query, n)
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
            res = fn(query, n, requests)
            if res and _relevant(query, res):
                return name, res
        except Exception as e:
            errors.append("%s: %s" % (name, e))
    if errors:
        raise RuntimeError("; ".join(errors))
    return None, []


def main():
    if len(sys.argv) < 2:
        print("usage: websearch.py <query> [num_results]"); return 2
    query = sys.argv[1]
    n = int(sys.argv[2]) if len(sys.argv) > 2 else 8
    try:
        engine, results = search(query, n)
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
