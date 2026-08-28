#!/usr/bin/env python3
"""webget - fetch a URL and print clean, LLM-ready markdown/text. For the local model's Bash tool.

    python tools/webget.py <url> [max_chars]

Fully local: no API key, no Anthropic-hosted tool, no MCP. The local uncensored model calls this
through Bash to READ any web page. Extraction ladder (best available wins):
  1. fetch: plain HTTP (requests) - fast path for normal pages
  2. extract: trafilatura -> markdown (benchmark-best boilerplate removal) ; bs4 fallback ; regex last
  3. escalate: if the page comes back blocked/JS-empty AND Scrapling is installed, refetch with its
     stealth browser (TLS impersonation + Cloudflare bypass), then extract again.
PDFs (content-type application/pdf, a .pdf URL, or the %PDF magic bytes) are extracted offline with
pypdf -> pdfminer.six; if neither is installed it says so instead of printing binary soup. HTML
<table> data is appended after the main text as pipe-delimited rows so tabular pages stay usable.
So a normal article is cheap, and a JS/anti-bot page still gets read. Prints title + final URL + text.
"""
import io
import sys
import re

DEFAULT_MAX = 6000
MIN_GOOD = 250   # under this many chars, assume the plain fetch was blocked/JS-gated -> escalate
MAX_PDF_PAGES = 50   # extraction-time cap for pathological PDFs; the char cap trims output anyway
MAX_TABLES = 8       # keep the appended TABLES section bounded
MAX_TABLE_ROWS = 40
UA = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126 Safari/537.36"


def _has(mod):
    try:
        __import__(mod); return True
    except Exception:
        return False


def _decode(data, ctype=""):
    """bytes -> str, honouring the Content-Type charset when it names a real codec."""
    m = re.search(r"charset=([\w.-]+)", ctype or "", re.I)
    if m:
        try:
            return data.decode(m.group(1), "replace")
        except LookupError:
            pass
    return data.decode("utf-8", "replace")


def fetch_raw(url, timeout=20):
    """GET -> (bytes, final_url, content_type). The one place that talks HTTP."""
    try:
        import requests
        r = requests.get(url, headers={"User-Agent": UA, "Accept-Language": "en"},
                         timeout=timeout, allow_redirects=True)
        r.raise_for_status()
        return r.content, r.url, r.headers.get("Content-Type", "")
    except ImportError:
        import urllib.request
        req = urllib.request.Request(url, headers={"User-Agent": UA})
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return resp.read(), resp.geturl(), resp.headers.get("Content-Type", "")


def fetch_plain(url, timeout=20):
    data, final, ctype = fetch_raw(url, timeout)
    return _decode(data, ctype), final


def fetch_stealth(url):
    """Scrapling's stealth browser: JS rendering + Cloudflare bypass. Only used on escalation."""
    from scrapling.fetchers import StealthyFetcher
    page = StealthyFetcher.fetch(url, headless=True, network_idle=True, solve_cloudflare=True)
    html = getattr(page, "html_content", None) or (page.body if hasattr(page, "body") else "")
    return html, getattr(page, "url", url)


def looks_like_pdf(url, ctype, data):
    """Content-type header, .pdf URL path, or the %PDF magic bytes - any one is enough."""
    if "application/pdf" in (ctype or "").lower():
        return True
    if url.split("?", 1)[0].split("#", 1)[0].lower().endswith(".pdf"):
        return True
    return b"%PDF-" in data[:1024]


