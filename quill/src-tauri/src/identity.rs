// Auto-built + user-editable identity profile. Grounds the compose/rewrite
// loop in WHO the user is and the work they do, so replies/posts are role-correct and specific —
// the deeper fix for the LinkedIn "congratulated myself" class of error. Local-only; one editable
// markdown blob in settings (synthesised from the memory graph, then the user can correct it).

const KEY_PROFILE: &str = "identity_profile"; // the SHORT grounding blurb injected into prompts
const KEY_DOSSIER: &str = "identity_dossier"; // the RICH multi-section profile the user reads/edits

const IDENTITY_DOSSIER_SYSTEM: &str = "You are writing the user's personal identity profile from \
signals gathered on their own screen. Write in FIRST PERSON, plain markdown, using ONLY what the \
signals support — never invent. Use EXACTLY these sections, each with a '## ' heading, in order:\n\
## Role & focus\n(2-4 sentences: who they are, their role and organisation, and the projects they \
are driving.)\n\
## Tools & stack\n(a short bullet list of the tools, languages and services they actually use.)\n\
## Habits & patterns\n(2-3 sentences on when and how they work, drawn from the activity rhythm \
given. If that signal is thin, write exactly: Not enough signal yet.)\n\
## Communication style\n(2-3 sentences on how they write across platforms, drawn from the \
per-platform voice notes. If thin, write exactly: Not enough signal yet.)\n\
## Grounding\n(4-6 first-person lines another AI could use to write AS this person — name, role, \
main projects, key tools. No sub-headings, no bullets.)";

/// The saved identity profile (user-edited or synthesised), or None if unset/empty.
pub fn profile() -> Option<String> {
    let conn = crate::db::open_default()?;
    crate::db::get_setting(&conn, KEY_PROFILE).filter(|p| !p.trim().is_empty())
}


// ── Per-profile identity ──────────────────────────────────────────────────────
// Who the user is CHANGES by platform (polished "AI Specialist" on LinkedIn vs casual on Teams).
// Stored per profile key ("identity_profile:<key>"); falls back to the global profile so an
// unconfigured platform still gets grounded identity.

fn keyed(key: &str) -> String {
    format!("{KEY_PROFILE}:{key}")
}

/// This profile's identity, or the global one when no platform-specific profile is set.
pub fn profile_for(key: &str) -> Option<String> {
    let conn = crate::db::open_default()?;
    crate::db::get_setting(&conn, &keyed(key))
        .filter(|p| !p.trim().is_empty())
        .or_else(|| crate::db::get_setting(&conn, KEY_PROFILE).filter(|p| !p.trim().is_empty()))
}

/// Per-profile persona block for the prompt (empty when neither profile nor global is set).
pub fn for_prompt_keyed(key: &str) -> String {
    match profile_for(key) {
        Some(p) => format!("About the user (write from THEIR perspective and knowledge):\n{p}"),
        None => String::new(),
    }
}

/// The saved rich dossier (user-edited or synthesised), or None if unset/empty.
pub fn dossier() -> Option<String> {
    let conn = crate::db::open_default()?;
    crate::db::get_setting(&conn, KEY_DOSSIER).filter(|p| !p.trim().is_empty())
}

/// Save the dossier AND re-derive the short grounding blurb that actually goes into draft prompts,
/// so the two tiers never drift. The blurb is the '## Grounding' section (or a fallback trim).
pub fn apply_dossier(md: &str) {
    if let Some(conn) = crate::db::open_default() {
        let _ = crate::db::set_setting(&conn, KEY_DOSSIER, md);
        let _ = crate::db::set_setting(&conn, KEY_PROFILE, &derive_blurb(md));
    }
}

