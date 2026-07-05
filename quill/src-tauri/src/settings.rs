// Phase 2.3: user-controllable privacy state — a global pause switch and a
// user-editable app-exclusion list — persisted in the settings table and mirrored
// in process globals so the hot capture/trigger paths read them lock-cheaply.
//
// Default (built-in) exclusions live in `app::EXCLUDED_PREFIXES`; this module adds
// the USER's extra exclusions on top. Both gates are consulted by `app::is_excluded`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::RwLock;

const KEY_PAUSED: &str = "paused";
const KEY_USER_EXCLUSIONS: &str = "user_exclusions";
const KEY_USER_NAME: &str = "user_name";

static PAUSED: AtomicBool = AtomicBool::new(false);
static USER_EXCLUSIONS: RwLock<Vec<String>> = RwLock::new(Vec::new());
static USER_NAME: RwLock<String> = RwLock::new(String::new());

/// Is ambient capture paused? (The on-demand Option loop is unaffected.)
pub fn is_paused() -> bool {
    PAUSED.load(Ordering::Relaxed)
}

/// Pause/resume ambient capture and persist the choice.
pub fn set_paused(paused: bool) {
    PAUSED.store(paused, Ordering::Relaxed);
    persist(KEY_PAUSED, if paused { "1" } else { "0" });
}

/// The user's extra exclusion prefixes (does not include built-in defaults).
pub fn user_exclusions() -> Vec<String> {
    USER_EXCLUSIONS.read().map(|v| v.clone()).unwrap_or_default()
}

/// Replace the user's exclusion list and persist it. Excluding an app is a privacy statement —
/// with cognee it also FORGETS that app's memory dataset (the graph nodes visibly disappear).
pub fn set_user_exclusions(list: Vec<String>) {
    // forget() lane (M5): diff against the previous list; newly-excluded bundles lose their memory.
        {
        let old = user_exclusions();
        for added in list.iter().filter(|p| !p.trim().is_empty() && !old.contains(*p)) {
            let ds = crate::cognee::dataset_for(added, None);
            std::thread::spawn(move || match crate::cognee::forget_dataset(&ds) {
                Ok(()) => println!("[quill] forget: dropped memory dataset {ds}"),
                Err(e) => println!("[quill] forget skipped: {e}"),
            });
        }
    }

    let json = serde_json::to_string(&list).unwrap_or_else(|_| "[]".to_string());
    if let Ok(mut w) = USER_EXCLUSIONS.write() {
        *w = list;
    }
    persist(KEY_USER_EXCLUSIONS, &json);
}

/// Does `bundle` match a USER-added exclusion prefix?
pub fn is_user_excluded(bundle: &str) -> bool {
    USER_EXCLUSIONS
        .read()
        .map(|v| matches_any_prefix(bundle, &v))
        .unwrap_or(false)
}

/// The user's display name — who quill writes AS — or None if unset.
pub fn user_name() -> Option<String> {
    USER_NAME
        .read()
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Set the user's display name and persist it.
pub fn set_user_name(name: String) {
    if let Ok(mut w) = USER_NAME.write() {
        *w = name.clone();
    }
    persist(KEY_USER_NAME, &name);
}

/// Pure prefix-match: true if `bundle` starts with any non-empty prefix in `prefixes`.
fn matches_any_prefix(bundle: &str, prefixes: &[String]) -> bool {
    prefixes
        .iter()
        .any(|p| !p.is_empty() && bundle.starts_with(p.as_str()))
}

/// Load persisted settings into the process globals (call once at startup, after the
/// DB path is set). Missing keys keep their defaults (not paused, no user exclusions).
pub fn load_from_db() {
    let Some(conn) = crate::db::open_default() else {
        return;
    };
    if let Some(v) = crate::db::get_setting(&conn, KEY_PAUSED) {
        PAUSED.store(v == "1", Ordering::Relaxed);
    }
    if let Some(json) = crate::db::get_setting(&conn, KEY_USER_EXCLUSIONS) {
        if let Ok(list) = serde_json::from_str::<Vec<String>>(&json) {
            if let Ok(mut w) = USER_EXCLUSIONS.write() {
                *w = list;
            }
        }
    }
    if let Some(name) = crate::db::get_setting(&conn, KEY_USER_NAME) {
        if let Ok(mut w) = USER_NAME.write() {
            *w = name;
        }
    }
}

/// Best-effort persist of a single setting (no-op if the DB isn't ready, e.g. tests).
fn persist(key: &str, value: &str) {
    if let Some(conn) = crate::db::open_default() {
        if let Err(e) = crate::db::set_setting(&conn, key, value) {
            eprintln!("[quill] settings persist failed ({key}): {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_match_respects_nonempty_prefixes() {
        let prefixes = vec!["com.hnc.Discord".to_string(), "com.apple.mail".to_string()];
        assert!(matches_any_prefix("com.hnc.Discord", &prefixes));
        assert!(matches_any_prefix("com.apple.mail.compose", &prefixes)); // prefix
        assert!(!matches_any_prefix("com.tinyspeck.slackmacgap", &prefixes));
    }

    #[test]
    fn empty_prefixes_never_match() {
        assert!(!matches_any_prefix("com.hnc.Discord", &[]));
        // an empty-string prefix must NOT match everything
        assert!(!matches_any_prefix("anything", &["".to_string()]));
    }
}