def extract_pdf(data):
    """PDF bytes -> (title, text). pypdf first, pdfminer.six fallback - both pure-python, offline.
    Returns ("", "") when neither is installed or no text layer exists; main() explains which."""
    if _has("pypdf"):
        try:
            from pypdf import PdfReader
            reader = PdfReader(io.BytesIO(data))
            if reader.is_encrypted:
                reader.decrypt("")            # empty owner password is common; a real one still fails
            pages = [(p.extract_text() or "") for p in reader.pages[:MAX_PDF_PAGES]]
            text = "\n\n".join(t.strip() for t in pages if t.strip())
            if len(reader.pages) > MAX_PDF_PAGES:
                text += "\n\n[... %d more pages not extracted]" % (len(reader.pages) - MAX_PDF_PAGES)
            title = ""
            meta = reader.metadata
            if meta and getattr(meta, "title", None):
                title = str(meta.title)
            if text.strip():
                return title, re.sub(r"\n{3,}", "\n\n", text).strip()
        except Exception:
            pass                              # fall through to pdfminer
    if _has("pdfminer"):
        try:
            from pdfminer.high_level import extract_text as pdfminer_extract
            text = pdfminer_extract(io.BytesIO(data), maxpages=MAX_PDF_PAGES) or ""
            if text.strip():
                return "", re.sub(r"\n{3,}", "\n\n", text).strip()
        except Exception:
            pass
    return "", ""


def extract_tables(html, body_text=""):
    """HTML -> '<table>' data as pipe-delimited rows, one section per table, so tabular pages
    stay usable. Skips layout shells (<2 rows or single-column), nested inner tables, and tables
    whose first row already made it into body_text (trafilatura keeps some tables itself)."""
    if not _has("bs4"):
        return ""
    from bs4 import BeautifulSoup
    soup = BeautifulSoup(html, "lxml" if _has("lxml") else "html.parser")
    sections = []
    for table in soup.find_all("table"):
        if len(sections) >= MAX_TABLES:
            sections.append("[... more tables not shown, cap is %d]" % MAX_TABLES)
            break
        if table.find_parent("table"):        # inner rows already surface via the outer table
            continue
        rows, truncated = [], False
        for tr in table.find_all("tr"):
            cells = [re.sub(r"\s+", " ", c.get_text(" ", strip=True)).replace("|", "/")
                     for c in tr.find_all(["th", "td"])]
            if any(cells):
                rows.append(" | ".join(cells))
            if len(rows) >= MAX_TABLE_ROWS:
                truncated = True
                break
        if len(rows) < 2 or all(" | " not in r for r in rows):
            continue                          # layout shell, not data
        if body_text and rows[0] in body_text:
            continue                          # the main extraction already carries this table
        if truncated:
            rows.append("[... table truncated at %d rows]" % MAX_TABLE_ROWS)
        sections.append("TABLE %d:\n%s" % (len(sections) + 1, "\n".join(rows)))
    return "\n\n".join(sections)


def _with_tables(text, html):
    tables = extract_tables(html, text)
    if not tables:
        return text
    return (text + "\n\n" + tables).strip() if text else tables


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
    is_pdf = False
    try:
        data, final, ctype = fetch_raw(url)
        is_pdf = looks_like_pdf(url, ctype, data)
        if is_pdf:
            title, text = extract_pdf(data)
            if text:
                err = ""
            elif not (_has("pypdf") or _has("pdfminer")):
                err = "PDF fetched but no offline PDF library installed - pip install pypdf (pure-python, stays local)"
            else:
                err = "PDF fetched but no text extracted (encrypted or scanned-image PDF?)"
        else:
            html = _decode(data, ctype)
            title, text = extract(html)
            text = _with_tables(text, html)
            err = ""
    except Exception as e:
        text = ""
        err = "%s: %s" % (type(e).__name__, e)

    # escalate to the stealth browser if the plain fetch was blocked / JS-empty (never for PDFs -
    # a browser render of a PDF yields nothing extractable)
    if not is_pdf and len(text) < MIN_GOOD and _has("scrapling"):
        try:
            html2, final2 = fetch_stealth(url)
            t2, x2 = extract(html2)
            x2 = _with_tables(x2, html2)
            if len(x2) > len(text):                       # commit the stealth URL/title ONLY when its
                title, text, final = (t2 or title), x2, final2   # text actually wins - else the printed
            err = ""                                      # URL would mismatch the printed (plain) text
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
