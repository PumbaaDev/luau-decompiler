import json, collections, pathlib
S = pathlib.Path(r"C:/Users/jep/AppData/Local/Temp/claude/C--Users-jep-Downloads-luau-decompiler-v2/805655f0-97a0-4109-862b-a46a4be53137/scratchpad")
man = json.loads((S / "chunk_manifest.json").read_text(encoding="utf-8"))
SEP = chr(92)

rows = []
def load(name, remap):
    for line in open(name, encoding="utf-8", errors="replace"):
        try:
            j = json.loads(line)
        except Exception:
            continue
        fn = j["file"].replace(SEP, "/").split("/")[-1]
        off, orig = 0, fn
        if remap and fn in man:
            off, orig = man[fn]["offset"], man[fn]["orig"]
        for d in (j.get("defects") or []):
            if isinstance(d.get("line"), int):
                d["line"] += off
            d["_file"] = orig
            rows.append(d)

for f, r in [("sweep_full1143.jsonl", False), ("sweep_big23.jsonl", False), ("sweep_chunks.jsonl", True)]:
    load(f, r)

k = collections.Counter(d.get("kind", "?") for d in rows)
print("MERGED corpus defects:", len(rows))
for kk, c in k.most_common():
    print("   %-22s %5d  (%.0f%%)" % (kk, c, 100 * c / len(rows)))

dense = collections.Counter(d["_file"] for d in rows)
print("densest files:")
for f, c in dense.most_common(5):
    print("   %3d  %s" % (c, f[:60]))

with open("sweep_corpus_merged.jsonl", "w", encoding="utf-8") as fh:
    for d in rows:
        fh.write(json.dumps(d) + "\n")
print("wrote sweep_corpus_merged.jsonl (chunk lines mapped back to originals)")
