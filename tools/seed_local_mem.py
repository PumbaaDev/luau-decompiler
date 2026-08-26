#!/usr/bin/env python3
"""Seed local_mem with the facts measured in this project, one atomic fact each.

Includes a deliberately STALE hardware fact plus the current one that supersedes
it, so the supersession path is exercised rather than assumed.

    python tools/seed_local_mem.py
"""
from __future__ import annotations

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from mem import FACTS, dump_fact, build_index

D = "2026-08-23"

SEED = [
    # --- superseded on purpose, to prove the mechanism ---
    dict(id="hardware-old", title="Jep's PC (SUPERSEDED)", type="fact", authority="measured",
         aliases=["i3-10105F"], tags=["hardware"], updated="2026-06-26",
         body="Jep's PC is an Intel i3-10105F (4 cores / 8 threads) with 16GB RAM and an "
              "RTX 5060 Ti. The CPU is the bottleneck."),

    dict(id="hardware", title="Jep's PC specs", type="fact", authority="measured",
         aliases=["i7-11700F", "RTX 5060 Ti", "GTX 1650"], tags=["hardware"], updated=D,
         supersedes=["hardware-old"],
         body="Jep's PC: Intel i7-11700F (8 cores / 16 threads), 31.8GB DDR4-3200 in 4x8GB, "
              "RTX 5060 Ti 16GB as GPU 0 and a GTX 1650 4GB as GPU 1, Windows 11 Home. "
              "Upgraded from an i3-10105F + 16GB on 20 Aug 2026, so the old CPU bottleneck is gone."),

    dict(id="vram-ceiling", title="VRAM spill ceiling", type="fact", authority="measured",
         aliases=["14.2GB", "spill"], tags=["hardware", "models"], updated=D,
         body="Anything needing more than about 14.2GB of VRAM spills into system RAM on the "
              "RTX 5060 Ti 16GB. That is the physical card limit minus Windows. Dense models "
              "collapse when they spill: Qwen2.5-Coder-32B-abliterated measured 3.1 tok/s."),

    dict(id="moe-kv-cache", title="MoE tolerates weight spill, not KV cache spill", type="fact",
         authority="measured", aliases=["num_ctx", "KV cache"], tags=["models"], updated=D,
         body="MoE models tolerate spilling in their WEIGHTS because few params are active per "
              "token, but NOT in the KV cache. Qwen3-Coder-30B-A3B-abliterated (18GB) measured "
              "42.4 tok/s at num_ctx 8192, 32.0 at 16384, 16.5 at 32768, and only 5.0 tok/s at "
              "65536 where it goes 45GB resident. Always benchmark at the num_ctx you will use."),

    dict(id="uncensored-coder", title="uncensored-coder model", type="reference",
         authority="measured", aliases=["Qwen3-Coder-30B-A3B", "uncensored-coder"],
         tags=["models"], updated=D,
         body="uncensored-coder is an Ollama alias for "
              "hf.co/mradermacher/Qwen3-Coder-30B-A3B-Instruct-Heretic-i1-GGUF:Q4_K_M, "
              "18GB, MoE with 30B total and 3B active params, running at num_ctx 32768. Its "
              "system prompt costs 1957 tokens per call, leaving 14427 usable, about 985 lines "
              "of Lua per pass. Models live on E:\\ollama-models."),

    dict(id="model-confabulates", title="The local model invents facts it does not have",
         type="fact", authority="measured", aliases=["confabulation", "hallucination"],
         tags=["models", "risk"], updated=D,
         body="MEASURED failure: asked about a non-existent Roblox API method, uncensored-coder "
              "invented a full return type and parameter list, reasoning itself into the "
              "hallucination, even though its system prompt told it to say 'I do not know'. "
              "Abliteration removes the refusal reflex and weakens the uncertainty reflex with it. "
              "Never trust it to recall an API, a number, or whether something exists. It is "
              "reliable at operating on input you give it, not at remembering external fact."),

    dict(id="think-crash", title="Never send `think` to qwen3-coder", type="procedure",
         authority="measured", aliases=["think", "0xc0000409"], tags=["models", "gotcha"],
         updated=D,
         body="qwen3-coder CRASHES llama-server with 0xc0000409 if the `think` field is sent at "
              "all, killing every loaded model. Omit the field entirely for that family. gemma4 "
              "is the opposite: it returns an EMPTY response without think:false."),

    dict(id="corpus-union1324", title="The measurement corpus", type="reference",
         authority="measured", aliases=["union1324", "target/union1324"], tags=["decompiler"],
         updated="2026-08-22",
         body="The real corpus is target/union1324: 1,324 unique hash-deduped .bin files, "
              "decompiling to 1,323 .lua outputs. Do NOT measure on bc/, which is a 628-file "
              "subset and skewed. Semantic quality measured 1,305/1,323 clean, 98.6%."),

    dict(id="for-loop-bindings", title="Lua for-loop variables are declarations", type="fact",
         authority="measured", aliases=["find_undefined_registers", "false positive"],
         tags=["decompiler", "audit"], updated=D,
         body="An undefined-register checker that only recognises `local vN` reports every "
              "`for k, v3 in pairs(t)` loop variable as a defect. MEASURED on union1324: 172 "
              "flags of which 169 (98%) were for-loop bindings, all false positives. After "
              "counting for-loop control variables and function parameters as bindings, the "
              "corpus yields 3 genuine used-never-declared registers, all hand-verified."),

    dict(id="harness-lies", title="Suspect the harness before the model", type="procedure",
         authority="measured", aliases=["extract_code", "assert message"],
         tags=["method"], updated=D,
         body="Four separate measuring instruments produced false results in one session: an "
              "assert with no message recorded a bare 'WRONG:' and hid the real value; num_ctx "
              "pinned at 8192 made a 5 tok/s model look fast; extract_code taking the last "
              "fenced block scored a correct function 0/4 by executing a worked example; and a "
              "benchmark's planted answer key collided with its own generated data, scoring two "
              "wrong answers as perfect. Every failure made a wrong thing look right. When a "
              "capable model posts a strange result, check the instrument first."),

    dict(id="ox-alpha", title="Ox Alpha cannot run locally", type="reference",
         authority="claimed", aliases=["Ox Alpha", "stealth/ox-alpha"], tags=["models"],
         updated=D,
         body="Ox Alpha is an anonymous stealth model on OpenRouter (id stealth/ox-alpha), "
              "closed weights, API only, so it cannot be downloaded or run locally. Its "
              "'beats Claude Fable 5 at coding' claim is one developer's run of ten tasks, not "
              "an audited leaderboard. Hugging Face repos named ox-alpha are name-squats."),

    dict(id="clean-run-rule", title="Force a clean run when measuring decompiler quality",
         type="procedure", authority="measured", aliases=["resumed=0"], tags=["decompiler"],
         updated="2026-08-22",
         body="Empty the output directory before measuring and confirm resumed=0, otherwise the "
              "numbers mix old-opmap files and mean nothing. Also: `disassemble` needs BOTH "
              "--opmap AND --opmap-db or its output is wrong in a way that looks plausible."),
]


def main() -> int:
    FACTS.mkdir(parents=True, exist_ok=True)
    for fm in SEED:
        fm.setdefault("supersedes", [])
        fm.setdefault("aliases", [])
        fm.setdefault("tags", [])
        p = FACTS / (fm["id"] + ".md")
        p.write_text(dump_fact(fm), encoding="utf-8")
    print("seeded %d facts into %s" % (len(SEED), FACTS))
    return build_index()


if __name__ == "__main__":
    raise SystemExit(main())
