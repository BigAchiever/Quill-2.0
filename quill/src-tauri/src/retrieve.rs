// Memory retrieval for the ⌥-loop and the panel ask flow: ask cognee's graph+vector recall
// for relevant earlier knowledge and format a compact context block. This replaced both the
// dumb-recency handoff (injected unrelated content — "right topic, wrong slice") and the
// hand-rolled local-embeddings retrieval (superseded at the cognee cutover).
//
// Retrieval strategy is ROUTED, not defaulted — but every non-default route must first pass
// the bench gate (tools/search-bench.py) on our real graph: grounded answers AND p50 ≤ 8s
// inside the 10s recall budget. Evidence before knobs.

use crate::cognee::{self, RecallOpts};

const MIN_QUERY_CHARS: usize = 24; // too little to retrieve meaningfully on (drafting only)
const PER_SNAPSHOT_CHARS: usize = 700;
const TOTAL_CHARS: usize = 2200;
// Observed live (Teams): with topK 6, ALL top hits were captures of the current surface and
// the same-surface exclusion silently emptied the block. 12 leaves cross-surface survivors;
// CHUNKS retrieval is embedding-only so the extra candidates cost nothing, and TOTAL_CHARS
// still caps what reaches the prompt.
const TOP_K: u32 = 12;

/// The recall answerer's persona. NO_MATCH gives the model a legal way out — observed live:
/// without one, no-match queries produce 7-char stub answers that waste prompt budget. The
/// client-side filter below still backstops it (prompt obedience is hoped for, never assumed).
const QUILL_MEMORY_PROMPT: &str = "You are the user's ambient memory. Answer with concrete \
facts — names, dates, decisions, commitments. If the provided context does not answer the \
question, reply exactly NO_MATCH.";

// ── Bench-gated strategy table ────────────────────────────────────────────────
// Flip these ONLY with committed evidence from our own sidecar (tools/search-bench.md).

/// Retrieval strategy for BOTH recall lanes. Measured July 3 on a quiet sidecar:
/// GRAPH_COMPLETION > 40s (it runs a second LLM inside cognee — every production recall was
/// silently timing out against the 10s budget); CHUNKS = 2.3–3.1s, embedding-only, returns the
/// raw remembered excerpts with dataset provenance. Architecture note: cognee retrieves,
/// quill's own LLM synthesizes — one LLM pass, not two.
const RETRIEVAL_STRATEGY: Option<&'static str> = Some("CHUNKS");
/// Route time-scoped queries to cognee's TEMPORAL search. Pending bench — TEMPORAL may need
/// a temporally-cognified graph to return anything at all.
const TEMPORAL_ROUTING: bool = false;

/// Build a memory context block for the current trigger, or None when memory is unavailable /
/// nothing relevant is found. Degrades to screen-only drafting (None) when the sidecar is down
/// or slow — memory is an enhancement, never a hostage-taker. `exclude_bundle` = the current
/// app (its screen is already the primary context).
pub fn memory_block(query: &str, exclude_bundle: &str) -> Option<String> {
    let q = query.trim();
    if q.chars().count() < MIN_QUERY_CHARS {
        return None;
    }
    // Local lexical lane (FTS5): always-available, populates working_memory with local snapshot
    // provenance. Runs regardless of the sidecar.
    let local = local_recall(q, exclude_bundle);

    // Memory circle: the active platform's profile scopes recall to its circle's datasets, so a
    // personal draft never pulls a work fact (and vice-versa). None = no walls → search all
    // (pre-circles behaviour). key_for uses the domain noted this trigger (we run on that thread).
    let profile = crate::profile::key_for(exclude_bundle);
    let mut opts = route_opts(q);
    opts.datasets = crate::profile::recall_datasets(&profile);
    if let Some(ds) = &opts.datasets {
        println!("[quill] recall scoped to circle: {ds:?}");
    }
    let strategy = opts.search_type.unwrap_or("GRAPH_COMPLETION");
    let facts = cognee::recall_opts(&clip(q, 1500), &opts).unwrap_or_else(|e| {
        println!("[quill] cognee recall unavailable ({e}) — local FTS lane covers it");
        Vec::new()
    });
    // cognee facts are distilled + clean → preferred on the happy path. When the sidecar is thin
    // or down, the local FTS hits (raw captures with provenance) keep recall alive — resilience
    // without injecting noisy captures when cognee already answered.
    format_facts(&facts, &profile, strategy).or_else(|| local_block(&local))
}

