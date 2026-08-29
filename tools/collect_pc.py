"""collect_pc.py - token-saver collector: gather Jep's real knowledge files from across the PC,
extract their text, scrub secrets, and stage clean text for the opus+fable organise pass.

Jep asked to "search my whole PC for data and give it to the local model". The PC is mostly
media, installers, dev caches, and the already-indexed decompiler. This walks a CURATED set of
knowledge roots, keeps only text-bearing knowledge (docs, notes, uni work, YouTube scripts,
code), extracts text from PDF/docx/pptx, SCRUBS secrets, and writes scrubbed .txt into a staging
dir mirroring provenance. It reads files with scripts (no model tokens) - the reading/organising
is the agents' job in the next phase.

Hard exclusions:
- media (mp4/png/jpg/...), executables/installers (exe/msi/lnk/dll), archives, fonts
- dev caches / build output (node_modules, .git, target, venv, __pycache__, site-packages, ...)
- already-indexed corpora (the luau-decompiler projects, roblox_corpus, the ClaudeMemory vault)
- SECRET-NAMED files (client_secret*, *credential*, Backup-codes*, *.pem/.key, id_rsa, .env)
Plus in-content secret scrubbing (reused from ingest_personal) on everything that IS staged.

Usage:
    python tools/collect_pc.py --dry-run     # report what would be staged, extract nothing
    python tools/collect_pc.py               # stage scrubbed text, print STAGE_DIR + manifest path
"""
from __future__ import annotations
import os, re, sys, json, zipfile, hashlib, time, pathlib
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from tools.ingest_personal import build_scrubbers, scrub

HOME = r"C:\Users\jep"
# Curated knowledge roots. Order = provenance label priority.
ROOTS = [
    ("desktop",      r"C:\Users\jep\Desktop"),
    ("documents",    r"C:\Users\jep\Documents"),
    ("downloads",    r"C:\Users\jep\Downloads"),
    ("uni_lantern",  r"C:\Users\jep\Lantern of Shadows"),
    ("source",       r"C:\Users\jep\source"),
    ("tpt_uploader", r"C:\Users\jep\.tpt_uploader"),
    ("onedrive",     r"C:\Users\jep\OneDrive"),
    ("d_github",     r"D:\pc files\GitHub"),
    ("d_archive",    r"D:\Desktop Archive"),
    ("d_botb",       r"D:\_BOTB_QUARANTINE_2026-07-30"),
]

# KNOWLEDGE only: documents + notes. NOT code - the PC's code bulk is vendored C++ SDKs, GLM/GLFW
# headers, Unity Library folders, and ~15 duplicate copies of one uni OpenGL project. Jep's own
# code is already indexed (decompiler under jep:/digest, corpus under roblox:). What's NEW and worth
# knowing is his written knowledge: uni study material, assignments, dissertation, work/admin docs,
# YouTube scripts, notes.
TEXT_EXTS = {".md", ".txt", ".rtf", ".csv", ".tsv", ".ipynb"}
DOC_EXTS = {".pdf", ".docx", ".pptx"}          # need extraction
JSON_EXTS = set()                              # data dumps, not knowledge
INDEX_EXTS = TEXT_EXTS | DOC_EXTS

# path segments that mark junk / caches / already-indexed corpora
JUNK_SEG = tuple(s.lower() for s in (
    "node_modules", os.sep + ".git" + os.sep, os.sep + "target" + os.sep, ".venv", os.sep + "venv" + os.sep,
    "__pycache__", ".cache", "_model-cache", "roblox_corpus", ".next", os.sep + "dist" + os.sep,
    os.sep + "build" + os.sep, ".mypy_cache", "site-packages", os.sep + "obj" + os.sep,
    "luau-decompiler-v2", "luau-decompiler-public", "claudememory", os.sep + ".tmp",
    ".drivedownload", ".driveupload", os.sep + "library" + os.sep, os.sep + "temp" + os.sep,
    ".rustup", ".cargo", os.sep + "bin" + os.sep + "debug", "packages" + os.sep, ".gradle",
    "$recycle", "$windows", "$winreagent", "$sysreset", "windowsapps",
    "winutil", "chromedriver", "practice result_files", "dc site_files",
    "assetstore", os.sep + "packagecache" + os.sep, os.sep + "logs" + os.sep,
))
# filenames that name a secret -> never stage, regardless of content
SECRET_NAME = re.compile(
    r"(client_secret|credential|backup[-_ ]?codes?|\bsecret\b|\btoken\b|id_rsa|\.pem$|\.key$|"
    r"^\.env|recovery[-_ ]?codes?|private[-_ ]?key|apikey|api[-_ ]?key)", re.I)

MAX_TEXT = 3_000_000        # 3 MB of plain text is already a huge single note
MAX_PDF  = 40_000_000       # a big uni PDF
MAX_JSON = 200_000          # config-sized json only


# ---------- extractors (dependency-light) ----------
def _extract_pdf(path):
    try:
        import fitz  # PyMuPDF
    except Exception:
        return None            # signal: no extractor
    try:
        doc = fitz.open(path)
        out = []
        for pg in doc:
            out.append(pg.get_text("text"))
        doc.close()
        return "\n".join(out)
    except Exception:
        return ""


def _xml_text(data: bytes) -> str:
    # crude but robust: pull text between XML tags, collapse whitespace
    txt = re.sub(rb"<[^>]+>", b" ", data)
    return re.sub(r"[ \t]+", " ", txt.decode("utf-8", "replace"))


