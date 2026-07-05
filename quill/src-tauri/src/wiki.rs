// Wiki / distillation layer (P4b): per-entity summarized pages — the browsable, human unit of
// memory (per-entity wiki pages). Each page distills the LOCAL snapshots that mention an entity
// into a short factual profile. Entities come from the folded knowledge graph
// (cognee::graph_view — already alias-folded); content + provenance come from the local FTS
// index. So pages are always grounded in real captures, and distillation is throttle-friendly:
// it runs on the QUIET tick in small batches, converging over time (self-healing, like cognify).

use crate::db::WikiRow;

const PER_ENTITY_SNAPSHOTS: usize = 8;
const REFRESH_TTL_SECS: i64 = 24 * 60 * 60; // re-distill an entity at most once a day
const MIN_TITLE_CHARS: usize = 3;

const SUMMARY_SYSTEM: &str = "You are writing a short factual profile of ONE entity for the \
user's personal memory wiki, using excerpts of the user's own screen that mention it. Write 2–4 \
sentences: what the entity is, the user's relationship to or work with it, and concrete \
specifics (people, projects, dates, decisions) the excerpts support. Third person, dry, no \
preamble, no headings, no markdown. Use ONLY what the excerpts state — never invent. If the \
excerpts are too thin to say anything specific, reply with exactly: SKIP";

/// Entities that shouldn't get a wiki page: dates, emails, @handles, bare numbers — artifacts
/// of pre-P4a extraction (the anti-fragmentation prompt stops new ones; old nodes linger in the
/// graph). Observed live: pages were distilled for '2026-06-28' and an email address.
pub fn is_noise_entity(name: &str) -> bool {
    let t = name.trim();
    if t.contains('@') {
        return true; // emails and @handles
    }
    // Date-like or number-like: strip separators; if what's left is all digits, it's not a topic.
    let core: String = t.chars().filter(|c| c.is_alphanumeric()).collect();
    if !core.is_empty() && core.chars().all(|c| c.is_ascii_digit()) {
        return true;
    }
    false
}

/// URL/id-safe slug (also the fold identity used to skip already-distilled entities).
pub fn slugify(name: &str) -> String {
    let s: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    s.trim_matches('-').replace("--", "-")
}

/// Distill one entity into a wiki page from its local snapshots. None when there's nothing worth
/// saying (no mentions, or the model returns SKIP / meta-noise).
pub fn distill_entity(conn: &rusqlite::Connection, title: &str, kind: &str) -> Option<WikiRow> {
    let hits = crate::db::search_snapshots_fts(conn, title, "", PER_ENTITY_SNAPSHOTS).ok()?;
    if hits.is_empty() {
        return None;
    }
    let snapshot_ids: Vec<i64> = hits.iter().map(|h| h.id).collect();
    let (first_seen, last_seen) = crate::db::snapshot_time_bounds(conn, &snapshot_ids);
    let mut ctx = String::new();
    for h in &hits {
        let prov = h.window_title.as_deref().or(h.domain.as_deref()).unwrap_or(&h.app_bundle);
        let snippet: String = h.text.chars().take(500).collect();
        ctx.push_str(&format!("[{prov}]\n{snippet}\n\n"));
    }
    let user =
        format!("## Entity: {title}\n\n## Excerpts from the user's screen that mention it:\n{ctx}");
    let summary = crate::llm::chat(SUMMARY_SYSTEM, &user, 0.3).ok()?;
    let s = summary.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("skip") || crate::llm::smells_meta(s) {
        return None;
    }
    Some(WikiRow {
        slug: slugify(title),
        title: title.to_string(),
        kind: kind.to_string(),
        aliases: Vec::new(),
        summary: s.to_string(),
        mention_count: snapshot_ids.len() as i64,
        snapshot_ids,
        first_seen,
        last_seen,
        updated_at: crate::db::now_secs(),
    })
}

/// Distill up to `limit` wiki pages for the top graph entities that lack a fresh page. Bounded
/// per call (converges over successive quiet ticks) and gentle on the LLM. Returns how many were
/// written. Runs llm::chat directly (not via cognee) — schedule on QUIET ticks so it never
/// competes with an active keypress.
pub fn refresh(limit: usize) -> usize {
    // One refresh at a time: the manual button and the idle tick raced (observed live —
    // 'acme'/'openai' distilled twice), both reading the fresh-set before either wrote.
    static RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if RUNNING.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return 0;
    }
    let done = refresh_inner(limit);
    RUNNING.store(false, std::sync::atomic::Ordering::SeqCst);
    done
}

fn refresh_inner(limit: usize) -> usize {
    let Some(conn) = crate::db::open_default() else {
        return 0;
    };
    let gv = match crate::cognee::graph_view() {
        Ok(g) => g,
        Err(e) => {
            println!("[quill] wiki: graph unavailable ({e}) — skipping refresh");
            return 0;
        }
    };
    let fresh = crate::db::fresh_wiki_slugs(&conn, REFRESH_TTL_SECS);
    let mut done = 0;
    let mut titles: Vec<String> = Vec::new();
    for node in gv.nodes.iter().filter(|n| !n.cat.is_empty() && n.deg > 0) {
        if done >= limit {
            break;
        }
        if node.label.chars().count() < MIN_TITLE_CHARS
            || is_noise_entity(&node.label)
            || fresh.contains(&slugify(&node.label))
        {
            continue;
        }
        if let Some(page) = distill_entity(&conn, &node.label, &node.cat) {
            let _ = crate::db::upsert_wiki_page(&conn, &page);
            println!(
                "[quill] wiki: distilled '{}' ({} mentions)",
                page.title, page.mention_count
            );
            titles.push(page.title);
            done += 1;
        }
    }
    if done > 0 {
        crate::db::insert_event(
            "wiki",
            &format!("distilled {done} memory page{}", if done == 1 { "" } else { "s" }),
            &titles.join(" · "),
        );
    }
    done
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_is_url_safe_and_stable() {
        assert_eq!(slugify("Jordan Lee Rivera"), "jordan-lee-rivera");
        assert_eq!(slugify("Acme Advanced Solutions"), "acme-advanced-solutions");
        assert_eq!(slugify("  spaced.name!  "), "spaced-name");
    }

    #[test]
    fn noise_entities_get_no_pages() {
        // Observed live: pages distilled for a date and an email — both are noise.
        assert!(is_noise_entity("2026-06-28"));
        assert!(is_noise_entity("sam@example.net"));
        assert!(is_noise_entity("@jordan_r"));
        assert!(is_noise_entity("12:30"));
        // Real entities pass.
        assert!(!is_noise_entity("Jordan Lee Rivera"));
        assert!(!is_noise_entity("100xdevs")); // has letters — a real community name
        assert!(!is_noise_entity("lms-main"));
    }
}
