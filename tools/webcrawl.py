#!/usr/bin/env python3
"""webcrawl - crawl a whole site (a page AND its child pages) and return all the clean text.

webget.py reads ONE page; this follows the links from a start page, breadth-first, to gather
EVERYTHING relevant on a topic - so the local AI can have the full picture, not one snippet. Reuses
webget's fetch + extract (trafilatura, with Scrapling stealth escalation for JS/anti-bot pages).

BOUNDED so it can never run away or hammer a site: a page cap, a depth cap, same-domain-by-default,
a total-text cap, and a small politeness delay. Returns aggregated, source-tagged text.

Fetches each depth level in PARALLEL (4 workers) but processes results in submission order, so the
output is deterministic and byte-identical to a serial crawl - just 3-4x faster on the wall clock.

    python tools/webcrawl.py <url> [max_pages] [max_depth]
    python tools/webcrawl.py https://docs.example.com/guide 25 2
"""
from __future__ import annotations

import concurrent.futures
import html
import re
import sys
import threading
import time
from pathlib import Path
from urllib.parse import urljoin, urlparse

sys.path.insert(0, str(Path(__file__).resolve().parent))
import webget

MAX_PAGES_CAP = 60          # hard ceiling regardless of what's asked
MAX_DEPTH_CAP = 4           # hard depth ceiling
MIN_TEXT = 150              # below this a page counts as "blocked/empty" -> try stealth
DEFAULT_PAGES = 20
DEFAULT_DEPTH = 2
TOTAL_CHAR_CAP = 24000     # stop once we have this much aggregated text
CRAWL_WORKERS = 4           # parallel fetchers per depth level
POLITENESS_DELAY = 0.15     # per-worker sleep after each fetch, success or not
SKIP_EXT = (".pdf", ".jpg", ".jpeg", ".png", ".gif", ".svg", ".zip", ".mp4", ".mp3",
            ".css", ".js", ".ico", ".woff", ".woff2", ".xml", ".rss")

# Scrapling's stealth path launches a real browser - serialize escalations so memory stays flat
_STEALTH_LOCK = threading.Lock()


def _links(page_html: str, base: str):
    out = []
    for h in re.findall(r'href=["\']([^"\'>]+)', page_html or ""):
        h = html.unescape(h.strip())                    # &amp; in an href -> & (real URL)
        if not h or h.startswith(("mailto:", "javascript:", "tel:", "#")):
            continue
        u = urljoin(base, h).split("#")[0]
        if u.startswith("http") and not urlparse(u).path.lower().endswith(SKIP_EXT):
            out.append(u)                               # test extension against the PATH, not query string
    return out


def _fetch_page(url: str):
    """One worker: plain fetch -> extract -> stealth escalation. Never raises - a bad page can't
    kill the crawl. Returns (page_html, final_url, title, text)."""
    page_html, final, title, text = "", url, "", ""
    try:
        page_html, final = webget.fetch_plain(url)
        title, text = webget.extract(page_html)                 # extract INSIDE the try
    except Exception:
        page_html, text = "", ""
    # escalate to stealth when plain is blocked/empty OR raised (HTTP 403 / JS wall) - the old
    # `except: continue` skipped this entirely, so anti-bot blocks were never retried.
    if len(text) < MIN_TEXT and webget._has("scrapling"):
        try:
            with _STEALTH_LOCK:                                 # one browser at a time
                html2, final2 = webget.fetch_stealth(url)
            title2, text2 = webget.extract(html2)
            if len(text2) > len(text):
                page_html, final, title, text = html2, final2, title2, text2   # keep the stealth title too
        except Exception:
            pass
    time.sleep(POLITENESS_DELAY)                                 # be polite on EVERY fetch, success or not
    return page_html, final, title, text


def crawl(start: str, max_pages: int = DEFAULT_PAGES, max_depth: int = DEFAULT_DEPTH,
          same_domain: bool = True):
    if not start.startswith(("http://", "https://")):
        start = "https://" + start
    max_pages = min(max_pages, MAX_PAGES_CAP)
    max_depth = min(max(0, max_depth), MAX_DEPTH_CAP)            # clamp depth (was unbounded)
    max_fetches = min(max_pages * 3, MAX_PAGES_CAP * 3)          # bound HTTP work even if pages yield no text
    root = urlparse(start).netloc
    seen: set[str] = set()
    pages, total, fetches = [], 0, 0
    frontier, depth = [start], 0                                 # URLs for the CURRENT level, in discovery order

    with concurrent.futures.ThreadPoolExecutor(max_workers=CRAWL_WORKERS) as pool:
        while frontier and len(pages) < max_pages and total < TOTAL_CHAR_CAP and fetches < max_fetches:
            level = []
            for url in frontier:                                 # dedup once per level - same first-wins
                if url not in seen:                              # order the old queue pop gave
                    seen.add(url)
                    level.append(url)
            next_frontier, i = [], 0
            while i < len(level) and len(pages) < max_pages and total < TOTAL_CHAR_CAP \
                    and fetches < max_fetches:
                chunk = level[i:i + min(CRAWL_WORKERS, max_fetches - fetches)]
                i += len(chunk)
                fetches += len(chunk)
                futures = [pool.submit(_fetch_page, u) for u in chunk]   # fetch the chunk in PARALLEL
                for fut in futures:                              # ...but process in SUBMISSION order,
                    page_html, final, title, text = fut.result()  # so output matches a serial crawl
                    if len(pages) >= max_pages or total >= TOTAL_CHAR_CAP:
                        continue                                 # bound tripped mid-chunk: a serial crawl
                    if text.strip():                             # would never have fetched this -> discard
                        pages.append((final, title, text.strip()))
                        total += len(text)
                    if depth < max_depth and page_html:          # enqueue child links for the NEXT level
                        for lk in _links(page_html, final):
                            if lk not in seen and (not same_domain or urlparse(lk).netloc == root):
                                next_frontier.append(lk)
            frontier, depth = next_frontier, depth + 1
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