/// Memory context for the panel ask flow. Unlike drafting recall there is no minimum length
/// (panel questions are short — "what is quill?") and no surface to exclude.
pub fn ask_block(question: &str) -> Option<String> {
    let q = question.trim();
    if q.is_empty() {
        return None;
    }
    let mut blocks: Vec<String> = Vec::new();

    // 0z. TIME-WINDOW activity, computed from the LOCAL clock in SQL (not the LLM) — the
    //     authoritative answer to "what did I do between 3-5 PM / this morning / yesterday". Reports
    //     an empty window honestly instead of letting the model answer from a different day.
    if let Some(b) = window_block(q) {
        blocks.push(b);
    }

    // 0a. FACTUAL activity aggregate — "what sites/apps did I visit today" is a COUNT over captured
    //     domains, not a semantic-recall question. Answer it from structured data so it can't
    //     hallucinate google.com or surface old prominent browsing (Clash/Gold-Pass) over today's.
    if let Some(b) = today_activity_block(q) {
        blocks.push(b);
    }

    // 0. MOST RECENT raw mentions (recency-ordered, timestamped). The wiki + cognee lanes below are
    //    distilled/semantic — great for "summarize X", but they surface PROMINENT facts, not the
    //    freshest ones, so "what's the latest with X" used to answer with old context. This lane
    //    fixes that: the newest snapshots that match the question, tagged with how long ago.
    if let Some(b) = recent_block(q) {
        blocks.push(b);
    }

    // 1. Distilled wiki pages — the summarized "what quill knows about X" layer. Best for
    //    "summarize the LMS project": the lms page's own summary answers it directly, locally,
    //    without the sidecar. This is why the wiki exists — chat uses it now.
    if let Some(conn) = crate::db::open_default() {
        let pages = crate::db::search_wiki_pages(&conn, q, 4);
        if !pages.is_empty() {
            let mut b = String::from("--- What you know about the entities in this question ---\n");
            for (title, summary) in pages {
                b.push_str(&format!("## {title}\n{}\n\n", clip(summary.trim(), 600)));
            }
            blocks.push(b);
        }
    }

    // 2. cognee semantic recall — best-effort (may time out under load; that's fine, we have the others).
    let opts = RecallOpts {
        search_type: RETRIEVAL_STRATEGY,
        top_k: Some(TOP_K),
        system_prompt: None,
        datasets: None,
    };
    match cognee::recall_opts(&clip(q, 1500), &opts) {
        Ok(facts) => {
            if let Some(b) = format_facts(&facts, "", RETRIEVAL_STRATEGY.unwrap_or("CHUNKS")) {
                blocks.push(b);
            }
        }
        Err(e) => println!("[quill] ask cognee recall unavailable ({e}) — using wiki + local FTS"),
    }

    // 3. Local FTS over raw snapshots — always available, sidecar-free (the same lane drafts use).
    if let Some(b) = local_block(&local_recall(q, "")) {
        blocks.push(b);
    }

    if blocks.is_empty() {
        None
    } else {
        Some(blocks.join("\n"))
    }
}

const RECALL_HEADER: &str =
    "--- Relevant knowledge from your earlier activity (background only) ---\n";

/// Local FTS recall + working_memory revival. Runs on the trigger thread; always available. The
/// hits (real snapshot rows with provenance) populate working_memory so the panel and exact
/// recall can cite local rows — the local-provenance the cognee cutover had dropped.
fn local_recall(query: &str, exclude_bundle: &str) -> Vec<crate::db::LocalHit> {
    let Some(conn) = crate::db::open_default() else {
        return Vec::new();
    };
    let hits = crate::db::search_snapshots_fts(&conn, query, exclude_bundle, 6).unwrap_or_default();
    // bm25 is lower-is-better; store its negation as a positive relevance for the panel.
    let wm: Vec<(i64, f32)> = hits.iter().map(|h| (h.id, (-h.score) as f32)).collect();
    let _ = crate::db::set_working_memory(&conn, &wm);
    hits
}