/// Pull the '## Grounding' section out of a dossier as the prompt-injected blurb. Falls back to the
/// first few non-heading lines when the user hand-edited the dossier without that section.
pub fn derive_blurb(dossier: &str) -> String {
    let mut lines = dossier.lines();
    let grounding: String = lines
        .by_ref()
        .skip_while(|l| !l.trim().eq_ignore_ascii_case("## grounding"))
        .skip(1) // the heading itself
        .take_while(|l| !l.trim_start().starts_with("## "))
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if !grounding.trim().is_empty() {
        return grounding;
    }
    // Fallback: first 8 non-heading, non-empty lines.
    dossier
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .take(8)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Prettify a bundle id for the LLM ("com.hnc.Discord" → "Discord", "com.microsoft.teams2" → "teams2").
fn pretty_app(bundle: &str) -> &str {
    bundle.rsplit('.').next().unwrap_or(bundle)
}

/// A compact "when you do what" summary from the snapshot rhythm — the raw material for the
/// dossier's Habits section. Empty when there's too little to say.
fn habits_summary(conn: &rusqlite::Connection) -> String {
    let rows = crate::db::activity_by_hour_app(conn, 30).unwrap_or_default();
    if rows.is_empty() {
        return String::new();
    }
    let mut by_bucket: std::collections::HashMap<String, Vec<(String, i64)>> =
        std::collections::HashMap::new();
    for (bucket, app, n) in rows {
        by_bucket.entry(bucket).or_default().push((app, n));
    }
    let mut out = String::new();
    for b in ["morning", "afternoon", "evening", "night"] {
        if let Some(apps) = by_bucket.get(b) {
            let mut a = apps.clone();
            a.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
            a.truncate(3);
            let list =
                a.iter().map(|(app, n)| format!("{} ({n})", pretty_app(app))).collect::<Vec<_>>().join(", ");
            out.push_str(&format!("- {b}: {list}\n"));
        }
    }
    out
}

/// Synthesise a rich dossier DRAFT from the memory graph, wiki, per-platform voice and activity
/// rhythm. Does NOT save — the caller reviews/edits first (Identity screen), or routes it through a
/// proposal (ambient auto-refresh).
pub fn synth_from_memory() -> Result<String, String> {
    let conn = crate::db::open_default().ok_or("db not ready")?;
    let entities = crate::memory::list_entities(&conn, 40).map_err(|e| e.to_string())?;
    if entities.is_empty() {
        return Err("no memory yet — use the app a while so it can learn, then try again".into());
    }
    let mut ctx = String::new();
    if let Some(name) = crate::settings::user_name() {
        ctx.push_str(&format!("Known name: {name}\n\n"));
    }

    ctx.push_str("## Most-mentioned entities (kind: name — description)\n");
    for e in &entities {
        ctx.push_str(&format!("- {}: {} — {}\n", e.kind, e.name, e.description.as_deref().unwrap_or("")));
    }

    // Distilled wiki summaries — the "what you know" layer, richer than bare entity descriptions.
    if let Ok(pages) = crate::db::list_wiki_pages(&conn, 25) {
        if !pages.is_empty() {
            ctx.push_str("\n## Memory-wiki summaries\n");
            for p in &pages {
                let s: String = p.summary.chars().take(240).collect();
                ctx.push_str(&format!("- {}: {}\n", p.title, s));
            }
        }
    }

    // Per-platform learned voice — the raw material for the Communication style section.
    if let Ok(surfaces) = crate::db::distinct_style_surfaces(&conn) {
        let mut voice = String::new();
        for key in &surfaces {
            let bullets = crate::db::style_notes_for(&conn, key).unwrap_or_default();
            if !bullets.is_empty() {
                voice.push_str(&format!("- On {}: {}\n", key, bullets.join("; ")));
            }
        }
        if !voice.is_empty() {
            ctx.push_str("\n## Per-platform writing voice\n");
            ctx.push_str(&voice);
        }
    }

    // Time-of-day × app rhythm — the raw material for the Habits section.
    let habits = habits_summary(&conn);
    if !habits.is_empty() {
        ctx.push_str("\n## Activity rhythm (time-of-day: apps, capture counts)\n");
        ctx.push_str(&habits);
    }

    crate::llm::chat(IDENTITY_DOSSIER_SYSTEM, &ctx, 0.4)
}

/// Ambient identity refresh (called on a deep quiet tick). Self-throttled to ~daily. Synthesises a
/// fresh dossier and — if it differs from the saved one — queues a REVIEW proposal rather than
/// silently rewriting who the user is. The first-ever dossier is applied directly (nothing to
/// review). Runs llm::chat, so schedule it only when idle.
pub fn ambient_refresh() {
    const KEY_SYNTH_TS: &str = "identity_synth_ts";
    const SYNTH_TTL_SECS: i64 = 24 * 60 * 60;

    let Some(conn) = crate::db::open_default() else {
        return;
    };
    let now = crate::db::now_secs();
    let last =
        crate::db::get_setting(&conn, KEY_SYNTH_TS).and_then(|v| v.parse().ok()).unwrap_or(0);
    if now - last < SYNTH_TTL_SECS {
        return;
    }
    // Stamp BEFORE the (slow, fallible) synth so a failure doesn't retry every tick.
    let _ = crate::db::set_setting(&conn, KEY_SYNTH_TS, &now.to_string());

    let draft = match synth_from_memory() {
        Ok(d) => d,
        Err(_) => return, // not enough memory yet, or LLM busy — try again next window
    };
    let current = crate::db::get_setting(&conn, KEY_DOSSIER).unwrap_or_default();
    if current.trim().is_empty() {
        apply_dossier(&draft);
        crate::db::insert_event("identity", "Built your identity profile", "See it under Identity.");
        println!("[quill] identity: first dossier synthesised");
    } else if draft.trim() != current.trim() && !crate::db::has_proposal(&conn, "identity", "") {
        let _ = crate::db::insert_proposal(&conn, "identity", "", &current, &draft);
        crate::db::insert_event(
            "review",
            "Identity update ready",
            "Review and approve it under Identity.",
        );
        println!("[quill] identity: dossier proposal queued");
    }
}
