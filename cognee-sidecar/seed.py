#!/usr/bin/env python3
"""M2: seed cognee from quill's existing snapshots (comms-first subset).

Per app: export snapshots to temp files -> /add in batches of 20 -> /cognify (background)
-> poll until COMPLETED. Watchdog: a batch stuck INITIATED > WEDGE_SECS gets one container
restart + one cognify retry (rare LLM calls can hang; cognee has no built-in timeout).

Run:  nohup python3 seed.py > seed.log 2>&1 &
"""

import json
import sqlite3
import subprocess
import sys
import tempfile
import time
from datetime import datetime, timezone
from pathlib import Path

BASE = "http://127.0.0.1:8765/api/v1"
QDB = Path.home() / "Library/Application Support/com.danishalisiddiqui.quill/quill.db"
BATCH = 20           # docs per add call (adds are cheap; batching just bounds multipart size)
MIN_CHARS = 250      # skip trivial captures
MAX_CHARS = 5000     # clip huge ones (bounds extraction time)
# Patient watchdog: cognify fans out many concurrent LLM calls and can saturate a local model —
# batches legitimately run long. 45 min only catches TRUE hangs (rare dropped-request case).
WEDGE_SECS = 45 * 60
POLL_SECS = 30

# (app_bundle, dataset_name, max_docs newest-first, mode). Comms surfaces first (example plan).
# mode: "full" = add + cognify · "cognify" = docs already added, just process · "skip" = done.
PLAN = [
    ("com.microsoft.Outlook",         "app-outlook",  None, "skip"),     # already processed
    ("com.microsoft.teams2",          "app-teams",    None, "skip"),     # already processed
    ("net.whatsapp.WhatsApp",         "app-whatsapp", None, "skip"),     # already processed
    ("com.hnc.Discord",               "app-discord",  None, "skip"),     # already processed
    ("ai.perplexity.comet",           "app-comet",    400,  "cognify"),  # add up to 400 newest, then cognify
]


def log(msg: str) -> None:
    print(f"[{datetime.now().strftime('%H:%M:%S')}] {msg}", flush=True)


def curl_json(args: list[str], timeout: int = 120) -> tuple[int, str]:
    p = subprocess.run(["curl", "-s", "-m", str(timeout), *args], capture_output=True, text=True)
    return p.returncode, p.stdout


def dataset_status() -> dict:
    _, out = curl_json([f"{BASE}/datasets/status"], timeout=10)
    try:
        return json.loads(out)
    except Exception:
        return {}


def dataset_id(name: str) -> str | None:
    _, out = curl_json([f"{BASE}/datasets"], timeout=10)
    try:
        for d in json.loads(out):
            if d.get("name") == name:
                return d["id"]
    except Exception:
        pass
    return None


def restart_container() -> None:
    log("WATCHDOG: restarting quill-cognee (wedged LLM call)")
    subprocess.run(["docker", "restart", "quill-cognee"], capture_output=True)
    time.sleep(25)


def _add_files(files: list[Path], dataset: str) -> bool:
    args = ["-X", "POST", f"{BASE}/add", "-F", f"datasetName={dataset}"]
    for f in files:
        args += ["-F", f"data=@{f};type=text/plain"]
    code, out = curl_json(args, timeout=180)
    return code == 0 and '"error"' not in out.lower()[:200]


def add_batch(files: list[Path], dataset: str) -> bool:
    """Batch add; on failure (e.g. cognee 500s on duplicate-content IntegrityError) retry each
    file individually so one dupe doesn't sink the other 19. True if ANY file made it in."""
    if _add_files(files, dataset):
        return True
    log("  batch add failed — retrying per-file (duplicates get skipped)")
    ok = 0
    for f in files:
        if _add_files([f], dataset):
            ok += 1
    log(f"  per-file retry: {ok}/{len(files)} added")
    return ok > 0


def cognify(dataset: str) -> bool:
    """Trigger cognify (background) then poll to completion; one restart+retry on wedge."""
    for attempt in (1, 2):
        code, out = curl_json(
            ["-X", "POST", f"{BASE}/cognify", "-H", "Content-Type: application/json",
             "-d", json.dumps({"datasets": [dataset], "runInBackground": True})],
            timeout=60,
        )
        if code != 0:
            log(f"  cognify trigger failed (curl={code}), attempt {attempt}")
            time.sleep(10)
            continue
        dsid = dataset_id(dataset)
        start = time.time()
        while True:
            time.sleep(POLL_SECS)
            st = dataset_status().get(dsid, "?")
            if st == "DATASET_PROCESSING_COMPLETED":
                return True
            if st == "DATASET_PROCESSING_ERRORED":
                log(f"  cognify ERRORED on {dataset} (attempt {attempt})")
                break  # retry once
            if time.time() - start > WEDGE_SECS:
                if attempt == 1:
                    restart_container()
                    break  # re-trigger cognify — already-processed docs are skipped
                log(f"  cognify STILL wedged after retry — skipping rest of {dataset}")
                return False
    return False


def main() -> None:
    conn = sqlite3.connect(QDB)
    tmp = Path(tempfile.mkdtemp(prefix="quill-seed-"))
    total_docs = datasets_ok = 0
    t0 = time.time()

    for app, dataset, cap, mode in PLAN:
        if mode == "skip":
            log(f"{dataset}: skip (already completed)")
            continue

        if mode == "full":
            # One row per DISTINCT content hash (newest occurrence) — cognee 500s on duplicates.
            rows = conn.execute(
                "SELECT MAX(id), MAX(ts), text FROM snapshots WHERE app_bundle=? AND LENGTH(text)>=? "
                "GROUP BY text_hash ORDER BY MAX(ts) DESC" + (f" LIMIT {cap}" if cap else ""),
                (app, MIN_CHARS),
            ).fetchall()
            if not rows:
                log(f"{dataset}: no rows, skipping")
                continue
            log(f"{dataset}: {len(rows)} snapshot(s) — add all, then ONE patient cognify")

            files: list[Path] = []
            for sid, ts, text in rows:
                when = datetime.fromtimestamp(ts, tz=timezone.utc).strftime("%Y-%m-%d %H:%M UTC")
                body = f"[screen capture · {app} · {when}]\n{text[:MAX_CHARS]}"
                f = tmp / f"{dataset}-{sid}.txt"
                f.write_text(body)
                files.append(f)

            # Phase 1: add everything (cheap, no LLM). Per-file retry absorbs duplicates.
            for i in range(0, len(files), BATCH):
                add_batch(files[i : i + BATCH], dataset)
            total_docs += len(files)
        else:
            log(f"{dataset}: cognify-only (docs already added)")

        # Phase 2: ONE cognify sweeps everything unprocessed in the dataset. The patient
        # 45-min window tolerates a saturated local model; only a true hang trips the watchdog.
        if cognify(dataset):
            datasets_ok += 1
            log(f"{dataset}: COMPLETED")
        else:
            log(f"{dataset}: cognify did not complete — sweep again in the morning")

    log(f"DONE: {total_docs} docs added, {datasets_ok} dataset(s) cognified in {(time.time()-t0)/60:.0f} min")


if __name__ == "__main__":
    main()