/// A context block built from local FTS hits — the sidecar-free fallback. Raw captures, so
/// clipped and provenance-tagged; None when nothing usable.
/// Parse a natural-language time window from the question into a concrete [start,end) epoch range,
/// computed from the LOCAL clock (deterministic — no LLM). Handles "between H and H (am/pm)",
/// "this morning/afternoon/evening", "tonight", "yesterday", "last hour", "right now", "today".
fn parse_window(conn: &rusqlite::Connection, ql: &str) -> Option<(i64, i64, String)> {
    let today = crate::db::local_date(conn, 0)?;
    let at = |t: &str| crate::db::local_epoch(conn, &format!("{today} {t}"));
    let now = crate::db::now_secs();

    // Explicit "between 3 and 5 pm" / "3 to 5 pm" / "3-5pm".
    if ql.contains("between") || ql.contains(" to ") || ql.contains('-') {
        let pm = ql.contains("pm") || ql.contains("p.m");
        let am = ql.contains("am") || ql.contains("a.m");
        let nums: Vec<u32> = ql
            .split(|c: char| !c.is_ascii_digit())
            .filter_map(|s| s.parse::<u32>().ok())
            .filter(|&n| (1..=12).contains(&n))
            .collect();
        if nums.len() >= 2 {
            let to24 = |h: u32| if pm && h < 12 { h + 12 } else if am && h == 12 { 0 } else { h };
            let (s, e) = (to24(nums[0]), to24(nums[1]));
            if let (Some(a), Some(b)) = (at(&format!("{s:02}:00:00")), at(&format!("{e:02}:00:00"))) {
                if b > a {
                    let mer = if pm { " PM" } else if am { " AM" } else { "" };
                    return Some((a, b, format!("{}–{}{mer} today", nums[0], nums[1])));
                }
            }
        }
    }

    if ql.contains("this morning") {
        return Some((at("05:00:00")?, at("12:00:00")?, "this morning".into()));
    }
    if ql.contains("this afternoon") {
        return Some((at("12:00:00")?, at("17:00:00")?, "this afternoon".into()));
    }
    if ql.contains("this evening") || ql.contains("tonight") {
        return Some((at("17:00:00")?, now, "this evening".into()));
    }
    if ql.contains("last hour") || ql.contains("past hour") {
        return Some((now - 3600, now, "the last hour".into()));
    }
    if ql.contains("right now") || ql.contains("just now") || ql.contains("last few minutes") {
        return Some((now - 1200, now, "the last ~20 minutes".into()));
    }
    if ql.contains("yesterday") {
        let y = crate::db::local_date(conn, -1)?;
        return Some((crate::db::local_epoch(conn, &format!("{y} 00:00:00"))?, at("00:00:00")?, "yesterday".into()));
    }
    if ql.contains("today") || ql.contains("so far") {
        return Some((at("00:00:00")?, now, "today".into()));
    }
    None
}

/// Deterministic activity for a locally-computed time window — the fix for temporal questions
/// ("what did I do between 3–5 PM"), so the answer comes from the ACTUAL rows in that window (and
/// honestly reports an empty window) instead of the LLM guessing what "today" means.
fn window_block(question: &str) -> Option<String> {
    let conn = crate::db::open_default()?;
    let (start, end, label) = parse_window(&conn, &question.to_lowercase())?;
    let (doms, apps) = crate::db::window_activity(&conn, start, end);
    if doms.is_empty() && apps.is_empty() {
        println!("[quill] recall: window '{label}' has NO captures");
        return Some(format!(
            "--- FACTUAL: there are NO captures for {label} (nothing was on screen, or Quill wasn't \
running then). Tell the user plainly that you have no record of that window — do NOT answer from a \
different day. ---\n"
        ));
    }
    let mut b = format!(
        "--- FACTUAL activity for {label} (counted from the captures in that exact window — answer \
'what did I do {label}' from THIS list ONLY) ---\n"
    );
    if !doms.is_empty() {
        b.push_str("Websites:\n");
        for (d, n) in doms.iter().take(12) {
            b.push_str(&format!("- {d} ({n})\n"));
        }
    }
    if !apps.is_empty() {
        b.push_str("Apps:\n");
        for (a, n) in apps.iter().take(10) {
            b.push_str(&format!("- {a} ({n})\n"));
        }
    }
    println!("[quill] recall: window '{label}' → {} domains, {} apps", doms.len(), apps.len());
    Some(b)
}

