# Quill — Setup

Quill is a **local-first, ambient AI writing assistant**. It quietly builds a private
knowledge graph of your work (via a self-hosted **cognee** memory engine) and uses it to
draft and improve your writing *inside the apps you already use* — all on your own machine.

> **Requirements:** a **Mac** (Apple Silicon recommended) · **Docker Desktop** · and **one**
> of: an **OpenAI API key** (fastest) *or* **[Ollama](https://ollama.com)** installed (fully
> private, nothing leaves your machine). ~10 minutes.

---

## 1. Get the code

```bash
git clone <this-repo> quill && cd quill
```

## 2. Configure the memory engine (cognee)

```bash
cp cognee-sidecar/.env.example cognee-sidecar/.env
```

Open `cognee-sidecar/.env` and uncomment **one** LLM block:

- **Quick start (OpenAI):** it's already the default — just paste your key into `LLM_API_KEY`
  and `EMBEDDING_API_KEY`.
- **Private (Ollama):** comment out block (A), uncomment blocks (B) for both LLM and
  embeddings, then pull the models:
  ```bash
  ollama pull qwen2.5:14b
  ollama pull nomic-embed-text
  ```

## 3. Start the memory engine

```bash
docker compose up -d
# verify (should return 200 within a second once warm):
curl -s -o /dev/null -w "cognee: HTTP %{http_code}\n" http://127.0.0.1:8765/health
```

## 4. Configure Quill's own LLM

Quill's drafting uses the **same** provider. Create its config where the packaged app reads it:

```bash
mkdir -p ~/Library/Application\ Support/com.danishalisiddiqui.quill
cp quill/src-tauri/.env.example ~/Library/Application\ Support/com.danishalisiddiqui.quill/.env
```

Edit that file and uncomment the matching block (OpenAI = default; for Ollama use the
**`localhost`** URLs — Quill runs on the host, not in Docker).

## 5. Install the app

1. Open **`Quill.dmg`** and drag **Quill** to Applications.
2. It's **unsigned** (development build), so the first launch is blocked by Gatekeeper.
   **Right-click the app → Open → Open** to bypass it (only needed once).

## 6. Grant Accessibility permission

Quill reads the focused text field and types drafts back, which needs macOS Accessibility:

> **System Settings → Privacy & Security → Accessibility → enable Quill**, then relaunch.

Without this, ambient capture and inline drafting won't work.

---

## Using it

- Quill captures ambiently as you work; the **constellation** (its knowledge graph) fills in
  over the first few minutes as cognee ingests and builds it.
- Press **Right‑Option (⌥)** in any text field to draft/rewrite using your memory.
- Open the panel to browse the **constellation**, the **memory wiki**, and to **chat**.

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| **"cognee busy/unreachable"** | The engine is warming up or busy building the graph. `docker compose logs -f quill-cognee`; give it a few minutes, especially on first run. |
| `curl .../health` times out | Sidecar still starting, or its LLM is unreachable — check the LLM/embedding keys in `cognee-sidecar/.env`. |
| Ollama option: empty results | `ollama pull qwen2.5:14b nomic-embed-text` and confirm `ollama` is running (`curl localhost:11434/api/tags`). |
| Drafts do nothing | Quill's `.env` (step 4) missing/incorrect, or Accessibility not granted (step 6). |
| Port 8765 in use | Something else grabbed it; edit the port in `docker-compose.yml`. |

## Privacy

With the **Ollama** option, **nothing leaves your machine** — cognee, the graph, and the LLM
all run locally. With the OpenAI option, only the text sent for drafting/graph-building goes
to OpenAI; your capture history and graph stay local either way.
