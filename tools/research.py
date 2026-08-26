#!/usr/bin/env python3
"""research - autonomous "research a topic -> cited report", fully local. For the model's Bash tool.

    python tools/research.py "<topic or question>" [iterations]

Wraps local-deep-research (LearningCircuit/local-deep-research): it runs a search -> read -> reason
loop against a LOCAL Ollama model and a keyless search engine, and returns a synthesized answer
with citations. Everything stays on the machine (the searches hit the web; the reasoning is local).

Model: defaults to the research-tuned uncensored model (RESEARCH_MODEL), overridable. Search:
DuckDuckGo via the ddgs library (keyless). Point it at a local SearXNG instead by setting
LDR_SEARCH_TOOL=searxng and running the compose in tools/searxng/.

This is heavier and slower than websearch.py + webget.py (many LLM calls) - use it for a real
multi-source report, not a quick lookup.
"""
import os
import sys
import warnings

warnings.filterwarnings("ignore")

OLLAMA_URL = os.environ.get("OLLAMA_URL", "http://localhost:11434")
RESEARCH_MODEL = os.environ.get(
    "RESEARCH_MODEL",
    "hf.co/mradermacher/Qwen3-30B-A3B-Instruct-2507-Heretic-i1-GGUF:Q4_K_M")
SEARCH_TOOL = os.environ.get("LDR_SEARCH_TOOL", "duckduckgo")


def run(query, iterations):
    from local_deep_research.api import quick_summary
    override = {
        "llm.provider": "ollama",
        "llm.model": RESEARCH_MODEL,
        "llm.ollama.url": OLLAMA_URL,
        "llm.temperature": 0.2,
        "search.tool": SEARCH_TOOL,
        "search.iterations": iterations,
        "search.questions_per_iteration": 2,
        "search.max_results": 6,
    }
    return quick_summary(
        query=query,
        provider="ollama",
        temperature=0.2,
        max_search_results=6,
        settings_override=override,
    )


def main():
    if len(sys.argv) < 2:
        print("usage: research.py \"<topic>\" [iterations]"); return 2
    query = sys.argv[1]
    iterations = int(sys.argv[2]) if len(sys.argv) > 2 else 1
    print("researching (model=%s, search=%s, iters=%d) ...\n"
          % (RESEARCH_MODEL.split("/")[-1][:40], SEARCH_TOOL, iterations), file=sys.stderr)
    try:
        result = run(query, iterations)
    except Exception as e:
        import traceback
        print("RESEARCH ERROR: %s: %s" % (type(e).__name__, e))
        traceback.print_exc()
        return 1
    if not isinstance(result, dict):
        print(result); return 0
    print("=" * 60)
    print(result.get("summary") or result.get("answer") or "(no summary)")
    sources = result.get("sources") or result.get("all_links_of_system") or []
    if sources:
        print("\n--- SOURCES ---")
        for s in sources[:12]:
            if isinstance(s, dict):
                print(" - %s  %s" % (s.get("title", "")[:60], s.get("link") or s.get("url") or ""))
            else:
                print(" - %s" % s)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