/// Deterministic "today's activity" facts (domains + apps), for aggregate questions like "what
/// websites did I visit today". Gated to activity questions so it doesn't add noise elsewhere.
fn today_activity_block(question: &str) -> Option<String> {
    let ql = question.to_lowercase();
    let is_activity = [
        "website", "websites", "site", "sites", "visit", "browse", "browsing", "domain", "url",
        "apps", "webpage", "web page",
    ]
    .iter()
    .any(|k| ql.contains(k));
    if !is_activity {
        return None;
    }
    let conn = crate::db::open_default()?;
    let domains = crate::db::domains_today(&conn).unwrap_or_default();
    let apps = crate::db::apps_today(&conn).unwrap_or_default();
    if domains.is_empty() && apps.is_empty() {
        return None;
    }
    let mut b = String::from(
        "--- FACTUAL today's activity (from captured page domains/apps, most-visited first — answer \
'what did I visit/use today' from THIS list ONLY; do not add sites from older memory) ---\n",
    );
    if !domains.is_empty() {
        b.push_str("Websites today:\n");
        for (d, n) in domains.iter().take(20) {
            b.push_str(&format!("- {d} ({n})\n"));
        }
    }
    if !apps.is_empty() {
        b.push_str("Apps today:\n");
        for (a, n) in apps.iter().take(12) {
            b.push_str(&format!("- {a} ({n})\n"));
        }
    }
    println!("[quill] recall: today-activity block ({} domains, {} apps)", domains.len(), apps.len());
    Some(b)
}

/// Most-recent snapshots matching the query, newest first, each tagged with how long ago — the
/// recency lane that grounds "latest"/"recent" questions in the ACTUAL recent conversation.
fn recent_block(query: &str) -> Option<String> {
    let conn = crate::db::open_default()?;
    let hits = crate::db::search_snapshots_recent(&conn, query, 24).unwrap_or_default();
    if hits.is_empty() {
        return None;
    }
    // A conversation ("latest with a person") lives in MESSAGING apps — so prefer communication
    // surfaces (chat/mail/social) over a terminal/IDE/browser that merely mentions the name (e.g.
    // THIS debugging session capturing us talk about the very person). Only fall back to all
    // surfaces when no comm surface matched, so non-conversation questions still work.
    let is_comm = |h: &crate::db::LocalHit| {
        let ds = crate::cognee::dataset_for(&h.app_bundle, h.domain.as_deref());
        matches!(crate::profile::class_of(&ds), "chat" | "mail" | "social")
    };
    let comm: Vec<&(i64, crate::db::LocalHit)> = hits.iter().filter(|(_, h)| is_comm(h)).collect();
    let chosen: Vec<&(i64, crate::db::LocalHit)> =
        if comm.is_empty() { hits.iter().collect() } else { comm };

    let now = crate::db::now_secs();
    let mut block = String::from(
        "--- MOST RECENT mentions (newest first — prefer these for 'latest'/'recent' questions) ---\n",
    );
    let (mut used, mut total) = (0usize, 0usize);
    for entry in &chosen {
        let ts = entry.0;
        let h = &entry.1;
        let t = h.text.trim();
        if t.chars().count() < 24 {
            continue;
        }
        let snippet = clip(t, PER_SNAPSHOT_CHARS);
        let prov = h.window_title.as_deref().or(h.domain.as_deref()).unwrap_or(&h.app_bundle);
        block.push_str(&format!("[{} · {prov}]\n{snippet}\n\n", rel_time(now - ts)));
        total += snippet.chars().count();
        used += 1;
        if total >= TOTAL_CHARS || used >= 4 {
            break;
        }
    }
    if used == 0 {
        return None;
    }
    println!("[quill] recall: {used} RECENT mention(s) surfaced (recency lane, comm-preferred)");
    Some(block)
}

