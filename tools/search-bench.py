#!/usr/bin/env python3
"""Benchmark cognee search strategies on Quill's real graph.

Every retrieval knob in the app must earn its place here first: a strategy ships only if
it (a) returns a non-empty, grounded answer on a self-hosted graph (a local LLM + Ollama
embeddings — cloud-tuned defaults don't transfer), and (b) fits recall's 10s UX budget
(gate: p50 <= 8s). Output doubles as the README's design-decision evidence.

Run:  python3 tools/search-bench.py [--out tools/search-bench.md]
"""

import argparse
import json
import statistics
import subprocess
import time
from datetime import datetime

BASE = "http://127.0.0.1:8765/api/v1"
BUDGET_P50_SECS = 8.0
CALL_TIMEOUT = 45  # observe true latency well past budget; only cut off real hangs

# The Quill recall persona proposed for the app. Benched here to measure
# NO_MATCH obedience instead of assuming it — the model has earned our distrust.
QUILL_PROMPT = (
    "You are the user's ambient memory. Answer with concrete facts — names, dates, "
    "decisions, commitments. If the provided context does not answer the question, "
    "reply exactly NO_MATCH."
)

# (strategy, systemPrompt or None). GRAPH_COMPLETION is the shipping baseline.
ARMS = [
    ("GRAPH_COMPLETION", None),
    ("GRAPH_COMPLETION", QUILL_PROMPT),
    ("RAG_COMPLETION", None),
    ("CHUNKS", None),
    ("HYBRID_COMPLETION", None),
    ("TEMPORAL", None),
    ("GRAPH_COMPLETION_COT", None),
    ("FEELING_LUCKY", None),
]

# Example queries, including a time-scoped query and one
# deliberate no-answer probe (measures stub/NO_MATCH behavior).
QUERIES = [
    ("time-scoped query",
     "What changed in the legal assistant project since last month?"),
    ("person-fact (Outlook thread)",
     "What did Sam confirm about the meeting?"),
    ("cross-surface synthesis",
     "What is the Acme legal assistant project and what work was done on it?"),
    ("surface-fact (LinkedIn)",
     "Who commented on my recent LinkedIn post and what did they say?"),
    ("no-answer probe (nothing in graph)",
     "What is my grandmother's secret lasagna recipe?"),
]


def search(strategy: str, query: str, system_prompt: str | None) -> tuple[float, str, bool]:
    """One POST /search. Returns (secs, answer-or-error, ok)."""
    body = {"searchType": strategy, "query": query, "topK": 6}
    if system_prompt:
        body["systemPrompt"] = system_prompt
    t0 = time.time()
    p = subprocess.run(
        ["curl", "-s", "-m", str(CALL_TIMEOUT), "-X", "POST", f"{BASE}/search",
         "-H", "Content-Type: application/json", "-d", json.dumps(body)],
        capture_output=True, text=True,
    )
    secs = time.time() - t0
    if p.returncode != 0:
        return secs, f"(curl failed rc={p.returncode} — timeout/hang)", False
    try:
        val = json.loads(p.stdout)
    except Exception:
        return secs, f"(non-JSON: {p.stdout[:120]})", False
    return secs, flatten(val), True


def flatten(val) -> str:
    """Search responses vary by strategy (completion string vs chunk lists) — normalize."""
    if isinstance(val, str):
        return val.strip()
    if isinstance(val, list):
        parts = [flatten(v) for v in val]
        return " | ".join(p for p in parts if p)[:600]
    if isinstance(val, dict):
        for k in ("answer", "text", "result", "search_result", "content", "detail"):
            if val.get(k):
                return flatten(val[k])
        return json.dumps(val)[:300]
    return str(val)


def clip(s: str, n: int = 160) -> str:
    s = " ".join(s.split())
    return s if len(s) <= n else s[: n - 1] + "…"


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default="tools/search-bench.md")
    args = ap.parse_args()

    lines = [
        f"# cognee search-strategy bench — {datetime.now():%Y-%m-%d %H:%M}",
        "",
        "Self-hosted sidecar, a local LLM + Ollama embeddings, Quill's real graph.",
        f"Gate: non-empty grounded answer AND p50 ≤ {BUDGET_P50_SECS:.0f}s "
        "(recall's 10s UX budget, minus headroom).",
        "",
        "| strategy | query | secs | answer |",
        "|---|---|---:|---|",
    ]
    lat: dict[str, list[float]] = {}
    for strategy, prompt in ARMS:
        arm = strategy + (" +quill-prompt" if prompt else "")
        print(f"\n=== {arm} ===", flush=True)
        for label, q in QUERIES:
            secs, ans, ok = search(strategy, q, prompt)
            lat.setdefault(arm, []).append(secs)
            print(f"  [{secs:5.1f}s] {label}: {clip(ans, 110)}", flush=True)
            lines.append(f"| {arm} | {label} | {secs:.1f} | {clip(ans)} |")

    lines += ["", "## Latency summary", "", "| strategy | p50 | max | gate (p50 ≤ 8s) |", "|---|---:|---:|---|"]
    print("\n=== latency summary ===")
    for arm, xs in lat.items():
        p50, mx = statistics.median(xs), max(xs)
        verdict = "PASS" if p50 <= BUDGET_P50_SECS else "FAIL"
        print(f"  {arm}: p50={p50:.1f}s max={mx:.1f}s {verdict}")
        lines.append(f"| {arm} | {p50:.1f}s | {mx:.1f}s | {verdict} |")

    with open(args.out, "w") as f:
        f.write("\n".join(lines) + "\n")
    print(f"\nwrote {args.out}")


if __name__ == "__main__":
    main()
