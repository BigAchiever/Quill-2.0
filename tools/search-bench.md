# cognee retrieval-strategy evidence — 2026-07-03

Self-hosted sidecar (`cognee/cognee:main`, kuzu + lancedb), a local OpenAI-compatible LLM,
Ollama `nomic-embed-text` embeddings, Quill's real graph.
Gate for shipping a strategy in the recall hot path: **grounded results AND well inside
recall's 10s UX budget** (a keypress is never held hostage by memory).

Measured with `curl` against a quiet sidecar (no pipelines running):

| strategy | latency | result | gate |
|---|---|---|---|
| GRAPH_COMPLETION (default) | **>40s** (timed out at cap) | n/a — never returned | **FAIL** |
| CHUNKS | **3.1s cold / 2.3s warm** | 6 relevant excerpts with dataset provenance | **PASS** |

Consequence (shipped in `retrieve.rs::RETRIEVAL_STRATEGY`): both recall lanes use
`CHUNKS`. GRAPH_COMPLETION runs a *second* LLM inside cognee to write an answer that
Quill's own LLM would re-synthesize anyway — on a self-hosted LLM that costs 40+ seconds
and duplicates the synthesis layer. With CHUNKS, **cognee retrieves, Quill's LLM
synthesizes**: one LLM pass, sub-3-second memory.

Also explains history: every "recall unavailable — drafting from screen only" line in
earlier live sessions was GRAPH_COMPLETION silently exceeding the budget.

Still gated OFF pending measurement on a temporally-cognified graph: `TEMPORAL` routing
for time-scoped queries (`looks_time_scoped` detector is wired and tested, the flip is
one constant). The full multi-strategy matrix lives in `tools/search-bench.py`.
