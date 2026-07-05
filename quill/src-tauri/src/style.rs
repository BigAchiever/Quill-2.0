// Per-surface writing-style learning. The ONLY signal is the
// user's OWN writing: their raw Mode-1 input — the text they typed before quill rewrites it. We
// never learn quill's generated output, nor edits of it. Distilled to a few tone bullets per app
// and injected into future prompts. Re-learn is debounced; samples are retention-capped.

const SAMPLE_MIN_CHARS: usize = 15; // ignore one-word inputs — too little to characterise tone
const MIN_SAMPLES_TO_LEARN: i64 = 2; // a couple of signals is enough to start
const SAMPLES_FOR_PROMPT: usize = 12;
const SAMPLES_KEEP: i64 = 50; // per-surface retention cap

/// Record a piece of the user's OWN writing (their raw input — NEVER quill's output) as a style
/// sample for `surface`, then (debounced) re-distil the per-app tone.
pub fn record_user_writing(surface: &str, text: &str) {
    let text = text.trim();
    if surface.is_empty() || text.chars().count() < SAMPLE_MIN_CHARS {
        return;
    }
    // Key voice + this-writing memory to the DOMAIN-AWARE profile (LinkedIn in any browser is one
    // profile) — the same key ambient capture uses, so a platform's voice and its memory align.
    let key = crate::profile::key_for(surface);

    // "Learn my voice here" consent (Profiles): when the user turns it off for this surface, we skip
    // style learning. Memory/recall is governed separately (capture + exclusions + memory circles).
    if voice_learning_on(&key) {
        println!(
            "[quill] style: recorded your writing in profile {key} ({} chars)",
            text.chars().count()
        );
        store_sample(&key, text);
    }

    // Hot lane: the user's OWN writing is the highest-signal memory there is — remember it
    // immediately (full add+cognify), tagged with its profile dataset, off this thread.
    {
        let body = format!("[the user's own writing · {key}]\n{}", text);
        std::thread::spawn(move || {
            if let Err(e) = crate::cognee::remember(&body, &key) {
                println!("[quill] cognee remember skipped: {e}");
            }
        });
    }
}

/// Insert a sample, prune to the retention cap, then (debounced) re-distil the style.
fn store_sample(surface: &str, text: &str) {
    let Some(conn) = crate::db::open_default() else {
        return;
    };
    if crate::db::insert_style_sample(&conn, surface, text).is_err() {
        return;
    }
    let _ = crate::db::prune_style_samples(&conn, surface, SAMPLES_KEEP);
    let n = crate::db::style_sample_count(&conn, surface).unwrap_or(0);
    if should_learn(n) {
        learn(&conn, surface);
    }
}

/// Re-learn early (2–6 samples) then every 5th — NOT on every interaction (avoids an LLM call
/// per sample + bullets that flicker because they're re-distilled from scratch each time).
fn should_learn(n: i64) -> bool {
    n >= MIN_SAMPLES_TO_LEARN && (n <= 6 || n % 5 == 0)
}

fn learn(conn: &rusqlite::Connection, surface: &str) {
    let samples =
        crate::db::recent_style_samples(conn, surface, SAMPLES_FOR_PROMPT).unwrap_or_default();
    if (samples.len() as i64) < MIN_SAMPLES_TO_LEARN {
        return;
    }
    match crate::llm::distill_style(surface, &samples) {
        Ok(bullets) if !bullets.is_empty() => {
            let current = crate::db::style_notes_for(conn, surface).unwrap_or_default();
            if current.is_empty() {
                // First voice for this surface — nothing to compare against, so apply directly.
                if crate::db::replace_style_notes(conn, surface, &bullets).is_ok() {
                    println!("[quill] style learned for {surface}: {} bullet(s)", bullets.len());
                }
            } else if bullets != current && !crate::db::has_proposal(conn, "voice", surface) {
                // A CHANGE to an existing voice — route through review-diff instead of silently
                // overwriting how the user sounds (one pending proposal per surface at a time).
                let before = current.join("\n");
                let after = bullets.join("\n");
                if crate::db::insert_proposal(conn, "voice", surface, &before, &after).is_ok() {
                    crate::db::insert_event(
                        "review",
                        &format!("Voice update ready for {surface}"),
                        "Review and approve it in Profiles.",
                    );
                    println!("[quill] style proposal queued for {surface}");
                }
            }
        }
        Ok(_) => {}
        Err(e) => eprintln!("[quill] style learn failed for {surface}: {e}"),
    }
}

/// Whether voice-learning is enabled for a surface — default ON; the Profiles toggle writes
/// `voice_optout:<key>` = "1" to turn it off.
pub fn voice_learning_on(key: &str) -> bool {
    crate::db::open_default()
        .and_then(|c| crate::db::get_setting(&c, &format!("voice_optout:{key}")))
        .map(|v| v != "1")
        .unwrap_or(true)
}

/// The learned style bullets for `surface` (empty if none yet).
pub fn bullets_for(surface: &str) -> Vec<String> {
    let Some(conn) = crate::db::open_default() else {
        return Vec::new();
    };
    crate::db::style_notes_for(&conn, surface).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debounce_learns_early_then_every_fifth() {
        // learn at 2..=6, then only on multiples of 5
        assert!(!should_learn(1));
        assert!(should_learn(2));
        assert!(should_learn(6));
        assert!(!should_learn(7));
        assert!(!should_learn(9));
        assert!(should_learn(10));
    }
}
