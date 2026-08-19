#!/usr/bin/env python3
"""Sweep decompiled Luau files through a local Ollama model, emit JSONL findings.

First-pass triage only: flags which files look defective and why, so the
expensive reasoning pass can be aimed at the ranked shortlist instead of the
whole corpus.

    python tools/sweep_local.py bc --limit 20
    python tools/sweep_local.py bc --out sweep.jsonl

Resumes automatically: files already present in the output JSONL are skipped.
"""
from __future__ import annotations

import argparse
import json
import sys
import threading
import time
import urllib.error
import urllib.request
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path

OLLAMA_URL = "http://localhost:11434/api/generate"
DEFAULT_MODEL = "gemma4:12b-it-q4_K_M"

# Keeping num_ctx small matters: a 256K context builds a KV cache large enough
# to push the model off the GPU entirely. 8K holds the median 3.1KB file with
# room to spare and keeps the 12B at 100% GPU.
NUM_CTX = 8192
# 800 was too tight: schema-constrained output on defect-heavy files got cut
# mid-object, producing unparseable JSON. 2000 still truncated a defect-dense
# file at char 6883, so this is 4000. Truncation shows up as a JSONDecodeError
# rather than a partial result, so it is always worth paying for headroom.
NUM_PREDICT = 4000

# The server can die mid-sweep (it was OOM-killed once at file 549 of 605),
# after which every subsequent request fails instantly with connection-refused.
RETRIES = 4
RETRY_BACKOFF = (5, 15, 45, 90)

# `kind` is an enum, not a free string. Prose instructions did not hold: told
# to use only the listed kinds, the model stopped emitting
# "potential_security_risk" and emitted "security_risk", "logic_error" and
# "performance_issue" instead -- renaming the invention rather than dropping it.
# The schema is enforced at the decoder, so it cannot route around this.
DEFECT_KINDS = [
    "undefined_register",
    "name_collision",
    "dropped_call",
    "dead_store",
    "duplicate_call",
    "truncated_body",
    "dropped_operator",
    "branch_local_discarded",
]

SCHEMA = {
    "type": "object",
    "properties": {
        "verdict": {"type": "string", "enum": ["clean", "suspect", "broken"]},
        "defects": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "line": {"type": "integer"},
                    "snippet": {"type": "string"},
                    "kind": {"type": "string", "enum": DEFECT_KINDS},
                    "why": {"type": "string"},
                },
                "required": ["line", "snippet", "kind", "why"],
            },
        },
    },
    "required": ["verdict", "defects"],
}

# The "do not speculate about Roblox API semantics" line is not boilerplate:
# both the 26B and 30B models invented a bug by claiming child-instance lookups
# like `game.ReplicatedFirst.PlaceType` were invalid property accesses.
PROMPT_HEAD = """You are auditing output from a Luau decompiler for defects.

Known defect kinds:
  undefined_register - a bare vN (e.g. `v7`) referenced but never assigned
  name_collision     - one debug name reused for unrelated values, often with numeric suffixes
  dropped_call       - a call whose result is referenced but the call itself is missing
  dead_store         - assigned and never read
  duplicate_call     - the same call emitted twice into different locals
  truncated_body     - function body missing, or a bare `return {}` stub

Your ONLY job is finding DECOMPILER defects: places where the decompiler lost
information that the original bytecode contained. You are NOT reviewing the
game's design, its security, or its code style. If a line would be normal in
hand-written Luau, it is not a defect no matter how it looks.

NOT DEFECTS. Do not report any of these. Each was invented by a model on this
corpus and wasted review time:
  - Hex literals (`0x1E`, `0x5014`). Decompilers emit numeric constants in hex.
    This is normal output, not obfuscation and not a suspicious value.
  - `require()` of a module from ReplicatedStorage, ServerStorage or script.
    This is ordinary Roblox module structure.
  - Service calls: `game:GetService("RunService")`, ContextActionService,
    TweenService, CollectionService and friends.
  - Child-instance lookups: `game.ReplicatedFirst.PlaceType`, `script.IsHiveHub`,
    `arg2.Totals.Collectibles`. Valid Roblox, not property errors.
  - Anything you would label security_risk, obfuscated_constant,
    suspicious_value, dynamic_type_checking or external_module_dependency.
    None of those are decompiler defects. Do not use those kinds.
  - Magic numbers, long tables of constants, or values that look like
    timestamps. Game data is allowed to be game data.
  - Missing type annotations, missing error handling, or naming style.

Rules:
  - Only report a defect you can point at with an exact line from the file.
  - Use ONLY the kinds listed above. Do not invent new kind names.
  - Misleading variable names are name_collision, not broken logic, when the
    surrounding code is structurally correct.
  - If you are unsure whether something is a decompiler defect or just how the
    game was written, leave it out. A missed defect costs less than a false one.
  - verdict: "broken" if any defect makes the code non-runnable or wrong;
    "suspect" if only naming/redundancy issues; "clean" if nothing concrete.

FILE:
"""


