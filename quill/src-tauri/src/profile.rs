// Per-platform profiles: one persona per surface — voice, identity, signature (and, later, a
// memory circle). The profile KEY reuses cognee's dataset key space (domain-aware: LinkedIn in
// ANY browser is one profile, distinct from Gmail-web and native Teams), so a profile and its
// memory dataset are one identity. Keyed accessors sit on top of the settings table.

use std::cell::RefCell;

thread_local! {
    // The focused window's domain for the CURRENT trigger, noted once at handle_trigger start so
    // persona/style all resolve the SAME domain-aware key without each re-walking the AX tree
    // (read_anchors climbs ancestors + may scan for a URL — not free). Set + read on the trigger
    // thread; gen() runs there too (only the loader animation is a separate thread), so it's
    // visible everywhere the persona/sign-off/recall are built.
    static CURRENT_DOMAIN: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Note the focused window's domain at the very start of a trigger (None for native apps).
pub fn note_domain(domain: Option<String>) {
    CURRENT_DOMAIN.with(|d| *d.borrow_mut() = domain);
}

/// The domain noted for the current trigger, if any (lets chat-surface detection reuse the read).
pub fn current_domain() -> Option<String> {
    CURRENT_DOMAIN.with(|d| d.borrow().clone())
}

/// Collapse the app-vs-web variants of ONE platform to a single profile key, so a user's learned
/// voice / identity / signature / memory-circle carry over whether they're in the DESKTOP APP or the
/// BROWSER. Observed: Teams desktop = "app-teams2" but Teams web = "domain-teams-microsoft-com", so
/// the voice learned in one didn't transfer to the other. Non-platform surfaces keep their own key.
pub fn canonical(key: &str) -> String {
    let k = key.to_lowercase();
    const PLATFORMS: &[&str] = &[
        "teams", "whatsapp", "discord", "slack", "telegram", "signal", "outlook", "gmail",
        "linkedin", "instagram", "facebook", "messenger", "reddit",
    ];
    PLATFORMS
        .iter()
        .find(|p| k.contains(*p))
        .map(|p| (*p).to_string())
        .unwrap_or_else(|| key.to_string())
}

/// Canonical profile key for a surface: the domain this trigger noted (web platforms), else the app
/// bundle, then `canonical()`-folded so app and web variants of a platform share one persona.
pub fn key_for(bundle: &str) -> String {
    canonical(&crate::cognee::dataset_for(bundle, current_domain().as_deref()))
}

// ── Per-profile signature ─────────────────────────────────────────────────────
// A per-platform sign-off: full "Best regards,\nJordan Rivera" on Outlook, nothing on a
// chat surface. Stored per profile key; falls back to a name-based default at the call site.

fn sig_key(profile: &str) -> String {
    format!("signature:{}", canonical(profile))
}

/// This profile's sign-off block, or None when unset (caller falls back to the name default).
pub fn signature_for(profile: &str) -> Option<String> {
    let conn = crate::db::open_default()?;
    crate::db::get_setting(&conn, &sig_key(profile)).filter(|s| !s.trim().is_empty())
}

/// Set (or clear) this profile's signature.
pub fn set_signature(profile: &str, sig: &str) {
    if let Some(conn) = crate::db::open_default() {
        let _ = crate::db::set_setting(&conn, &sig_key(profile), sig);
    }
}

/// Platform class for a profile key → sensible defaults + a UI label (E5). Keyword match on the
/// dataset key ("app-outlook", "domain-linkedin-com"). Order matters: mail before chat so a
/// hypothetical "teams-mail" reads as mail; social is checked last.
pub fn class_of(key: &str) -> &'static str {
    let k = key.to_lowercase();
    const MAIL: [&str; 5] = ["outlook", "mail", "gmail", "smartemail", "airmail"];
    const CHAT: [&str; 6] = ["teams", "whatsapp", "discord", "slack", "telegram", "messenger"];
    const SOCIAL: [&str; 6] = ["linkedin", "twitter", "-x-", "instagram", "facebook", "reddit"];
    if MAIL.iter().any(|m| k.contains(m)) {
        "mail"
    } else if CHAT.iter().any(|c| k.contains(c)) {
        "chat"
    } else if SOCIAL.iter().any(|s| k.contains(s)) {
        "social"
    } else {
        "other"
    }
}

// ── Memory circles ────────────────────────────────────────────────────────────
// A circle groups profiles that SHARE memory. Recall for a draft is scoped to the datasets in
// the active profile's circle, so a personal draft never pulls a work fact (and vice-versa).
// Default circle keeps every profile together — the pre-circles behaviour and the cross-app
// "money shot" (Teams fact → Outlook reply) — until the user walls something off.

