#!/usr/bin/env python3
"""Real web access for the local setup: URL -> clean LLM-ready markdown, via Crawl4AI
(headless Chromium under the hood, so JS-rendered pages work). The local model runs
this with ONE Bash command and reasons over the markdown - no MCP tool schema, no
context bloat, one call instead of a ten-step browser dance.

    python tools/scrape.py https://example.com                 # markdown to stdout
    python tools/scrape.py https://example.com --out page.md    # save to a file
    python tools/scrape.py https://example.com --max-chars 20000 --raw

The local model should be TOLD to use it ("scrape <url> with tools/scrape.py"); a 30B
does not reliably reach for tools on its own. Deterministic scrape -> the model reads
the result is the reliable pattern.
"""
from __future__ import annotations

import argparse
import asyncio
import sys
import warnings

warnings.filterwarnings("ignore")   # deprecation noise must not pollute the markdown

# Playwright needs the Proactor loop on Windows (the default Selector loop cannot
# spawn the browser subprocess).
if sys.platform == "win32":
    asyncio.set_event_loop_policy(asyncio.WindowsProactorEventLoopPolicy())


async def _scrape(url: str, fit: bool):
    from crawl4ai import AsyncWebCrawler
    async with AsyncWebCrawler(verbose=False) as crawler:
        res = await crawler.arun(url=url)
        if not getattr(res, "success", False):
            return None, getattr(res, "error_message", "crawl failed")
        md = res.markdown
        # md is a str on older crawl4ai, a MarkdownGenerationResult on newer.
        if hasattr(md, "raw_markdown") or hasattr(md, "fit_markdown"):
            text = ""
            if fit:
                text = getattr(md, "fit_markdown", "") or ""
            if not text:
                text = getattr(md, "raw_markdown", "") or str(md)
        else:
            text = md or ""
        return text, None


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("url")
    ap.add_argument("--out", help="save markdown to this file instead of stdout")
    ap.add_argument("--max-chars", type=int, default=0, help="truncate output (0 = no limit)")
    ap.add_argument("--raw", action="store_true", help="raw markdown instead of the fit/cleaned view")
    a = ap.parse_args()

    try:
        text, err = asyncio.run(_scrape(a.url, fit=not a.raw))
    except Exception as e:
        print("SCRAPE ERROR: %s: %s" % (type(e).__name__, e), file=sys.stderr)
        return 2
    if err:
        print("SCRAPE FAILED: %s" % err, file=sys.stderr)
        return 1

    text = text or ""
    if a.max_chars and len(text) > a.max_chars:
        text = text[:a.max_chars] + "\n\n...[truncated at %d chars]" % a.max_chars

    if a.out:
        with open(a.out, "w", encoding="utf-8") as f:
            f.write(text)
        print("wrote %d chars -> %s" % (len(text), a.out))
    else:
        try:
            sys.stdout.reconfigure(encoding="utf-8")
        except Exception:
            pass
        print(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
