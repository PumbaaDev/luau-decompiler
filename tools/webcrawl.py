#!/usr/bin/env python3
"""webcrawl - crawl a whole site (a page AND its child pages) and return all the clean text.

webget.py reads ONE page; this follows the links from a start page, breadth-first, to gather
EVERYTHING relevant on a topic - so the local AI can have the full picture, not one snippet. Reuses
webget's fetch + extract (trafilatura, with Scrapling stealth escalation for JS/anti-bot pages).

BOUNDED so it can never run away or hammer a site: a page cap, a depth cap, same-domain-by-default,
a total-text cap, and a small politeness delay. Returns aggregated, source-tagged text.

    python tools/webcrawl.py <url> [max_pages] [max_depth]
    python tools/webcrawl.py https://docs.example.com/guide 25 2
"""
from __future__ import annotations

import collections
import re
import sys
import time
from pathlib import Path
from urllib.parse import urljoin, urlparse

sys.path.insert(0, str(Path(__file__).resolve().parent))
import webget

MAX_PAGES_CAP = 60          # hard ceiling regardless of what's asked
MIN_TEXT = 150              # below this a page counts as "blocked/empty" -> try stealth
DEFAULT_PAGES = 20
DEFAULT_DEPTH = 2
TOTAL_CHAR_CAP = 24000     # stop once we have this much aggregated text
SKIP_EXT = (".pdf", ".jpg", ".jpeg", ".png", ".gif", ".svg", ".zip", ".mp4", ".mp3",
            ".css", ".js", ".ico", ".woff", ".woff2", ".xml", ".rss")


def _links(html: str, base: str):
    out = []
    for h in re.findall(r'href=["\']([^"\'>]+)', html or ""):
        h = h.strip()
        if not h or h.startswith(("mailto:", "javascript:", "tel:", "#")):
            continue
        u = urljoin(base, h).split("#")[0]
        if u.startswith("http") and not u.lower().endswith(SKIP_EXT):
            out.append(u)
    return out


def crawl(start: str, max_pages: int = DEFAULT_PAGES, max_depth: int = DEFAULT_DEPTH,
          same_domain: bool = True):
    if not start.startswith(("http://", "https://")):
        start = "https://" + start
    max_pages = min(max_pages, MAX_PAGES_CAP)
    root = urlparse(start).netloc
    seen: set[str] = set()
    queue = collections.deque([(start, 0)])
    pages, total = [], 0

    while queue and len(pages) < max_pages and total < TOTAL_CHAR_CAP:
        url, depth = queue.popleft()
        if url in seen:
            continue
        seen.add(url)
        try:
            html, final = webget.fetch_plain(url)
        except Exception:
            continue
        title, text = webget.extract(html)
        if len(text) < MIN_TEXT and webget._has("scrapling"):   # escalate blocked/JS pages
            try:
                html2, final = webget.fetch_stealth(url)
                _t, text2 = webget.extract(html2)
                if len(text2) > len(text):
                    text, html = text2, html2
            except Exception:
                pass
        if text.strip():
            pages.append((final, title, text.strip()))
            total += len(text)
        if depth < max_depth:                                    # enqueue child links
            for lk in _links(html, final):
                if lk not in seen and (not same_domain or urlparse(lk).netloc == root):
                    queue.append((lk, depth + 1))
        time.sleep(0.3)                                          # be polite
    return pages


def format_pages(pages, cap: int = TOTAL_CHAR_CAP) -> str:
    if not pages:
        return "(crawl found no readable content)"
    out, used = ["crawled %d pages:" % len(pages)], 0
    for url, title, text in pages:
        per = text[:2500]
        block = "\n=== %s%s ===\n%s" % (url, ("  (" + title + ")") if title else "", per)
        if used + len(block) > cap:
            out.append("\n[...crawl truncated at %d chars]" % cap)
            break
        out.append(block)
        used += len(block)
    return "\n".join(out)


def main():
    if len(sys.argv) < 2:
        print("usage: webcrawl.py <url> [max_pages] [max_depth]"); return 2
    url = sys.argv[1]
    mp = int(sys.argv[2]) if len(sys.argv) > 2 else DEFAULT_PAGES
    md = int(sys.argv[3]) if len(sys.argv) > 3 else DEFAULT_DEPTH
    print("crawling %s (max %d pages, depth %d)..." % (url, mp, md), file=sys.stderr)
    pages = crawl(url, mp, md)
    print(format_pages(pages))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