pub const DEFAULT_CIRCLE: &str = "work";

fn circle_setting(profile: &str) -> String {
    format!("profile_circle:{}", canonical(profile))
}

/// The circle a profile belongs to (default "work").
pub fn circle_of(profile: &str) -> String {
    crate::db::open_default()
        .and_then(|c| crate::db::get_setting(&c, &circle_setting(profile)))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_CIRCLE.to_string())
}

/// Assign a profile to a circle (from the Profiles screen).
pub fn set_circle(profile: &str, circle: &str) {
    let c = circle.trim();
    if let Some(conn) = crate::db::open_default() {
        let _ = crate::db::set_setting(
            &conn,
            &circle_setting(profile),
            if c.is_empty() { DEFAULT_CIRCLE } else { c },
        );
    }
}

/// Pure core: the recall scope for `active_profile`. Returns None (search-all) when NO dataset
/// sits outside the active circle — no walls exist, so avoid a needless include list and keep
/// pre-circles behaviour. Otherwise the explicit member list (always incl. the active profile),
/// which walls the circle BOTH ways.
fn scope(
    all: &[String],
    circle_of_ds: &impl Fn(&str) -> String,
    active_circle: &str,
    active_profile: &str,
) -> Option<Vec<String>> {
    let walls = all.iter().any(|d| circle_of_ds(d) != active_circle);
    if !walls {
        return None;
    }
    let mut members: Vec<String> =
        all.iter().filter(|d| circle_of_ds(d) == active_circle).cloned().collect();
    if !members.iter().any(|m| m == active_profile) {
        members.push(active_profile.to_string());
    }
    Some(members)
}

/// Recall dataset scope for a draft on `active_profile` (None = all). Uses the cached dataset list.
pub fn recall_datasets(active_profile: &str) -> Option<Vec<String>> {
    let all = all_datasets_cached();
    if all.is_empty() {
        return None;
    }
    let active_circle = circle_of(active_profile);
    scope(&all, &|d| circle_of(d), &active_circle, active_profile)
}

/// The sidecar's dataset names, cached 60s (recall is a hot path; the set moves slowly).
fn all_datasets_cached() -> Vec<String> {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    static CACHE: Mutex<Option<(Instant, Vec<String>)>> = Mutex::new(None);
    if let Some((at, v)) = &*CACHE.lock().unwrap_or_else(|p| p.into_inner()) {
        if at.elapsed() < Duration::from_secs(60) {
            return v.clone();
        }
    }
    let v = crate::cognee::list_datasets().unwrap_or_default();
    *CACHE.lock().unwrap_or_else(|p| p.into_inner()) = Some((Instant::now(), v.clone()));
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn key_is_domain_aware_when_noted_else_app() {
        // Web platform: any browser bundle → the DOMAIN profile (the web-blur fix).
        note_domain(Some("linkedin.com".to_string()));
        assert_eq!(key_for("ai.perplexity.comet"), "domain-linkedin-com");
        assert_eq!(key_for("com.google.Chrome"), "domain-linkedin-com");
        // Native apps note no domain → the app profile, matching cognee's dataset key.
        note_domain(None);
        assert_eq!(key_for("com.microsoft.teams2"), "app-teams2");
        assert_eq!(key_for("com.microsoft.Outlook"), "app-outlook");
    }

    #[test]
    fn circle_scope_walls_both_ways_and_noops_when_unwalled() {
        let all = vec![
            "app-outlook".to_string(),
            "app-teams2".to_string(),
            "domain-linkedin-com".to_string(),
            "domain-personal-gmail-com".to_string(),
        ];
        let mut assign: HashMap<&str, &str> = HashMap::new();
        assign.insert("domain-personal-gmail-com", "personal");
        let circ = |d: &str| assign.get(d).copied().unwrap_or(DEFAULT_CIRCLE).to_string();

        // Work draft: everything EXCEPT the walled personal dataset.
        let work = scope(&all, &circ, "work", "app-outlook").unwrap();
        assert!(work.contains(&"app-outlook".to_string()));
        assert!(work.contains(&"app-teams2".to_string()));
        assert!(!work.contains(&"domain-personal-gmail-com".to_string()));

        // Personal draft: ONLY personal — the wall holds the other way too.
        let personal = scope(&all, &circ, "personal", "domain-personal-gmail-com").unwrap();
        assert_eq!(personal, vec!["domain-personal-gmail-com".to_string()]);

        // No walls at all → None (search everything, pre-circles behaviour, magic intact).
        let none_assign = |_d: &str| DEFAULT_CIRCLE.to_string();
        assert!(scope(&all, &none_assign, "work", "app-outlook").is_none());
    }
}
