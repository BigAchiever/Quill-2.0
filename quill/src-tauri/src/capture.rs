// Phase 2: ambient capture loop. Periodically reads the focused window's visible text
// and stores a snapshot when it changes. Skips excluded apps. Lightweight + deduped.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

const INTERVAL_MS: u64 = 3000;
const MAX_CHARS: usize = 8000;

// Retention (Phase 2.4): keep the local store bounded. Conservative defaults — these
// only prune OLD / EXCESS snapshots, never reset the DB. Tune in Settings later.
const RETENTION_MAX_AGE_SECS: i64 = 30 * 24 * 60 * 60; // ~30 days of memory
const RETENTION_MAX_ROWS: i64 = 200_000; // hard cap on total snapshots
const RETENTION_EVERY_TICKS: u64 = 200; // ~10 min at a 3s interval

/// Set on app exit so the capture loop stops cleanly instead of being torn down mid-write.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Ask the capture loop to stop (called from the app's exit handler).
pub fn request_shutdown() {
    SHUTDOWN.store(true, Ordering::Relaxed);
}

fn hash_text(s: &str) -> i64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_and_content_sensitive() {
        assert_eq!(hash_text("hello world"), hash_text("hello world"));
        assert_ne!(hash_text("hello world"), hash_text("hello  world"));
        assert_ne!(hash_text("a"), hash_text("b"));
    }
}

/// Start the background capture loop under a SUPERVISOR (self-healing): the loop
/// runs inside catch_unwind, so a panic in a read/store doesn't silently kill capture or take the
/// UI with it — the supervisor respawns it with backoff. A clean return (shutdown / db won't open)
/// ends supervision.
pub fn start(app: tauri::AppHandle, db_path: PathBuf) {
    thread::spawn(move || {
        let mut backoff = 1u64;
        loop {
            let (a, p) = (app.clone(), db_path.clone());
            let outcome =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || run_capture(a, p)));
            if SHUTDOWN.load(Ordering::Relaxed) {
                break;
            }
            match outcome {
                Ok(()) => break, // clean exit (shutdown or db-open failure) — nothing to respawn
                Err(_) => {
                    eprintln!("[quill] capture loop PANICKED — respawning in {backoff}s");
                    crate::db::insert_event(
                        "system",
                        "capture crashed — recovered",
                        &format!("the capture loop panicked and was respawned after {backoff}s"),
                    );
                    thread::sleep(Duration::from_secs(backoff));
                    backoff = (backoff * 2).min(30);
                }
            }
        }
    });
}

