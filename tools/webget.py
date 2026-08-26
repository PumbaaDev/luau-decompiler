#!/usr/bin/env python3
"""webget - fetch a URL and print clean, readable text. For the local model's Bash tool.

    python tools/webget.py <url> [max_chars]

Fully local: no API key, no Anthropic-hosted tool, no MCP. The local uncensored model calls this
through Bash to READ any web page. Strips scripts/styles/nav/boilerplate and collapses whitespace so
the model gets article text, not markup. Prints title + final URL + text (default 6000 chars).
"""
import sys
import re

DEFAULT_MAX = 6000
UA = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126 Safari/537.36"


def fetch(url, timeout=20):
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


def extract(html):
    """HTML -> (title, clean text). Prefers <article>/<main>; drops boilerplate tags."""
    try:
        from bs4 import BeautifulSoup
    except ImportError:
        text = re.sub(r"(?is)<(script|style|noscript|template)[^>]*>.*?</\1>", " ", html)
        text = re.sub(r"(?s)<[^>]+>", " ", text)
        return "", re.sub(r"\s+", " ", text).strip()
    soup = BeautifulSoup(html, "lxml" if _has("lxml") else "html.parser")
    title = (soup.title.get_text(strip=True) if soup.title else "")
    for tag in soup(["script", "style", "noscript", "template", "svg", "nav", "footer",
                     "header", "form", "aside", "button"]):
        tag.decompose()
    main = soup.find("article") or soup.find("main") or soup.body or soup
    # keep block structure as newlines so the model sees paragraphs
    for br in main.find_all(["br"]):
        br.replace_with("\n")
    lines = []
    for el in main.find_all(["h1", "h2", "h3", "h4", "li", "p", "pre", "td", "blockquote"]):
        t = el.get_text(" ", strip=True)
        if t and len(t) > 1:
            lines.append(t)
    text = "\n".join(lines) if lines else main.get_text(" ", strip=True)
    text = re.sub(r"[ \t]+", " ", text)
    text = re.sub(r"\n{3,}", "\n\n", text)
    return title, text.strip()


def _has(mod):
    try:
        __import__(mod); return True
    except ImportError:
        return False


def main():
    if len(sys.argv) < 2:
        print("usage: webget.py <url> [max_chars]"); return 2
    url = sys.argv[1]
    if not url.startswith(("http://", "https://")):
        url = "https://" + url
    cap = int(sys.argv[2]) if len(sys.argv) > 2 else DEFAULT_MAX
    try:
        html, final = fetch(url)
    except Exception as e:
        print("FETCH ERROR: %s: %s" % (type(e).__name__, e)); return 1
    title, text = extract(html)
    if title:
        print("TITLE: " + title)
    print("URL: " + final)
    print("-" * 60)
    if len(text) > cap:
        print(text[:cap] + "\n\n[...truncated at %d chars; re-run with a larger max_chars for more]" % cap)
    else:
        print(text or "(no readable text extracted)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