def audit(path: Path, model: str, timeout: int) -> dict:
    """Send one file to the model; return a result record."""
    text = path.read_text(encoding="utf-8", errors="replace")
    numbered = "\n".join(f"{i}: {ln}" for i, ln in enumerate(text.splitlines(), 1))

    # num_ctx is a local-inference setting: it sizes the KV cache in VRAM.
    # Sending it to a cloud model does not help and actively truncates the
    # prompt, so cloud requests omit it entirely.
    is_cloud = model.endswith("-cloud")
    options = {"num_predict": NUM_PREDICT, "temperature": 0}
    if not is_cloud:
        options["num_ctx"] = NUM_CTX

    payload = {
        "model": model,
        "prompt": PROMPT_HEAD + numbered,
        "stream": False,
        "format": SCHEMA,
        "options": options,
    }
    # `think` is gemma-family only. It crashes qwen3-coder outright, and cloud
    # models reject or ignore it.
    if not is_cloud and model.startswith(("gemma4", "local-coder")):
        payload["think"] = False
    req = urllib.request.Request(
        OLLAMA_URL,
        data=json.dumps(payload).encode("utf-8"),
        headers={"Content-Type": "application/json"},
    )

    t0 = time.time()
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        body = json.load(resp)
    elapsed = time.time() - t0

    parsed = json.loads(body["response"])
    prompt_tokens = body.get("prompt_eval_count", 0)
    gen_tokens = body.get("eval_count", 0)

    defects = parsed.get("defects", [])
    return {
        "file": str(path),
        "verdict": enforce_verdict(parsed.get("verdict", "unknown"), defects),
        "model_verdict": parsed.get("verdict", "unknown"),
        "defects": defects,
        "prompt_tokens": prompt_tokens,
        "gen_tokens": gen_tokens,
        "seconds": round(elapsed, 2),
    }


# Kinds the PROMPT itself defines as cosmetic: "Misleading variable names are
# name_collision, not broken logic, when the surrounding code is structurally
# correct", and a dead store is a value nothing reads. The prompt then says
# verdict is "suspect" if only naming/redundancy issues.
#
# The model does not obey that. Measured on sweep_full1143.jsonl: 99 of 246
# "broken" files carried NOTHING but these kinds - 40% of the broken set graded
# as non-runnable on the strength of an ugly name. The kind enum is already
# enforced at the decoder rather than trusted to the prompt; the verdict rule
# needs the same treatment, for the same reason.
#
# This DOWNGRADES ONLY, and only when every defect is cosmetic. It never
# silences a finding: the defects stay in the record with their lines intact,
# and a single correctness kind anywhere in the file keeps "broken".
#
# dead_store WAS in this set and has been REMOVED. It looks like redundancy and
# is not. In 734_StatModifiers_BaseConversionRate the emitted body is
#
#     local Op3 = Op .. " Convert Amount"
#     if arg2 and arg2.Color then
#         local Op4 = ((Op3 .. " (") .. arg2.Color) .. " Bees)"   -- dropped
#     end
#     return Op3                                    -- returns the short string
#
# where the original reassigned `Op3`. The dead local IS the visible symptom of
# a lost reassignment, and the function returns the wrong value. A dead store is
# therefore a CORRECTNESS finding until proven otherwise, and 18 files were
# wrongly downgraded while it sat here.
COSMETIC_KINDS = {"name_collision"}


