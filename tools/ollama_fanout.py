#!/usr/bin/env python3
"""Run a batch of prompts across Ollama models concurrently, local and cloud.

Generalises the driver inside sweep_local.py. That one is welded to one task
(triage a Luau file) and one local model. This takes an arbitrary JSONL of
tasks and fans them out, so the same machinery serves corpus triage, research
merging, extraction, or anything else that is many-independent-prompts shaped.

    # one prompt per line of a JSONL, each {"id": ..., "prompt": ...}
    python tools/ollama_fanout.py tasks.jsonl --out results.jsonl
    python tools/ollama_fanout.py tasks.jsonl --model gpt-oss:120b-cloud -j 12

    # spread the same tasks across several models (round-robin)
    python tools/ollama_fanout.py tasks.jsonl --model gpt-oss:120b-cloud \
        --model qwen3.5:397b-cloud --model local-coder

Resumes automatically: ids already present in the output JSONL are skipped.

Cloud models are proxied by the local daemon using the signed key in
~/.ollama, so an Ollama Pro subscription needs no API key here and no separate
endpoint. Their weights never touch local VRAM, which is why a 1.6TB model and
a 7.6GB one can be driven from the same pool.
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

OLLAMA_CHAT = "http://localhost:11434/api/chat"

# Local models: a 256K default context builds a KV cache large enough to push
# the model half into system RAM. Capping it took gemma4:26b from 19.3 to 42.1
# tok/s. Cloud models are served remotely, so the cap is neither needed nor
# helpful there — it would just truncate a long prompt.
LOCAL_NUM_CTX = 8192

RETRIES = 4
RETRY_BACKOFF = (5, 15, 45, 90)

# Per-family request quirks, learned the hard way:
#   gemma  - without think:false the reply comes back EMPTY, the whole token
#            budget spent on reasoning traces that are then discarded.
#   qwen   - sending `think` at all does not 400, it kills llama-server with
#            0xc0000409 (stack buffer overrun), taking down every loaded model.
# Anything not matched gets no `think` field, which is the safe default.
THINK_FALSE = ("gemma", "local-coder")
THINK_NEVER = ("qwen",)


def is_cloud(model: str) -> bool:
    return model.endswith("-cloud") or ":cloud" in model


def build_body(model: str, task: dict, num_predict: int, num_ctx: int | None) -> dict:
    messages = []
    system = task.get("system")
    if system:
        messages.append({"role": "system", "content": system})
    messages.append({"role": "user", "content": task["prompt"]})

    options = {"num_predict": num_predict}
    # Only cap context for local models; see LOCAL_NUM_CTX above.
    if not is_cloud(model):
        options["num_ctx"] = num_ctx or LOCAL_NUM_CTX
    options.update(task.get("options") or {})

    body = {"model": model, "messages": messages, "stream": False, "options": options}

    low = model.lower()
    if any(k in low for k in THINK_NEVER):
        pass
    elif any(k in low for k in THINK_FALSE):
        body["think"] = False

    schema = task.get("schema")
    if schema:
        body["format"] = schema

    return body


def call(model: str, task: dict, num_predict: int, num_ctx: int | None, timeout: int) -> dict:
    """One request, with retries. Returns a result record, never raises."""
    body = json.dumps(build_body(model, task, num_predict, num_ctx)).encode("utf-8")
    last = ""
    for attempt in range(RETRIES):
        started = time.time()
        try:
            req = urllib.request.Request(
                OLLAMA_CHAT, data=body, headers={"Content-Type": "application/json"}
            )
            with urllib.request.urlopen(req, timeout=timeout) as fh:
                payload = json.load(fh)
            content = (payload.get("message") or {}).get("content", "")
            if not content.strip():
                # An empty body is the gemma think-trace failure, not success.
                last = "empty response"
                raise ValueError(last)
            out = {
                "id": task["id"],
                "model": model,
                "ok": True,
                "content": content,
                "elapsed": round(time.time() - started, 2),
                "eval_count": payload.get("eval_count"),
            }
            if task.get("schema"):
                try:
                    out["parsed"] = json.loads(content)
                except json.JSONDecodeError as exc:
                    # Schema-constrained output truncated mid-object. Keep the
                    # raw text so the caller can salvage or re-run it.
                    out["ok"] = False
                    out["error"] = "json decode: %s" % exc
            return out
        except Exception as exc:  # noqa: BLE001 - report every failure shape
            last = "%s: %s" % (type(exc).__name__, exc)
            if attempt < RETRIES - 1:
                time.sleep(RETRY_BACKOFF[attempt])
    return {"id": task["id"], "model": model, "ok": False, "error": last}


def load_tasks(path: Path) -> list[dict]:
    tasks = []
    for lineno, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        line = line.strip()
        if not line:
            continue
        try:
            task = json.loads(line)
        except json.JSONDecodeError as exc:
            sys.exit("%s:%d is not valid JSON: %s" % (path, lineno, exc))
        if "prompt" not in task:
            sys.exit("%s:%d has no 'prompt'" % (path, lineno))
        task.setdefault("id", str(lineno))
        tasks.append(task)
    return tasks


def done_ids(path: Path) -> set[str]:
    if not path.exists():
        return set()
    ids = set()
    for line in path.read_text(encoding="utf-8").splitlines():
        try:
            rec = json.loads(line)
        except json.JSONDecodeError:
            continue
        if rec.get("ok"):
            ids.add(str(rec.get("id")))
    return ids


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("tasks", type=Path, help="JSONL, one {id,prompt,...} per line")
    ap.add_argument("--out", type=Path, default=Path("fanout_results.jsonl"))
    ap.add_argument(
        "--model", action="append", default=[],
        help="repeatable; tasks are spread round-robin across the models given",
    )
    ap.add_argument("-j", "--jobs", type=int, default=8, help="concurrent requests")
    ap.add_argument("--num-predict", type=int, default=4000)
    ap.add_argument("--num-ctx", type=int, default=None, help="local models only")
    ap.add_argument("--timeout", type=int, default=900)
    ap.add_argument("--limit", type=int, default=0)
    ap.add_argument("--no-resume", action="store_true")
    args = ap.parse_args()

    models = args.model or ["gpt-oss:120b-cloud"]
    tasks = load_tasks(args.tasks)

    if not args.no_resume:
        already = done_ids(args.out)
        if already:
            before = len(tasks)
            tasks = [t for t in tasks if str(t["id"]) not in already]
            print("resuming: %d/%d already done" % (before - len(tasks), before))
    if args.limit:
        tasks = tasks[: args.limit]

    if not tasks:
        print("nothing to do")
        return 0

    print(
        "%d tasks across %d model(s) %s, %d concurrent"
        % (len(tasks), len(models), ", ".join(models), args.jobs)
    )

    lock = threading.Lock()
    counts = {"ok": 0, "fail": 0}
    started = time.time()

    with args.out.open("a", encoding="utf-8") as sink:
        with ThreadPoolExecutor(max_workers=args.jobs) as pool:
            futures = {
                pool.submit(
                    call, models[i % len(models)], task,
                    args.num_predict, args.num_ctx, args.timeout,
                ): task
                for i, task in enumerate(tasks)
            }
            for future in as_completed(futures):
                rec = future.result()
                with lock:
                    sink.write(json.dumps(rec, ensure_ascii=False) + "\n")
                    sink.flush()
                    counts["ok" if rec.get("ok") else "fail"] += 1
                    n = counts["ok"] + counts["fail"]
                    tag = "ok " if rec.get("ok") else "FAIL"
                    print(
                        "[%3d/%3d] %s %-22s %-24s %s"
                        % (
                            n, len(tasks), tag, str(rec["id"])[:22], rec["model"][:24],
                            ("%.1fs" % rec["elapsed"]) if rec.get("ok")
                            else rec.get("error", "")[:60],
                        ),
                        flush=True,
                    )

    wall = time.time() - started
    print(
        "\n%d ok, %d failed in %.0fs (%.1f tasks/min) -> %s"
        % (counts["ok"], counts["fail"], wall, 60 * (counts["ok"] + counts["fail"]) / wall, args.out)
    )
    return 0 if counts["fail"] == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