fn run_capture(app: tauri::AppHandle, db_path: PathBuf) {
    use tauri::Emitter;
    let conn = match crate::db::open(&db_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[quill] capture: failed to open db: {e}");
            return;
        }
    };
    println!("[quill] capture loop started → {}", db_path.display());

        // Running total kept in memory (seeded once) so we don't COUNT(*) the whole table on
        // every insert just for a log line — that scan was the loop's main avoidable cost.
        let mut total = crate::db::count(&conn).unwrap_or(0);
        let mut tick: u64 = 0;
        let mut stale_ax = 0u32; // consecutive empty AX reads (permission revoked / AX wedged)
        let mut last_wall = crate::db::now_secs();
        loop {
            thread::sleep(Duration::from_millis(INTERVAL_MS));
            tick = tick.wrapping_add(1);

            if SHUTDOWN.load(Ordering::Relaxed) {
                println!("[quill] capture loop stopping (shutdown requested)");
                break;
            }

            // Post-sleep / long-idle recovery: a big wall-clock jump between ticks means the
            // machine slept. AX reads self-recover on the next tick; just note it so a gap in
            // captured history is explained, not mysterious.
            let wall = crate::db::now_secs();
            if wall - last_wall > 60 {
                println!("[quill] capture resumed after a {}s gap (sleep/idle)", wall - last_wall);
            }
            last_wall = wall;

            // Periodically bound the DB so ambient capture can't grow it without limit.
            if tick % RETENTION_EVERY_TICKS == 0 {
                match crate::db::enforce_retention(
                    &conn,
                    RETENTION_MAX_AGE_SECS,
                    RETENTION_MAX_ROWS,
                ) {
                    Ok(n) if n > 0 => {
                        total = (total - n as i64).max(0); // keep the in-memory total honest
                        println!("[quill] retention: pruned {n} old snapshot(s)");
                    }
                    Ok(_) => {}
                    Err(e) => eprintln!("[quill] retention failed: {e}"),
                }
            }

            // Consolidate memory off the hot path, on quiet ticks only. TWO-SPEED memory (see
            // MEMORY-ARCHITECTURE.md): the raw ambient firehose stays LOCAL (SQLite/FTS) and is
            // NOT pushed into cognee — dumping screen-scrape noise into the graph buried the
            // sidecar in a backlog and polluted the graph. cognee is fed SELECTIVELY by the
            // authored lanes (style::remember, corrections) and, later, cleaned conversation
            // extracts (Phase 1c). The raw sync_step / cognify_dirty lanes are retired here.
            {
                // Drain the SELECTIVE conversation feed (Phase 1c): cognify datasets that got new
                // conversation captures, on quiet ticks only so LLM extraction never competes with
                // an active keypress. Low volume (conversation surfaces, deduped) → no backlog.
                if tick % 100 == 0 && crate::trigger::secs_since_last_trigger() > 3 * 60 {
                    std::thread::spawn(crate::cognee::cognify_dirty);
                }
                // Activity digests (Phase 2): turn raw browsing/IDE/tool captures into CLEAN graph
                // entities via one bounded LLM pass on a quiet tick — the surfaces the 1c filter
                // drops (the consolidation pass). Bounded per pass → no backlog. consolidate.rs.
                if tick % 140 == 0 && crate::trigger::secs_since_last_trigger() > 5 * 60 {
                    std::thread::spawn(crate::consolidate::digest_step);
                }
                // Wiki distillation (P4b): on a deep quiet tick, distill a few entity pages from
                // the LOCAL snapshots. Small batch → converges over idle periods; its llm::chat
                // calls (direct to the LLM) never land during an active keypress.
                if tick % 220 == 0 && crate::trigger::secs_since_last_trigger() > 10 * 60 {
                    std::thread::spawn(|| {
                        crate::wiki::refresh(3);
                    });
                }
                // Phase 3 — enrichment: run cognee's memify on a DEEP quiet tick (rotating one
                // dataset per pass) to keep the vector index fresh. NB: with triplet_embedding off
                // this does not prune nodes; noise is removed by `forget` / the one-off cleanup.
                // Long-running on the sidecar → only when idle a while, ~hourly.
                if tick % 1200 == 0 && crate::trigger::secs_since_last_trigger() > 20 * 60 {
                    std::thread::spawn(|| {
                        if let Err(e) = crate::cognee::memify() {
                            println!("[quill] memify skipped: {e}");
                        }
                    });
                }
                // Ambient identity refresh — deep idle; self-throttled to ~daily inside
                // ambient_refresh, which queues a review proposal instead of silently rewriting you.
                if tick % 1500 == 0 && crate::trigger::secs_since_last_trigger() > 15 * 60 {
                    std::thread::spawn(crate::identity::ambient_refresh);
                }
            }

            if crate::inject::is_injecting() {
                continue; // don't capture quill's own loader / typed output
            }

            if crate::settings::is_paused() {
                continue; // user paused ambient capture (Phase 2.3)
            }

            let bundle = crate::app::frontmost_bundle_id().unwrap_or_default();
            if bundle.is_empty() || crate::app::is_excluded(&bundle) {
                continue; // never capture excluded / unknown apps
            }

            let text = crate::ax::read_window_context(MAX_CHARS);
            if text.trim().len() < 2 {
                // Stale-AX detection: a focused, non-excluded app should have readable text.
                // Sustained emptiness = accessibility was revoked or the AX bridge wedged; warn
                // once (~1 min) so it's diagnosable instead of a silent memory blackout.
                stale_ax += 1;
                if stale_ax == 20 {
                    eprintln!(
                        "[quill] AX reads empty for ~1min in {bundle_hint} — Accessibility \
permission may have been revoked (System Settings → Privacy → Accessibility)",
                        bundle_hint = crate::app::frontmost_bundle_id().unwrap_or_default()
                    );
                    crate::db::insert_event(
                        "system",
                        "screen reads are coming back empty",
                        "Accessibility permission may have been revoked — check System Settings → Privacy & Security → Accessibility",
                    );
                }
                continue;
            }
            stale_ax = 0;

            let h = hash_text(&text);

            // Read anchors ONCE, before storing — persisted locally now (title/url/domain/
            // focused-element breadcrumb) AND reused for the cognee header, instead of read
            // only for cognee and thrown away locally.
            let anchors = crate::ax::read_anchors();
            let meta = crate::db::SnapMeta {
                window_title: anchors.window_title.as_deref(),
                url: anchors.url.as_deref(),
                domain: anchors.domain.as_deref(),
                focused_name: anchors.focused_name.as_deref(),
                focused_role: anchors.focused_role.as_deref(),
                focused_path: anchors.focused_path.as_deref(),
            };
            // Dwell-aware store: a near-duplicate of the app's last snapshot MERGES into it
            // (accrues dwell) rather than adding a row; only genuinely new content inserts and
            // flows to cognee. Replaces the in-memory dedup ring — see db::merge_or_insert.
            match crate::db::merge_or_insert_snapshot(&conn, &bundle, &text, h, &meta) {
                Ok((_id, true)) => {
                    // Dwell updated on the existing row — no new snapshot, no cognee add.
                }
                Ok((_id, false)) => {
                    total += 1;
                    let _ = app.emit("quill://capture", ()); // heartbeat → UI pulses the status dot
                    println!(
                        "[quill] captured: {} ({} chars) — {} total",
                        bundle,
                        text.len(),
                        total
                    );
                    // The raw snapshot lives in SQLite/FTS for instant recall (the fast lane).
                    // TWO-SPEED decision B (Phase 1c): feed cognee ONLY captures from conversation
                    // surfaces (chat / mail / social — where real messages live), lightly cleaned
                    // and deduped (new snapshots only). Dev tools, terminals and random browsing
                    // stay local-only, so the graph grows from meaningful content — not screen
                    // noise — and the volume is far too low to re-create a backlog.
                    let ds = crate::cognee::dataset_for(&bundle, anchors.domain.as_deref());
                    if matches!(crate::profile::class_of(&ds), "chat" | "mail" | "social") {
                        if let Some(clean) = clean_conversation(&text) {
                            std::thread::spawn(move || match crate::cognee::add(&clean, &ds) {
                                Ok(()) => {
                                    crate::cognee::mark_dirty(&ds);
                                    println!(
                                        "[quill] cognee ← conversation [{ds}] ({} chars)",
                                        clean.chars().count()
                                    );
                                }
                                Err(e) => println!("[quill] cognee conversation add failed [{ds}]: {e}"),
                            });
                        }
                    }
                }
                Err(e) => eprintln!("[quill] capture: insert failed: {e}"),
            }
        }
}

/// Light cleanup of a conversation capture before it enters cognee's graph (Phase 1c). Drops
/// blank / tiny lines and requires a minimum overall length, so trivial captures don't create
/// empty graph work. Conservative on purpose — the SURFACE filter (chat/mail/social) does the
/// heavy noise reduction; aggressive line-stripping risks dropping real short messages. A deeper
/// chrome-strip / message extractor is a Phase-2 refinement.
fn clean_conversation(text: &str) -> Option<String> {
    let kept: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| l.chars().count() >= 3)
        .collect();
    let out = kept.join("\n");
    if out.chars().count() >= 60 {
        Some(out)
    } else {
        None
    }
}
