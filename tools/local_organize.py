"""local_organize.py - organise staged PC topics into knowledge docs using the LOCAL model.

Claude's cloud quota kept getting exhausted mid-workflow. This does the same organise step on
Jep's own uncensored-coder via Ollama: free, unlimited, uncensored. For each topic folder of
staged (secret-scrubbed) .txt files it writes one clean markdown knowledge doc.

Usage:
    python tools/local_organize.py <batches_root> <out_dir>
"""
from __future__ import annotations
import os, re, sys, glob, json, time, urllib.request

MODEL = "uncensored-coder"
OLLAMA = "http://localhost:11434/api/generate"
CORPUS_CAP = 80_000        # ~20k tokens, fits num_ctx 32768 with room for the answer

PROMPT = (
    "You are organising Jep's OWN personal files into a clean knowledge note for his private "
    "offline assistant. Below is the text of his '{topic}' files (already secret-scrubbed). Write "
    "ONE concise, factual markdown knowledge document capturing everything genuinely useful to know "
    "about Jep from these files: his projects, dates, deadlines, decisions, numbers, statuses, "
    "employer/uni details, money/admin, and preferences. Merge duplicates, drop boilerplate and "
    "filler. Be specific (real names, dates, figures). Output ONLY the markdown document.\n\n"
    "FILES:\n{corpus}"
)


def gen(prompt: str, timeout: int = 600) -> str:
    body = {
        "model": MODEL, "prompt": prompt, "stream": False,
        "options": {"num_ctx": 32768, "temperature": 0.3, "num_predict": 2048},
        "keep_alive": -1,                     # int, not "-1" (Ollama rejects the string)
    }
    req = urllib.request.Request(OLLAMA, data=json.dumps(body).encode(),
                                 headers={"Content-Type": "application/json"})
    return json.loads(urllib.request.urlopen(req, timeout=timeout).read()).get("response", "")


def main() -> int:
    broot, out = sys.argv[1], sys.argv[2]
    os.makedirs(out, exist_ok=True)
    topics = {}
    for d in sorted(glob.glob(os.path.join(broot, "*"))):
        m = re.match(r"[a-z]?\d+_(.+)", os.path.basename(d))
        if not m:
            continue
        topics.setdefault(m.group(1), []).append(d)
    print(f"local_organize: {len(topics)} topics from {broot}")
    for t, dirs in topics.items():
        texts = []
        for d in dirs:
            for f in glob.glob(os.path.join(d, "*.txt")):
                try:
                    texts.append(open(f, encoding="utf-8", errors="replace").read())
                except Exception:
                    pass
        corpus = ("\n\n---\n\n".join(texts))[:CORPUS_CAP]
        if len(corpus.strip()) < 60:
            print(f"  {t}: empty, skipped"); continue
        t0 = time.time()
        try:
            doc = gen(PROMPT.format(topic=t, corpus=corpus)).strip()
        except Exception as e:
            print(f"  {t}: FAILED {str(e)[:70]}"); continue
        if len(doc) < 60:
            print(f"  {t}: model returned too little, skipped"); continue
        with open(os.path.join(out, t + ".md"), "w", encoding="utf-8", newline="") as w:
            w.write(f"# {t}\n\n{doc}")
        print(f"  {t}: {len(doc)} chars in {time.time()-t0:.0f}s")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
