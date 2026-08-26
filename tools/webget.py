#!/usr/bin/env python3
"""webget - fetch a URL and print clean, LLM-ready markdown/text. For the local model's Bash tool.

    python tools/webget.py <url> [max_chars]

Fully local: no API key, no Anthropic-hosted tool, no MCP. The local uncensored model calls this
through Bash to READ any web page. Extraction ladder (best available wins):
  1. fetch: plain HTTP (requests) - fast path for normal pages
  2. extract: trafilatura -> markdown (benchmark-best boilerplate removal) ; bs4 fallback ; regex last
  3. escalate: if the page comes back blocked/JS-empty AND Scrapling is installed, refetch with its
     stealth browser (TLS impersonation + Cloudflare bypass), then extract again.
So a normal article is cheap, and a JS/anti-bot page still gets read. Prints title + final URL + text.
"""
import sys
import re

DEFAULT_MAX = 6000
MIN_GOOD = 250   # under this many chars, assume the plain fetch was blocked/JS-gated -> escalate
UA = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126 Safari/537.36"


def _has(mod):
    try:
        __import__(mod); return True
    except Exception:
        return False


def fetch_plain(url, timeout=20):
    try:
        import requests
        r = requests.get(url, headers={"User-Agent": UA, "Accept-Language": "en"},
                         timeout=timeout, allow_redirects=True)
        r.raise_for_status()
        return r.text, r.url
    except ImportError:
        import urllib.request
        req = urllib.request.Request(url, headers={"User-Agent": UA})
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return resp.read().decode("utf-8", "replace"), resp.geturl()


def fetch_stealth(url):
    """Scrapling's stealth browser: JS rendering + Cloudflare bypass. Only used on escalation."""
    from scrapling.fetchers import StealthyFetcher
    page = StealthyFetcher.fetch(url, headless=True, network_idle=True, solve_cloudflare=True)
    html = getattr(page, "html_content", None) or (page.body if hasattr(page, "body") else "")
    return html, getattr(page, "url", url)


def extract(html):
    """HTML -> (title, clean markdown/text). trafilatura -> bs4 -> regex."""
    if _has("trafilatura"):
        import trafilatura
        md = trafilatura.extract(html, output_format="markdown", include_tables=True,
                                 include_links=False, favor_recall=True)
        if md and len(md.strip()) >= 40:
            title = ""
            meta = trafilatura.extract_metadata(html)
            if meta and getattr(meta, "title", None):
                title = meta.title
            return title, md.strip()
    if _has("bs4"):
        from bs4 import BeautifulSoup
        soup = BeautifulSoup(html, "lxml" if _has("lxml") else "html.parser")
        title = soup.title.get_text(strip=True) if soup.title else ""
        for tag in soup(["script", "style", "noscript", "template", "svg", "nav", "footer",
                         "header", "form", "aside", "button"]):
            tag.decompose()
        main = soup.find("article") or soup.find("main") or soup.body or soup
        lines = [el.get_text(" ", strip=True) for el in
                 main.find_all(["h1", "h2", "h3", "h4", "li", "p", "pre", "td", "blockquote"])]
        text = "\n".join(t for t in lines if t and len(t) > 1) or main.get_text(" ", strip=True)
        return title, re.sub(r"\n{3,}", "\n\n", re.sub(r"[ \t]+", " ", text)).strip()
    text = re.sub(r"(?is)<(script|style|noscript)[^>]*>.*?</\1>", " ", html)
    return "", re.sub(r"\s+", " ", re.sub(r"(?s)<[^>]+>", " ", text)).strip()


def main():
    if len(sys.argv) < 2:
        print("usage: webget.py <url> [max_chars]"); return 2
    url = sys.argv[1]
    if not url.startswith(("http://", "https://")):
        url = "https://" + url
    cap = int(sys.argv[2]) if len(sys.argv) > 2 else DEFAULT_MAX

    title = text = ""
    final = url
    try:
        html, final = fetch_plain(url)
        title, text = extract(html)
    except Exception as e:
        text = ""
        err = "%s: %s" % (type(e).__name__, e)
    else:
        err = ""

    # escalate to the stealth browser if the plain fetch was blocked / JS-empty
    if len(text) < MIN_GOOD and _has("scrapling"):
        try:
            html, final = fetch_stealth(url)
            t2, x2 = extract(html)
            if len(x2) > len(text):
                title, text = (t2 or title), x2
            err = ""
        except Exception as e:
            if not text:
                err = "stealth %s: %s" % (type(e).__name__, e)

    if not text:
        print("FETCH ERROR: %s" % (err or "no readable text extracted")); return 1
    if title:
        print("TITLE: " + title)
    print("URL: " + final)
    print("-" * 60)
    print(text[:cap] + ("\n\n[...truncated at %d chars]" % cap if len(text) > cap else ""))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