def _extract_docx(path):
    try:
        with zipfile.ZipFile(path) as z:
            parts = [n for n in z.namelist() if n == "word/document.xml"
                     or (n.startswith("word/") and n.endswith(".xml") and "document" in n)]
            out = []
            for n in parts or (["word/document.xml"] if "word/document.xml" in z.namelist() else []):
                out.append(_xml_text(z.read(n)))
            return "\n".join(out)
    except Exception:
        return ""


def _extract_pptx(path):
    try:
        with zipfile.ZipFile(path) as z:
            slides = sorted(n for n in z.namelist()
                            if n.startswith("ppt/slides/slide") and n.endswith(".xml"))
            return "\n".join(_xml_text(z.read(n)) for n in slides)
    except Exception:
        return ""


def _read_text(path):
    try:
        with open(path, "rb") as f:
            raw = f.read(MAX_TEXT + 1)
        if b"\x00" in raw[:65536]:          # binary sneaking in under a text ext
            return None
        return raw.decode("utf-8", "replace")
    except Exception:
        return None


def extract(path, ext):
    if ext == ".pdf":
        return _extract_pdf(path)
    if ext == ".docx":
        return _extract_docx(path)
    if ext == ".pptx":
        return _extract_pptx(path)
    return _read_text(path)


def _is_junk(fp_lower: str) -> bool:
    return any(seg in fp_lower for seg in JUNK_SEG)


def walk():
    """Yield (area, root, full_path, ext) for candidate knowledge files."""
    seen = set()
    for area, root in ROOTS:
        if not os.path.isdir(root):
            continue
        for dirpath, dirnames, filenames in os.walk(root):
            dl = dirpath.lower()
            if _is_junk(dl + os.sep):
                dirnames[:] = []            # prune the whole subtree
                continue
            for fn in filenames:
                ext = os.path.splitext(fn)[1].lower()
                if ext not in INDEX_EXTS:
                    continue
                if SECRET_NAME.search(fn):
                    yield (area, root, os.path.join(dirpath, fn), ext, "SECRET_NAME")
                    continue
                fp = os.path.join(dirpath, fn)
                if fp.lower() in seen:
                    continue
                seen.add(fp.lower())
                yield (area, root, fp, ext, "ok")


def main():
    dry = "--dry-run" in sys.argv[1:]
    pats = build_scrubbers()
    stage = os.path.join(os.environ.get("TEMP", "."), f"pc_stage_{int(os.getpid())}")
    if not dry:
        os.makedirs(stage, exist_ok=True)
    manifest = []
    stats = {"staged": 0, "skip_secret_name": 0, "skip_size": 0, "skip_no_extractor": 0,
             "skip_empty": 0, "skip_binary": 0, "skip_dup": 0, "text_mb": 0.0}
    by_area = {}
    seen_hash = set()          # content dedup: the opengl archive has ~15 identical project copies
    t0 = time.time()
    for area, root, fp, ext, flag in walk():
        if flag == "SECRET_NAME":
            stats["skip_secret_name"] += 1
            continue
        try:
            sz = os.path.getsize(fp)
        except OSError:
            continue
        cap = MAX_PDF if ext == ".pdf" else (MAX_JSON if ext in JSON_EXTS else MAX_TEXT)
        if sz > cap:
            stats["skip_size"] += 1
            continue
        text = extract(fp, ext)
        if text is None:
            stats["skip_binary"] += 1 if ext in TEXT_EXTS else 0
            stats["skip_no_extractor"] += 1 if ext in DOC_EXTS else 0
            continue
        text = text.strip()
        if len(text) < 40:
            stats["skip_empty"] += 1
            continue
        h = hashlib.sha1(text.encode("utf-8", "replace")).hexdigest()
        if h in seen_hash:                     # exact-duplicate file (duplicate project copy)
            stats["skip_dup"] += 1
            continue
        seen_hash.add(h)
        text = scrub(text, pats)
        rel = os.path.relpath(fp, root).replace(os.sep, "__")
        name = f"{area}__{rel}"
        name = re.sub(r"[^A-Za-z0-9._-]+", "_", name)[:150] + ".txt"
        stats["staged"] += 1
        stats["text_mb"] += len(text) / 1e6
        by_area[area] = by_area.get(area, 0) + 1
        manifest.append({"area": area, "src": fp, "staged": name, "chars": len(text), "ext": ext})
        if not dry:
            header = f"# source: {fp}\n# area: {area}\n\n"
            with open(os.path.join(stage, name), "w", encoding="utf-8") as w:
                w.write(header + text)
    dur = time.time() - t0
    print(f"collect_pc: scanned {len(ROOTS)} roots in {dur:.0f}s")
    print(f"  staged {stats['staged']} files, {stats['text_mb']:.1f} MB text")
    print(f"  by area: {dict(sorted(by_area.items(), key=lambda x:-x[1]))}")
    print(f"  skipped: secret-named={stats['skip_secret_name']} oversize={stats['skip_size']} "
          f"no-extractor={stats['skip_no_extractor']} binary={stats['skip_binary']} "
          f"empty={stats['skip_empty']} duplicate={stats['skip_dup']}")
    if dry:
        print("DRY RUN: nothing staged.")
        # show a category breakdown so the filter can be eyeballed
        import collections
        exts = collections.Counter(m["ext"] for m in manifest)
        print("  by ext:", dict(exts.most_common()))
        return 0
    mpath = os.path.join(stage, "_manifest.json")
    with open(mpath, "w", encoding="utf-8") as w:
        json.dump(manifest, w)
    print("STAGE_DIR:", stage)
    print("MANIFEST:", mpath)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
