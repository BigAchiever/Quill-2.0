// Activity-digest lane: a batched consolidation pass, with cognee as the graph store.
//
// Capture stays a firehose LOCALLY (SQLite/FTS = the fast lane). Here, on QUIET ticks only, we turn
// accumulated raw captures into a few CLEAN factual sentences via one bounded LLM call per surface,
// and feed THOSE to cognee. So browsing / IDE / tool / Claude entities reach the graph (which the
// 1c surface-filter would have dropped) WITHOUT the UI-chrome noise ever entering it — the LLM
// extraction is the cleaning. Bounded (≤ MAX_SURFACES digests/pass) → structurally can't re-create
// a backlog. Conversations keep the 1c hot lane (timeliness for drafting recall).

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};

/// Digest circuit-breaker: after an add TIMES OUT (sidecar busy mid-cognify), pause the whole lane
/// so it doesn't hammer the sidecar or spam the log — resumes in a free window. health() alone
/// isn't enough: /health answers even while /add is blocked by an in-flight cognify.
static COLD_UNTIL: AtomicI64 = AtomicI64::new(0);
const COOLDOWN_SECS: i64 = 300;

const CURSOR_KEY: &str = "digest_cursor";
const FETCH: usize = 200; // rows scanned per pass
const MAX_SURFACES: usize = 4; // ≤4 LLM digest calls per pass (well inside the LLM throttle)
const MIN_ROWS: usize = 2; // skip surfaces with too little to say
const MAX_DIGEST_INPUT: usize = 6000; // chars of raw context fed to the digest LLM per surface

/// One digest pass. Scans new snapshots, digests the busiest NON-conversation surfaces into clean
/// sentences, and feeds them to cognee. Advances the cursor to the max FETCHED id UNCONDITIONALLY
/// — a failed digest loses only that slice's graph enrichment (the raw data is still in FTS/wiki),
/// which deliberately avoids the head-of-line stall the retired sync_step lane suffered.
pub fn digest_step() {
    let Some(conn) = crate::db::open_default() else {
        return;
    };
    // Backed off after a recent timeout — skip entirely, don't touch the cursor (retry later).
    if crate::db::now_secs() < COLD_UNTIL.load(Ordering::Relaxed) {
        return;
    }
    let cursor: i64 = match crate::db::get_setting(&conn, CURSOR_KEY).and_then(|v| v.parse().ok()) {
        Some(c) => c,
        None => {
            // First run: start from NOW. Digesting all history is Phase 3's job, not the ambient
            // lane's — it would be its own (bounded, but large) storm on first launch.
            let now_max = crate::db::max_snapshot_id(&conn);
            let _ = crate::db::set_setting(&conn, CURSOR_KEY, &now_max.to_string());
            println!("[quill] digest cursor initialized → #{now_max}");
            return;
        }
    };

    // Only run when the sidecar can actually accept an add. During a cognify (e.g. the 1c lane
    // draining) the sidecar blocks and the add would hang for its full timeout and the digest be
    // lost. health() is a cheap "is it free?" probe (3s cap); if busy/down, skip this pass WITHOUT
    // advancing the cursor and retry next quiet tick — so digests only run in a free window and
    // never burn an LLM call whose result can't land. Not a stall risk: transient, self-recovers.
    if !crate::cognee::health() {
        return;
    }

    let rows = match crate::db::activity_after(&conn, cursor, FETCH) {
        Ok(r) if !r.is_empty() => r,
        _ => return, // caught up (or a DB hiccup — retried next quiet tick)
    };
    let max_id = rows.iter().map(|r| r.id).max().unwrap_or(cursor);

    // Group by dataset (surface). Only NON-conversation surfaces (class "other": browsing, IDEs,
    // tools, Claude) — conversations are handled by the 1c hot lane already.
    let mut by_surface: HashMap<String, Vec<String>> = HashMap::new();
    for r in rows {
        if r.app_bundle.is_empty() || crate::app::is_excluded(&r.app_bundle) {
            continue;
        }
        let ds = crate::cognee::dataset_for(&r.app_bundle, r.domain.as_deref());
        if crate::profile::class_of(&ds) != "other" {
            continue;
        }
        by_surface.entry(ds).or_default().push(r.text);
    }

    // Busiest surfaces first (more captures ≈ more attention), capped at MAX_SURFACES.
    let mut surfaces: Vec<(String, Vec<String>)> =
        by_surface.into_iter().filter(|(_, v)| v.len() >= MIN_ROWS).collect();
    surfaces.sort_by_key(|(_, v)| std::cmp::Reverse(v.len()));
    surfaces.truncate(MAX_SURFACES);

    for (ds, snippets) in surfaces {
        let ctx = build_context(&snippets);
        match crate::llm::digest_activity(&ds, &ctx) {
            Ok(digest) => {
                let body = format!("[activity digest · {ds}]\n{digest}");
                match crate::cognee::add(&body, &ds) {
                    Ok(()) => {
                        crate::cognee::mark_dirty(&ds);
                        println!(
                            "[quill] digest → cognee [{ds}] ({} chars, from {} captures)",
                            digest.chars().count(),
                            snippets.len()
                        );
                    }
                    Err(e) => {
                        println!("[quill] digest add failed [{ds}]: {e} — pausing digest lane");
                        // Sidecar busy (mid-cognify) — the rest of this pass would fail the same
                        // way. Back off; cursor still advances (window loss ok — FTS has the raw).
                        COLD_UNTIL.store(crate::db::now_secs() + COOLDOWN_SECS, Ordering::Relaxed);
                        break;
                    }
                }
            }
            Err(e) => println!("[quill] digest skipped [{ds}]: {e}"),
        }
    }

    // Advance unconditionally (see the doc comment): raw data is never lost — it lives in FTS.
    let _ = crate::db::set_setting(&conn, CURSOR_KEY, &max_id.to_string());
}

/// Concatenate a surface's snippets into one digest input: trimmed lines, blanks/tiny lines
/// dropped, clipped to MAX_DIGEST_INPUT. Light touch — the digest LLM does the real cleaning.
fn build_context(snippets: &[String]) -> String {
    let mut out = String::new();
    for s in snippets {
        for line in s.lines().map(str::trim) {
            if line.chars().count() < 4 {
                continue;
            }
            out.push_str(line);
            out.push('\n');
            if out.chars().count() >= MAX_DIGEST_INPUT {
                return out;
            }
        }
        out.push_str("---\n");
    }
    out
}
