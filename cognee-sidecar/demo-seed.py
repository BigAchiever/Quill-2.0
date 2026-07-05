#!/usr/bin/env python3
"""Reproduce Quill's cross-app recall demo in ~2 minutes (for evaluators).

Quill's memory normally fills AMBIENTLY — it watches your screen as you work, so a fresh
install starts empty by design. This script injects one realistic "seen earlier in Teams"
fact into the cognee memory so you can experience the hero moment immediately:

  1. Run me:  python3 demo-seed.py           (sidecar must be up — see README)
  2. Open any email/compose field and type nothing (or a placeholder-like empty reply).
  3. Make sure the visible screen mentions the question, e.g. have an email open that asks
     "When is the Comet launch happening?"
  4. Press RIGHT-OPTION in the compose field.
  5. Quill drafts the answer with a fact that is nowhere on your screen:
     "…moved to Friday, Priya asked for extra QA time."

That fact traveled: injected memory → knowledge graph → graph recall → your draft.
"""

import subprocess
import sys
import tempfile
from pathlib import Path

BASE = "http://127.0.0.1:8765/api/v1"

FACT = """[screen capture · com.microsoft.teams2 · Engineering channel]
Priya Nair: Team, heads up — we're moving the Comet launch to FRIDAY.
Priya Nair: I want the extra two days for a full QA pass on the checkout flow.
Jordan Rivera: Makes sense. I'll let the client know once they ask.
Priya Nair: Thanks. Friday morning, 10am IST. Lock it in.
"""


def main() -> None:
    with tempfile.NamedTemporaryFile("w", suffix=".txt", delete=False) as f:
        f.write(FACT)
        path = Path(f.name)
    print("Injecting the demo fact into cognee (add + cognify — needs the LLM, ~15s)…")
    r = subprocess.run(
        ["curl", "-s", "-m", "300", "-X", "POST", f"{BASE}/remember",
         "-F", f"data=@{path};type=text/plain", "-F", "datasetName=app-teams"],
        capture_output=True, text=True,
    )
    path.unlink(missing_ok=True)
    if r.returncode != 0 or '"error"' in r.stdout[:200].lower():
        print(f"FAILED — is the sidecar up on :8765? Response: {r.stdout[:300]}")
        sys.exit(1)
    print("Done. Now: open an email asking about the Comet launch date, focus the reply box,")
    print("press RIGHT-OPTION — the draft will say Friday (a fact not on your screen).")
    print("Verify the memory yourself:")
    print(f"""  curl -s {BASE}/recall -H 'Content-Type: application/json' \\
    -d '{{"query":"when is the comet launch?"}}'""")


if __name__ == "__main__":
    main()