def enforce_verdict(verdict: str, defects: list) -> str:
    """Apply the prompt's own verdict rule instead of trusting the model to."""
    if verdict != "broken" or not defects:
        return verdict
    kinds = {d.get("kind") for d in defects if isinstance(d, dict)}
    if kinds and kinds <= COSMETIC_KINDS:
        return "suspect"
    return verdict


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("indir", help="directory of .lua files to sweep")
    ap.add_argument("--out", default="sweep.jsonl")
    ap.add_argument("--model", default=DEFAULT_MODEL)
    ap.add_argument("--limit", type=int, default=0, help="0 = no limit")
    ap.add_argument("--timeout", type=int, default=300)
    ap.add_argument("--max-bytes", type=int, default=60000, help="skip larger files")
    # 4 measured as the knee on a 5060 Ti: 1x -> 1.26x -> 1.47x -> 1.55x for
    # 1/2/4/8 workers. Past 4 the gain is ~6% for double the KV cache.
    ap.add_argument("--workers", type=int, default=4, help="concurrent requests")
    args = ap.parse_args()

    out_path = Path(args.out)
    done: set[str] = set()
    if out_path.exists():
        # Drop prior error records and rewrite, so a resume retries the
        # failures instead of counting them as done.
        kept = []
        errors = 0
        for line in out_path.read_text(encoding="utf-8").splitlines():
            try:
                rec = json.loads(line)
            except json.JSONDecodeError:
                continue
            if rec.get("verdict") == "error":
                errors += 1
                continue
            kept.append(line)
            done.add(rec["file"])
        if errors:
            out_path.write_text("\n".join(kept) + "\n", encoding="utf-8")
        print(f"resuming: {len(done)} done, {errors} prior error(s) will be retried")

    files = sorted(p for p in Path(args.indir).rglob("*.lua") if p.is_file())
    todo = [p for p in files if str(p) not in done and p.stat().st_size <= args.max_bytes]
    skipped_big = sum(1 for p in files if p.stat().st_size > args.max_bytes)
    if skipped_big:
        print(f"skipping {skipped_big} file(s) over {args.max_bytes} bytes")
    if args.limit:
        todo = todo[: args.limit]

    print(f"{len(todo)} file(s) to process with {args.model}")
    if not todo:
        return 0

    t_start = time.time()
    counts = {"clean": 0, "suspect": 0, "broken": 0, "error": 0}
    tok_in = tok_out = 0
    write_lock = threading.Lock()
    progress = {"n": 0}

    def process(path: Path) -> dict:
        """Audit one file, retrying transient connection failures."""
        for attempt in range(RETRIES):
            try:
                return audit(path, args.model, args.timeout)
            except (urllib.error.URLError, OSError) as exc:
                # Usually the server being down; wait for it to come back.
                if attempt == RETRIES - 1:
                    return {"file": str(path), "verdict": "error",
                            "error": f"{type(exc).__name__}: {exc}"}
                time.sleep(RETRY_BACKOFF[attempt])
            except (json.JSONDecodeError, KeyError) as exc:
                # Malformed model output; retrying rarely helps.
                return {"file": str(path), "verdict": "error",
                        "error": f"{type(exc).__name__}: {exc}"}
        return {"file": str(path), "verdict": "error", "error": "retries exhausted"}

    with out_path.open("a", encoding="utf-8") as fh:
        with ThreadPoolExecutor(max_workers=args.workers) as pool:
            futures = {pool.submit(process, p): p for p in todo}
            for fut in as_completed(futures):
                path = futures[fut]
                rec = fut.result()

                # Workers finish out of order, so the counters, the JSONL
                # append and the progress counter all have to be serialised.
                with write_lock:
                    progress["n"] += 1
                    i = progress["n"]
                    counts[rec["verdict"]] = counts.get(rec["verdict"], 0) + 1
                    tok_in += rec.get("prompt_tokens", 0)
                    tok_out += rec.get("gen_tokens", 0)

                    fh.write(json.dumps(rec) + "\n")
                    fh.flush()

                    elapsed = time.time() - t_start
                    rate = i / elapsed if elapsed else 0
                    eta = (len(todo) - i) / rate if rate else 0
                    print(
                        f"[{i}/{len(todo)}] {rec['verdict']:<7} "
                        f"{len(rec.get('defects', [])):>2}d  {path.name[:44]:<44} "
                        f"{rate*60:5.1f} files/min  ETA {eta/60:5.1f}m",
                        flush=True,
                    )

    elapsed = time.time() - t_start
    print("\n=== SUMMARY ===")
    for k, v in counts.items():
        if v:
            print(f"  {k:<8} {v}")
    print(f"  elapsed   {elapsed/60:.1f} min")
    print(f"  tokens    {tok_in} in / {tok_out} out")
    if elapsed:
        print(f"  prefill   {tok_in/elapsed:.0f} tok/s effective")
    print(f"  output    {out_path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