fn rel_time(secs: i64) -> String {
    let s = secs.max(0);
    if s < 3600 {
        format!("{}m ago", s / 60)
    } else if s < 86400 {
        format!("{}h ago", s / 3600)
    } else {
        format!("{}d ago", s / 86400)
    }
}

fn local_block(hits: &[crate::db::LocalHit]) -> Option<String> {
    let mut block = String::from(RECALL_HEADER);
    let (mut used, mut total) = (0usize, 0usize);
    for h in hits {
        let t = h.text.trim();
        if t.chars().count() < 24 {
            continue;
        }
        let snippet = clip(t, PER_SNAPSHOT_CHARS);
        let prov = h
            .window_title
            .as_deref()
            .or(h.domain.as_deref())
            .unwrap_or(&h.app_bundle);
        block.push_str(&format!("[{prov}]\n{snippet}\n\n"));
        total += snippet.chars().count();
        used += 1;
        if total >= TOTAL_CHARS || used >= 4 {
            break;
        }
    }
    if used == 0 {
        return None;
    }
    println!("[quill] recall: {used} local fact(s), {total} chars (FTS, sidecar-free)");
    Some(block)
}

/// Panel memory search: raw recalled excerpts with provenance — the SAME retrieval path the
/// drafts use, so the panel shows the truth about what memory would deliver, not a demo view.
pub fn search_memory(query: &str, k: u32) -> Result<Vec<cognee::Fact>, String> {
    let opts = RecallOpts {
        search_type: RETRIEVAL_STRATEGY,
        top_k: Some(k),
        system_prompt: None,
        datasets: None, // panel search spans all memory
    };
    cognee::recall_opts(&clip(query.trim(), 1500), &opts)
}

/// Strategy routing for a drafting recall: time-scoped queries go to TEMPORAL (once gated
/// in); everything else stays on the graph-completion baseline that passed live testing.
fn route_opts(query: &str) -> RecallOpts {
    let search_type = if TEMPORAL_ROUTING && looks_time_scoped(query) {
        Some("TEMPORAL")
    } else {
        RETRIEVAL_STRATEGY
    };
    // The memory persona only matters for completion strategies (kept for the TEMPORAL route);
    // CHUNKS is embedding-only and ignores it.
    let system_prompt =
        if search_type == Some("CHUNKS") { None } else { Some(QUILL_MEMORY_PROMPT) };
    // datasets (memory circle) is filled by the caller (memory_block) — it needs the surface.
    RecallOpts { search_type, top_k: Some(TOP_K), system_prompt, datasets: None }
}

/// Does the query ask about a time window? Word-level matching, never substring — "jun"
/// must not fire inside "junction".
fn looks_time_scoped(q: &str) -> bool {
    const MARKERS: [&str; 5] = ["yesterday", "today", "tonight", "ago", "since"];
    const MONTHS: [&str; 12] = [
        "january", "february", "march", "april", "may", "june", "july", "august",
        "september", "october", "november", "december",
    ];
    const SPAN_HEAD: [&str; 3] = ["last", "this", "past"];
    const SPAN_TAIL: [&str; 8] =
        ["week", "weeks", "month", "months", "year", "years", "night", "days"];
    let lower = q.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '/')
        .filter(|w| !w.is_empty())
        .collect();
    for (i, w) in words.iter().enumerate() {
        if MARKERS.contains(w) || MONTHS.contains(w) {
            return true;
        }
        if SPAN_HEAD.contains(w)
            && words.get(i + 1).is_some_and(|n| SPAN_TAIL.contains(n))
        {
            return true;
        }
        // Ordinal day: "24th", "1st", "2nd", "3rd" ("month" splits to non-digits → no fire)
        if w.len() >= 3 {
            let (digits, suffix) = w.split_at(w.len() - 2);
            if ["st", "nd", "rd", "th"].contains(&suffix)
                && digits.chars().all(|c| c.is_ascii_digit())
            {
                return true;
            }
        }
        // Numeric date ("06/24") or a plain year ("2026")
        if w.contains('/') && w.chars().all(|c| c.is_ascii_digit() || c == '/') {
            return true;
        }
        if w.len() == 4
            && (w.starts_with("19") || w.starts_with("20"))
            && w.chars().all(|c| c.is_ascii_digit())
        {
            return true;
        }
    }
    false
}

