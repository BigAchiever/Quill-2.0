# Quill — Setup for Judges & Evaluators

Run the packaged app in **~10 minutes**. **No Rust, Node, or build toolchain required** — you
only need Docker and one LLM option (an API key *or* local Ollama). If you'd rather run from
source, see [`SETUP.md`](SETUP.md) instead.

> **TL;DR:** start the memory engine (`docker compose up -d`) → drop your LLM key in two `.env`
> files → open `Quill.dmg` → grant Accessibility → press **Right-Option (`⌥`)** in any text field.

> **🔒 Before you grant Accessibility — a straight security note.** Quill is **unsigned and
> open source**. To work, it reads your focused text field and types drafts back (macOS
> Accessibility). Your captured text stays on your Mac **except** what's sent to the LLM you
> configure — with a cloud key it goes to that provider; choose the **local Ollama** option for
> full privacy. The memory engine binds to `127.0.0.1` only; data at rest isn't encrypted yet.

---

## What you need

- **macOS 13+ on Apple Silicon** (the DMG is arm64; on an Intel Mac, build from source — see [`SETUP.md`](SETUP.md))
- **[Docker Desktop](https://www.docker.com/products/docker-desktop/)**, running (hosts the
  Cognee memory engine)
- **One** LLM option:
  - **Fastest:** an **OpenAI** or **[OpenRouter](https://openrouter.ai/)** API key, **or**
  - **Fully local / private:** **[Ollama](https://ollama.com/)** installed (nothing leaves your
    machine)

---

## 1. Get the app + config files

The app installs from a `.dmg`; the memory engine runs from this repo's `docker-compose.yml`
(nothing here is compiled — it uses the public `cognee/cognee` image).

- **Download the app:** **[`Quill.dmg`](https://github.com/BigAchiever/Quill-2.0/releases/download/v0.1.0/Quill.dmg)** — Apple Silicon build (~5 MB)
- **Get this repo** (for the sidecar config + compose file — it's ~2 MB, no build needed):
  ```bash
  git clone https://github.com/BigAchiever/Quill-2.0.git
  cd Quill-2.0
  ```
  _(Or download the repo ZIP from GitHub and `cd` into it.)_

## 2. Configure the memory engine (Cognee)

```bash
cp cognee-sidecar/.env.example cognee-sidecar/.env
```

Open `cognee-sidecar/.env` and uncomment **one** LLM block:

- **OpenAI (fastest):** it's the default block — paste your key into **`LLM_API_KEY`** and
  **`EMBEDDING_API_KEY`**. Done.
- **Local / private (Ollama):** comment out the OpenAI block, uncomment the Ollama blocks for
  both LLM and embeddings, then pull the models:
  ```bash
  ollama pull qwen2.5:14b
  ollama pull nomic-embed-text
  ```

## 3. Start the memory engine

```bash
docker compose up -d

# health check — should return HTTP 200 once it's warm (first start can take a minute):
curl -s -o /dev/null -w "cognee: HTTP %{http_code}\n" http://127.0.0.1:8765/health
```

## 4. Configure Quill's own LLM

The **packaged app** reads its config from your Application Support folder. Create it there:

```bash
mkdir -p ~/Library/Application\ Support/com.danishalisiddiqui.quill
cp quill/src-tauri/.env.example ~/Library/Application\ Support/com.danishalisiddiqui.quill/.env
```

Edit that file and uncomment the block matching step 2 (use the **same** provider):

| Variable | What it is |
|---|---|
| `QUILL_LLM_URL` | chat-completions endpoint, e.g. `https://api.openai.com/v1/chat/completions` |
| `QUILL_LLM_KEY` | API key for that endpoint (`ollama` for local Ollama) |
| `QUILL_LLM_MODEL` | model id, e.g. `gpt-4o-mini` or `qwen2.5:14b` |

> For Ollama, use the **`localhost`** URL — Quill runs on the host, while the sidecar runs in
> Docker (so *its* config uses `host.docker.internal`).

## 5. Install & open the app

1. Open **`Quill.dmg`** and drag **Quill** to **Applications**.
2. It's an **unsigned** build, so the first launch is blocked by Gatekeeper. **Right-click the
   app → Open → Open** to allow it. If that's greyed out on newer macOS, run
   `xattr -dr com.apple.quarantine /Applications/quill.app` and reopen. (Only needed once.)

## 6. Grant Accessibility

Quill reads the focused text field and types drafts back into it, which needs macOS
Accessibility:

> **System Settings → Privacy & Security → Accessibility → enable Quill**, then relaunch Quill.

Without this, ambient capture and inline drafting won't work.

---

## ▶️ See it work in 2 minutes

Quill's memory normally fills **ambiently** as you work, so a fresh install starts empty by
design. To experience the hero moment immediately, inject one realistic "seen earlier" fact:

```bash
# with the sidecar running (step 3):
python3 cognee-sidecar/demo-seed.py
```

Then:

1. Open any email/compose field (or a note) and leave it empty.
2. Make sure the question is visible on screen — e.g. an email asking *"When is the Comet launch?"*
3. Press **Right-Option (`⌥`)** in the empty reply box.
4. Quill drafts the answer using a fact that is **nowhere on your screen** — *"…moved to Friday."*

That fact traveled: **injected memory → Cognee knowledge graph → graph recall → your draft.**
Open the panel to watch it appear as a node in the **constellation** and as a distilled **wiki
page**.

---

## What you're evaluating (the Cognee story)

Quill is built on Cognee's memory lifecycle — the graph *is* the product. As you use it, notice:

- **`remember`** — your writing and the conversations you see become graph nodes (`add` +
  `cognify`).
- **`recall`** — every `⌥` draft starts by recalling grounded facts from the graph (with a local
  full-text fallback so a keypress never blocks).
- **`improve` (`memify`)** — the graph self-improves: prunes stale nodes, strengthens frequent
  ties, derives new facts.
- **`forget`** — excluding an app or surface drops exactly that dataset from memory, with no
  residue.
- **Provenance & isolation** — every surface is its own Cognee dataset, so recalled facts carry
  *where they came from* and a single surface can be forgotten surgically.

More detail: [`README.md`](README.md) → *"How Cognee powers Quill"*, and the retrieval-strategy
evidence in [`tools/search-bench.md`](tools/search-bench.md).

---

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| **"Quill can't be opened" (Gatekeeper)** | Unsigned build. **Right-click → Open → Open** (once), or *System Settings → Privacy & Security → Open Anyway*. |
| **`curl .../health` times out** | Sidecar still starting, or its LLM/embedding keys are wrong. `docker compose logs -f quill-cognee`; give it a minute on first run. |
| **"cognee busy/unreachable" in the app** | The engine is warming up or building the graph. Wait a moment — Quill falls back to local recall meanwhile and won't block. |
| **Drafts do nothing** | The app's `.env` (step 4) is missing/incorrect, or Accessibility (step 6) isn't granted. |
| **Ollama option returns nothing** | `ollama pull qwen2.5:14b nomic-embed-text` and confirm Ollama is running (`curl localhost:11434/api/tags`). |
| **Port 8765 already in use** | Something else grabbed it; edit the port mapping in `docker-compose.yml` and update `QUILL_COGNEE_URL`. |

---

## Privacy

With the **Ollama** option, **nothing leaves your machine** — the memory engine, the graph, and
the LLM all run locally. With a cloud key, only the text sent for drafting/graph-building goes to
that provider; your capture history and knowledge graph always stay on your Mac. Pause capture,
exclude apps, or **forget** a surface at any time from the panel.