/// Filter + format recalled facts into the context block; None when nothing survives.
fn format_facts(facts: &[cognee::Fact], exclude_ds: &str, strategy: &str) -> Option<String> {
    let mut block = String::from(RECALL_HEADER);
    let mut used = 0usize;
    let mut total = 0usize;
    let mut same_surface = 0usize;
    for f in facts {
        // Skip non-answers and echoes of the surface the user is already looking at.
        // Observed live: GRAPH_COMPLETION returns tiny stub answers (7 chars) on no-match
        // queries — injecting those wastes prompt budget and confuses the draft model.
        // NO_MATCH is the sentinel QUILL_MEMORY_PROMPT asks for.
        let t = f.text.trim();
        let lower = t.to_lowercase();
        if !exclude_ds.is_empty() && f.dataset.as_deref() == Some(exclude_ds) {
            same_surface += 1;
            continue;
        }
        if t.chars().count() < 24
            || lower.contains("no_match")
            || lower.contains("please provide the context")
            || lower.contains("i don't know")
            || lower.contains("i do not know")
            || lower.contains("not enough context")
            || lower.contains("no information")
        {
            continue;
        }
        let snippet = clip(t, PER_SNAPSHOT_CHARS);
        match f.dataset.as_deref() {
            Some(ds) => block.push_str(&format!("[{ds}]\n{snippet}\n\n")),
            None => block.push_str(&format!("{snippet}\n\n")),
        }
        total += snippet.chars().count();
        used += 1;
        if total >= TOTAL_CHARS {
            break;
        }
    }
    if used == 0 {
        // NEVER silent: this exact path hid a fully-working recall behind an empty block live
        // (all top hits were same-surface captures and the exclusion ate every one of them).
        println!(
            "[quill] recall: 0 of {} result(s) usable ({same_surface} same-surface) via {strategy} — screen-only",
            facts.len()
        );
        return None;
    }
    println!("[quill] recall: {used} fact(s), {total} chars via {strategy}");
    Some(block)
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max).collect();
        t.push('…');
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_scoped_queries_detected() {
        assert!(looks_time_scoped(
            "What changed in the legal assistant since June 24?"
        ));
        assert!(looks_time_scoped("what did I do yesterday"));
        assert!(looks_time_scoped("summarize last week's threads"));
        assert!(looks_time_scoped("meetings on 06/24"));
        assert!(looks_time_scoped("what happened on the 24th"));
    }

    #[test]
    fn plain_queries_not_time_scoped() {
        assert!(!looks_time_scoped("what did Sam confirm about the meeting?"));
        // "jun" inside a word must not fire; "month" ends in "th" but has no digits.
        assert!(!looks_time_scoped("junction box wiring notes for the month-end"));
        assert!(!looks_time_scoped("who commented on my LinkedIn post"));
    }

    #[test]
    fn routing_uses_the_measured_chunks_strategy() {
        // CHUNKS passed the latency gate (2.3–3.1s vs GRAPH_COMPLETION >40s, tools/
        // search-bench.md); TEMPORAL stays gated off. CHUNKS is embedding-only, so no
        // memory persona rides along.
        let o = route_opts("What changed since June 24?");
        assert_eq!(
            o.search_type,
            if TEMPORAL_ROUTING { Some("TEMPORAL") } else { Some("CHUNKS") }
        );
        assert_eq!(o.top_k, Some(TOP_K));
        assert!(o.system_prompt.is_none());
    }
}
