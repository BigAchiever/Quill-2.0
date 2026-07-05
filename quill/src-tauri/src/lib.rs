// quill app entry: non-activating overlay panel + global right-Option trigger + ambient
// capture. The panel floats over other apps WITHOUT stealing focus from the user's field.

use tauri::{Emitter, Manager};
use tauri_nspanel::cocoa::appkit::NSWindowCollectionBehavior;
use tauri_nspanel::WebviewWindowExt;

mod app;
mod ax;
mod capture;
mod chronicle;
mod cognee;
mod consolidate;
mod db;
mod identity;
mod inject;
mod llm;
mod memory;
mod profile;
mod retrieve;
mod settings;
mod style;
mod trigger;
mod util;
mod wiki;

// NSWindowStyleMaskNonactivatingPanel — the key bit that makes the panel
// never become the key/active window (so focus stays with the underlying app).
const NONACTIVATING_PANEL_MASK: i32 = 1 << 7;

// Render OVER the menu bar / notch — at the screen-saver/shielding tier (1000), well above
// the menu bar (24) and pop-up menus (101) — so the pill sits IN the menu-bar row.
const PANEL_LEVEL: i32 = 1000;

/// Today's coarse activity timeline ("what was I working on?"). Reads the last 24h of
/// snapshots and groups them into per-app sessions (a >5min gap starts a new session).
/// Pure read — no mutation, no egress.
#[tauri::command]
fn chronicle_today() -> Result<Vec<chronicle::Segment>, String> {
    const DAY_SECS: i64 = 24 * 60 * 60;
    const SESSION_GAP_SECS: i64 = 5 * 60;
    let conn = db::open_default().ok_or_else(|| "db not ready".to_string())?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    chronicle::build(&conn, now - DAY_SECS, SESSION_GAP_SECS).map_err(|e| e.to_string())
}

/// Privacy controls (Phase 2.3): pause ambient capture and edit the exclusion list.
#[tauri::command]
fn get_paused() -> bool {
    settings::is_paused()
}

#[tauri::command]
fn set_paused(paused: bool) {
    settings::set_paused(paused);
}

#[tauri::command]
fn get_exclusions() -> Vec<String> {
    settings::user_exclusions()
}

#[tauri::command]
fn set_exclusions(list: Vec<String>) {
    settings::set_user_exclusions(list);
}

/// Identity: who quill writes AS (so replies are role-correct — thank vs congratulate).
#[tauri::command]
fn get_user_name() -> Option<String> {
    settings::user_name()
}

#[tauri::command]
fn set_user_name(name: String) {
    settings::set_user_name(name);
}

/// Dynamic chat starters, generated from the user's OWN memory (the cognee knowledge graph) instead
/// of a hardcoded list — so the suggestions name real projects/people the user actually works with.
/// Falls back to sensible generics when the graph is thin/unavailable. Off the UI thread.
#[tauri::command]
async fn chat_starters() -> Vec<String> {
    tauri::async_runtime::spawn_blocking(build_starters).await.unwrap_or_else(|_| default_starters())
}

fn default_starters() -> Vec<String> {
    vec![
        "What did I work on today?".to_string(),
        "Who did I talk to recently?".to_string(),
        "What's on my plate this week?".to_string(),
    ]
}

/// The highest-degree (most-connected) entity in a category, Title-Cased for display.
fn top_entity(gv: &cognee::GraphView, cat: &str) -> Option<String> {
    gv.nodes
        .iter()
        .filter(|n| n.cat == cat)
        .max_by_key(|n| n.deg)
        .map(|n| {
            n.label
                .split_whitespace()
                .map(|w| {
                    let mut c = w.chars();
                    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
}

fn build_starters() -> Vec<String> {
    let mut out: Vec<String> = vec!["What did I work on today?".to_string()];
    if let Ok(gv) = cognee::graph_view() {
        if let Some(p) = top_entity(&gv, "Projects") {
            out.push(format!("Summarize {p}"));
        }
        if let Some(person) = top_entity(&gv, "People") {
            out.push(format!("What's the latest with {person}?"));
        }
        if out.len() < 3 {
            if let Some(o) = top_entity(&gv, "Orgs") {
                out.push(format!("What's going on with {o}?"));
            }
        }
        if out.len() < 3 {
            if let Some(t) = top_entity(&gv, "Tools") {
                out.push(format!("What have I been doing with {t}?"));
            }
        }
    }
    // Pad to 3 with generics not already chosen.
    for d in ["Who did I talk to recently?", "What's on my plate this week?", "Catch me up on my week"] {
        if out.len() >= 3 {
            break;
        }
        if !out.iter().any(|s| s == d) {
            out.push(d.to_string());
        }
    }
    out.truncate(3);
    out
}

/// Panel chat: answer a question grounded in recent captured activity (off the UI thread).
#[tauri::command]
async fn ask(message: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || ask_blocking(&message))
        .await
        .map_err(|e| e.to_string())?
}

fn ask_blocking(message: &str) -> Result<String, String> {
    let conn = db::open_default().ok_or_else(|| "db not ready".to_string())?;
    let recents = db::recent_snapshots(&conn, "", 4, 30 * 60).unwrap_or_default();
    // Long-term memory first (cognee graph), recent screen activity after. The panel used to
    // answer from the last 30 minutes of snapshots only — blind to everything memory knows.
    let memory = retrieve::ask_block(message).unwrap_or_default();
    let recent_ctx = recents
        .iter()
        .map(|r| format!("[{}]\n{}", r.app_bundle, r.text))
        .collect::<Vec<_>>()
        .join("\n\n");
    let context = if memory.is_empty() {
        recent_ctx
    } else {
        format!("{memory}\n{recent_ctx}")
    };
    // Anchor the model in the CURRENT time — without this it has no idea what "today" means and
    // treats dates found inside the captured text as today (observed: answered a "today" question
    // with July-2 activity). This makes "today"/"this morning"/"now"/"recently" resolve correctly.
    let now = db::now_local(&conn);
    let mut persona = settings::user_name()
        .map(|n| format!("The user's name is {n}. "))
        .unwrap_or_default();
    persona.push_str(&format!(
        "RIGHT NOW it is {now}. Resolve 'today', 'tonight', 'this morning/afternoon', 'now', and \
'recently' relative to THIS moment. Dates and times that appear INSIDE the captured text are when \
those events happened — never assume they are today. If the captures don't cover the asked-about \
time window, say so plainly rather than answering from a different day."
    ));
    llm::answer(message, &context, &persona)
}

/// Size + dock the panel in the menu-bar strip (over the notch):
/// set the frame's TOP-LEFT point directly on the NSWindow, centered at the very top of the
/// screen. This bypasses the framework's position clamp (which keeps windows below the menu
/// bar). The high window LEVEL (PANEL_LEVEL) makes it paint OVER the menu bar.
#[tauri::command]
fn dock_panel(window: tauri::WebviewWindow, width: f64, height: f64, duration: f64) {
    use objc::runtime::Object;
    use objc::{class, msg_send, sel, sel_impl};
    use tauri_nspanel::cocoa::foundation::{NSPoint, NSRect, NSSize};

    let Ok(ns) = window.ns_window() else {
        return;
    };
    let nswindow = ns as *mut Object;
    unsafe {
        // Re-assert a high level on the NSWindow itself right before positioning: macOS skips
        // the frame constraint (which otherwise clamps below the menu bar) for high-level windows.
        let _: () = msg_send![nswindow, setLevel: 1000_i64];
        // Deliver hover (mouse-moved) events even when non-key → hover-to-open works WITHOUT
        // needing an initial click to wake the panel.
        let _: () = msg_send![nswindow, setAcceptsMouseMovedEvents: true];
        let mut screen: *mut Object = msg_send![nswindow, screen];
        if screen.is_null() {
            screen = msg_send![class!(NSScreen), mainScreen];
        }
        if screen.is_null() {
            return;
        }
        let sframe: NSRect = msg_send![screen, frame];
        let x = sframe.origin.x + (sframe.size.width - width) / 2.0;
        // bottom-left origin. +1 lifts the window TOP one point ABOVE the screen top (y=-1 in
        // top-left space) so the docked pill tucks flush past the edge with no hairline seam.
        let y_bottom = sframe.origin.y + sframe.size.height - height + 1.0;
        let target = NSRect::new(NSPoint::new(x, y_bottom), NSSize::new(width, height));
        if duration <= 0.0 {
            let _: () = msg_send![nswindow, setFrame: target display: true animate: false];
        } else {
            // Explicit-duration animation via the window's animator (NSAnimationContext) so we
            // can make open slow & calm but close quicker. Top stays pinned → grows DOWNWARD.
            let ctx = class!(NSAnimationContext);
            let _: () = msg_send![ctx, beginGrouping];
            let current: *mut Object = msg_send![ctx, currentContext];
            let _: () = msg_send![current, setDuration: duration];
            let animator: *mut Object = msg_send![nswindow, animator];
            let _: () = msg_send![animator, setFrame: target display: true];
            let _: () = msg_send![ctx, endGrouping];
        }
    }
}

/// Grow the panel into a large, screen-CENTERED window (the settings surface — a spacious page,
/// not the menu-bar strip). Same NSWindow, re-framed centered on both axes with a slight upward
/// bias so it sits under the notch nicely.
#[tauri::command]
fn dock_centered(window: tauri::WebviewWindow, width: f64, height: f64, duration: f64) {
    use objc::runtime::Object;
    use objc::{class, msg_send, sel, sel_impl};
    use tauri_nspanel::cocoa::foundation::{NSPoint, NSRect, NSSize};

    let Ok(ns) = window.ns_window() else {
        return;
    };
    let nswindow = ns as *mut Object;
    unsafe {
        let _: () = msg_send![nswindow, setLevel: 1000_i64];
        let _: () = msg_send![nswindow, setAcceptsMouseMovedEvents: true];
        let mut screen: *mut Object = msg_send![nswindow, screen];
        if screen.is_null() {
            screen = msg_send![class!(NSScreen), mainScreen];
        }
        if screen.is_null() {
            return;
        }
        let sframe: NSRect = msg_send![screen, frame];
        let x = sframe.origin.x + (sframe.size.width - width) / 2.0;
        // Centered vertically, nudged up 6% so it reads as anchored to the top of the screen.
        let y_bottom = sframe.origin.y + (sframe.size.height - height) / 2.0 + sframe.size.height * 0.06;
        let target = NSRect::new(NSPoint::new(x, y_bottom), NSSize::new(width, height));
        if duration <= 0.0 {
            let _: () = msg_send![nswindow, setFrame: target display: true animate: false];
        } else {
            let ctx = class!(NSAnimationContext);
            let _: () = msg_send![ctx, beginGrouping];
            let current: *mut Object = msg_send![ctx, currentContext];
            let _: () = msg_send![current, setDuration: duration];
            let animator: *mut Object = msg_send![nswindow, animator];
            let _: () = msg_send![animator, setFrame: target display: true];
            let _: () = msg_send![ctx, endGrouping];
        }
    }
}

/// Entity-graph memory (Phase 3): the "Memory" tab list, filled by the background extractor.
/// Pure read, no egress.
#[tauri::command]
fn list_memory() -> Result<Vec<memory::Entity>, String> {
    let conn = db::open_default().ok_or_else(|| "db not ready".to_string())?;
    memory::list_entities(&conn, 200).map_err(|e| e.to_string())
}

/// Panel memory view: the engine's vitals for the TWO-SPEED model — cognee liveness + its curated
/// knowledge-graph datasets, and the local fast lane's size (SQLite/FTS = always-available instant
/// recall). Raw snapshots are local-only by design now, so there's no "sync backlog" to surface.
#[derive(serde::Serialize)]
struct MemoryStatus {
    sidecar_ok: bool,
    datasets: Vec<String>,
    local_snapshots: i64,
}

#[tauri::command]
async fn memory_status() -> Result<MemoryStatus, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let conn = db::open_default().ok_or_else(|| "db not ready".to_string())?;
        let local_snapshots = db::max_snapshot_id(&conn);
        let sidecar_ok = cognee::health();
        let datasets =
            if sidecar_ok { cognee::list_datasets().unwrap_or_default() } else { Vec::new() };
        Ok(MemoryStatus { sidecar_ok, datasets, local_snapshots })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Panel memory search — same retrieval path the drafts use (retrieve::search_memory).
#[tauri::command]
async fn search_memory(query: String) -> Result<Vec<cognee::Fact>, String> {
    tauri::async_runtime::spawn_blocking(move || retrieve::search_memory(&query, 8))
        .await
        .map_err(|e| e.to_string())?
}

/// Panel constellation: the real knowledge graph, distilled (cognee::graph_view, cached).
#[tauri::command]
async fn graph_view() -> Result<cognee::GraphView, String> {
    tauri::async_runtime::spawn_blocking(cognee::graph_view)
        .await
        .map_err(|e| e.to_string())?
}

/// One per-platform profile for the Profiles panel: the persona bound to a surface. Who-you-are is
/// global (Identity screen) — a profile is purely HOW you write here + which memory it can see.
#[derive(serde::Serialize)]
struct Profile {
    key: String,               // "domain-linkedin-com", "app-teams2", "app-outlook"
    label: String,             // human form: "linkedin.com", "outlook", "teams2"
    voice: Vec<String>,        // learned style bullets (shown in FULL — no truncation)
    voice_samples: i64,        // provenance: how many of the user's own messages it was learned from
    voice_examples: Vec<String>, // a few of those messages, for "here's why I think this"
    learning: bool,            // "Learn my voice here" consent toggle (default on)
    signature: String,
    circle: String,            // memory circle (profiles in the same circle share recall)
    shares_with: Vec<String>,  // other profiles in the same circle (memory-wall transparency)
    class: String,             // mail / chat / social / other (drives defaults + the UI label)
}

fn pretty_profile(key: &str) -> String {
    key.strip_prefix("domain-")
        .map(|d| d.replace('-', "."))
        .or_else(|| key.strip_prefix("app-").map(str::to_string))
        .unwrap_or_else(|| key.to_string())
}

/// List the platforms the user has written on, each with its learned voice (+ provenance), consent
/// state, signature and memory circle. Who-you-are is global — not per profile anymore.
#[tauri::command]
fn list_profiles() -> Result<Vec<Profile>, String> {
    let conn = db::open_default().ok_or_else(|| "db not ready".to_string())?;
    let keys = db::distinct_style_surfaces(&conn).map_err(|e| e.to_string())?;
    // Precompute circle → keys so each profile can show who it shares memory with.
    let circles: Vec<(String, String)> =
        keys.iter().map(|k| (k.clone(), profile::circle_of(k))).collect();
    Ok(keys
        .iter()
        .map(|key| {
            let voice = db::style_notes_for(&conn, key).unwrap_or_default();
            let voice_samples = db::style_sample_count(&conn, key).unwrap_or(0);
            let voice_examples = db::recent_style_samples(&conn, key, 3).unwrap_or_default();
            let learning = style::voice_learning_on(key);
            let signature = db::get_setting(&conn, &format!("signature:{key}")).unwrap_or_default();
            let circle = profile::circle_of(key);
            let shares_with: Vec<String> = circles
                .iter()
                .filter(|(k, c)| k != key && *c == circle)
                .map(|(k, _)| pretty_profile(k))
                .collect();
            let class = profile::class_of(key).to_string();
            Profile {
                label: pretty_profile(key),
                key: key.clone(),
                voice,
                voice_samples,
                voice_examples,
                learning,
                signature,
                circle,
                shares_with,
                class,
            }
        })
        .collect())
}

/// Draft a short sample in a profile's learned voice — lets the user SEE the persona before trusting
/// it. Reuses the exact draft persona (`trigger::persona_for_key`) with a canned prompt per class.
#[tauri::command]
async fn preview_voice(key: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let persona = trigger::persona_for_key(&key);
        match profile::class_of(&key) {
            "mail" => llm::email_compose(
                "A teammate sent a document for review. Write a short note saying you'll review it \
today and send feedback by end of day.",
                &persona,
            ),
            "social" => llm::reply_to("Congrats on the new role — well deserved!", "", &persona),
            _ => llm::reply_to("Hey, can you share the deck when you get a sec?", "", &persona),
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Toggle the "Learn my voice here" consent for a surface (default on).
#[tauri::command]
fn set_voice_learning(key: String, on: bool) {
    if let Some(conn) = db::open_default() {
        let _ =
            db::set_setting(&conn, &format!("voice_optout:{key}"), if on { "0" } else { "1" });
    }
}

/// Pending voice/identity change proposals awaiting the user's review (old → new).
#[tauri::command]
fn list_proposals() -> Result<Vec<db::ProposalRow>, String> {
    let conn = db::open_default().ok_or_else(|| "db not ready".to_string())?;
    db::list_proposals(&conn).map_err(|e| e.to_string())
}

/// Approve (apply) or dismiss a pending proposal, then remove it.
#[tauri::command]
fn resolve_proposal(id: i64, approve: bool) -> Result<(), String> {
    let conn = db::open_default().ok_or_else(|| "db not ready".to_string())?;
    if let Some(p) = db::get_proposal(&conn, id) {
        if approve {
            match p.kind.as_str() {
                "voice" => {
                    let bullets: Vec<String> = p
                        .after
                        .lines()
                        .map(str::trim)
                        .filter(|l| !l.is_empty())
                        .map(str::to_string)
                        .collect();
                    let _ = db::replace_style_notes(&conn, &p.key, &bullets);
                }
                "identity" => identity::apply_dossier(&p.after),
                _ => {}
            }
        }
        let _ = db::delete_proposal(&conn, id);
    }
    Ok(())
}

/// Save a profile's signature (per-platform sign-off).
#[tauri::command]
fn set_profile_signature(key: String, signature: String) {
    profile::set_signature(&key, &signature);
}

/// Assign a profile to a memory circle (profiles sharing a circle name share recall).
#[tauri::command]
fn set_profile_circle(key: String, circle: String) {
    profile::set_circle(&key, &circle);
}

/// The distilled wiki pages, most-recently-updated first (P4b).
#[tauri::command]
fn list_wiki() -> Result<Vec<db::WikiRow>, String> {
    let conn = db::open_default().ok_or_else(|| "db not ready".to_string())?;
    db::list_wiki_pages(&conn, 200).map_err(|e| e.to_string())
}

/// One entity's wiki page (for the constellation detail card).
#[tauri::command]
fn get_wiki_page(slug: String) -> Option<db::WikiRow> {
    let conn = db::open_default()?;
    db::get_wiki_page(&conn, &slug)
}

/// Distill a batch of wiki pages on demand (LLM-heavy → off the UI thread). Returns how many
/// pages were written.
#[tauri::command]
async fn refresh_wiki() -> Result<usize, String> {
    tauri::async_runtime::spawn_blocking(|| wiki::refresh(8))
        .await
        .map_err(|e| e.to_string())
}

/// The inbox: quill's activity feed (drafts delivered, wiki batches, system notices, saved chats).
#[tauri::command]
fn list_inbox() -> Result<Vec<db::EventRow>, String> {
    let conn = db::open_default().ok_or_else(|| "db not ready".to_string())?;
    db::list_events(&conn, 100).map_err(|e| e.to_string())
}

#[tauri::command]
fn inbox_unread() -> i64 {
    db::open_default().map(|c| db::unread_events(&c)).unwrap_or(0)
}

#[tauri::command]
fn mark_inbox_read(id: Option<i64>) {
    if let Some(conn) = db::open_default() {
        let _ = match id {
            Some(id) => db::mark_event_read(&conn, id),
            None => db::mark_all_events_read(&conn),
        };
    }
}

/// Save the current chat into the inbox (called by "new chat" so nothing is silently lost).
#[tauri::command]
fn archive_chat(title: String, transcript: String) {
    db::insert_event("chat", &title, &transcript);
}

/// Silent Accessibility check for the settings Permissions card (no system prompt).
#[tauri::command]
fn ax_trusted() -> bool {
    ax::is_trusted()
}

/// Open System Settings at the Accessibility pane (re-granting is routine after rebuilds).
#[tauri::command]
fn open_ax_settings() {
    ax::open_accessibility_settings();
}

/// Danger zone: irreversibly wipe the LOCAL memory — snapshots (+FTS via triggers), events,
/// wiki pages, learned style, working memory. Settings (name/exclusions/identity) survive.
/// Cognee's graph datasets are per-app; drop them via the exclusions flow (forget) as needed.
#[tauri::command]
fn wipe_all_data() -> Result<(), String> {
    let conn = db::open_default().ok_or_else(|| "db not ready".to_string())?;
    db::wipe_memory(&conn).map_err(|e| e.to_string())?;
    db::insert_event("system", "all local memory deleted", "snapshots, wiki, style and events were wiped at your request");
    Ok(())
}

/// One snapshot in the relevance-ranked "working memory" the last trigger pulled in (Phase 3).
#[derive(serde::Serialize)]
struct WorkingItem {
    app: String,
    ts: i64,
    relevance: f64,
    text: String,
}

/// The current working memory (what the most recent compose retrieved), most-relevant first.
/// Inspectable "now" surface (working-memory view). Pure read, no egress.
#[tauri::command]
fn get_working_memory() -> Result<Vec<WorkingItem>, String> {
    let conn = db::open_default().ok_or_else(|| "db not ready".to_string())?;
    let wm = db::working_memory(&conn, 20).map_err(|e| e.to_string())?;
    let ids: Vec<i64> = wm.iter().map(|(id, _)| *id).collect();
    let rel: std::collections::HashMap<i64, f64> = wm.into_iter().collect();
    let mut out: Vec<WorkingItem> = db::snapshots_by_ids(&conn, &ids)
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|r| WorkingItem {
            app: r.app_bundle,
            ts: r.ts,
            relevance: *rel.get(&r.id).unwrap_or(&0.0),
            text: r.text,
        })
        .collect();
    out.sort_by(|a, b| b.relevance.partial_cmp(&a.relevance).unwrap_or(std::cmp::Ordering::Equal));
    Ok(out)
}

/// The user's identity DOSSIER (rich markdown) — what the Identity screen shows/edits. Falls back
/// to the old short blurb on installs predating the dossier. Empty until first synthesised.
#[tauri::command]
fn get_identity() -> Option<String> {
    identity::dossier().or_else(identity::profile)
}

/// The SHORT grounding blurb actually injected into draft prompts — surfaced for transparency
/// ("this is what Quill tells the model about you").
#[tauri::command]
fn get_identity_blurb() -> Option<String> {
    identity::profile()
}

#[tauri::command]
fn set_identity(profile: String) {
    identity::apply_dossier(&profile); // saves the dossier AND re-derives the grounding blurb
}

/// Set true ONLY by the tray "Quit" item, so a stray ⌘Q or a window close hides the agent instead
/// of terminating it (start automatically, quit deliberately).
static QUITTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether Quill is set to launch at login.
#[tauri::command]
fn get_autostart(app: tauri::AppHandle) -> bool {
    use tauri_plugin_autostart::ManagerExt;
    app.autolaunch().is_enabled().unwrap_or(false)
}

/// Enable/disable launch at login (the Privacy toggle).
#[tauri::command]
fn set_autostart(app: tauri::AppHandle, on: bool) {
    use tauri_plugin_autostart::ManagerExt;
    let al = app.autolaunch();
    let _ = if on { al.enable() } else { al.disable() };
}

/// memify(): cognee's graph self-improvement — prune stale nodes, strengthen frequent ties,
/// derive new facts — "memory that gets better, not just bigger". Also suitable
/// for a nightly schedule later. No-op error if the sidecar is down.
#[tauri::command]
async fn run_memify() -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(|| {
        cognee::memify().map(|()| "memify started on the sidecar".to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Synthesise a fresh identity DOSSIER draft from memory (LLM). Returns the draft for the user to
/// review/edit — saving happens on their explicit Save (`set_identity`), not here.
#[tauri::command]
async fn rebuild_identity() -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(identity::synth_from_memory)
        .await
        .map_err(|e| e.to_string())?
}

/// Whether the panel is currently expanded — read by the hover monitor to pick the correct
/// hit-test region (small pill vs full panel). Set by the frontend on expand/collapse.
static PANEL_EXPANDED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[tauri::command]
fn set_panel_expanded(expanded: bool) {
    PANEL_EXPANDED.store(expanded, std::sync::atomic::Ordering::Relaxed);
}

/// Global cursor monitor: drives hover-to-open WITHOUT the panel ever needing keyboard focus.
/// A non-activating NSPanel only receives mouse-moved events while it is the key window, and
/// clicking it to make it key steals focus from the user's text field — so DOM/CSS hover is
/// unusable here. Instead we poll the global cursor position (Core Graphics, focus-independent)
/// and emit enter/leave for the top-centre dock region. Dims MUST match the frontend's
/// PILL_*/PANEL_* constants. Coords are global points, top-left origin (same space as the
/// NSScreen frame dock_panel positions against), so the region lines up with the docked window.
fn start_hover_monitor(app: tauri::AppHandle) {
    use core_graphics::display::CGDisplay;
    use core_graphics::event::CGEvent;
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
    use std::sync::atomic::Ordering;

    const PILL_W: f64 = 200.0;
    const PILL_H: f64 = 34.0;
    const PANEL_W: f64 = 600.0;
    const PANEL_H: f64 = 460.0;

    std::thread::spawn(move || {
        let mut inside_prev = false;
        loop {
            std::thread::sleep(std::time::Duration::from_millis(120));
            let Ok(src) = CGEventSource::new(CGEventSourceStateID::CombinedSessionState) else {
                continue;
            };
            let Ok(ev) = CGEvent::new(src) else { continue };
            let loc = ev.location(); // global cursor, top-left origin, points
            let screen_w = CGDisplay::main().bounds().size.width;
            let (w, h) = if PANEL_EXPANDED.load(Ordering::Relaxed) {
                (PANEL_W, PANEL_H)
            } else {
                (PILL_W, PILL_H)
            };
            let x0 = (screen_w - w) / 2.0;
            let inside = loc.x >= x0 && loc.x <= x0 + w && loc.y >= 0.0 && loc.y <= h;
            if inside != inside_prev {
                inside_prev = inside;
                let _ = app.emit("quill://hover", inside);
            }
        }
    });
}

/// Whether the companion fish is currently floating on the desktop (vs. docked/hidden).
static FISH_FLOATING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Dock slot (top-left of the panel toolbar) and the floating rest spot, as (x, y, w, h) in
/// top-left screen points. The 600.0 mirrors the frontend's PANEL_W.
fn fish_frames(screen_w: f64, screen_h: f64) -> ((f64, f64, f64, f64), (f64, f64, f64, f64)) {
    let panel_x0 = (screen_w - 600.0) / 2.0;
    let dock = (panel_x0 + 12.0, 3.0, 34.0, 30.0);
    let (fw, fh) = (84.0, 70.0);
    let float = ((screen_w - fw) / 2.0, (screen_h - fh) / 2.0, fw, fh);
    (dock, float)
}

/// Move/resize an NSWindow to a top-left-origin rect, optionally animated (NSAnimationContext).
unsafe fn move_window(
    nswindow: *mut objc::runtime::Object,
    screen_h: f64,
    rect: (f64, f64, f64, f64),
    duration: f64,
) {
    use objc::runtime::Object;
    use objc::{class, msg_send, sel, sel_impl};
    use tauri_nspanel::cocoa::base::nil;
    use tauri_nspanel::cocoa::foundation::{NSPoint, NSRect, NSSize, NSString};
    let (tx, ty, w, h) = rect;
    let y_bottom = screen_h - ty - h; // top-left origin → bottom-left origin
    let target = NSRect::new(NSPoint::new(tx, y_bottom), NSSize::new(w, h));
    if duration <= 0.0 {
        let _: () = msg_send![nswindow, setFrame: target display: true animate: false];
    } else {
        let ctx = class!(NSAnimationContext);
        let _: () = msg_send![ctx, beginGrouping];
        let current: *mut Object = msg_send![ctx, currentContext];
        let _: () = msg_send![current, setDuration: duration];
        // Smooth acceleration + settle, instead of the default near-linear motion.
        let timing_name = NSString::alloc(nil).init_str("easeInEaseOut");
        let timing: *mut Object =
            msg_send![class!(CAMediaTimingFunction), functionWithName: timing_name];
        let _: () = msg_send![current, setTimingFunction: timing];
        let animator: *mut Object = msg_send![nswindow, animator];
        let _: () = msg_send![animator, setFrame: target display: true];
        let _: () = msg_send![ctx, endGrouping];
    }
}

/// Pop the fish out of the dock onto the desktop, animating from the dock slot to the rest spot.
#[tauri::command]
fn fish_undock(app: tauri::AppHandle) {
    use objc::runtime::Object;
    use objc::{class, msg_send, sel, sel_impl};
    use tauri_nspanel::cocoa::foundation::NSRect;
    let Some(win) = app.get_webview_window("fish") else {
        return;
    };
    let Ok(ns) = win.ns_window() else { return };
    let nswindow = ns as *mut Object;
    unsafe {
        let screen: *mut Object = msg_send![class!(NSScreen), mainScreen];
        if screen.is_null() {
            return;
        }
        let sframe: NSRect = msg_send![screen, frame];
        let (sw, sh) = (sframe.size.width, sframe.size.height);
        let (dock, float) = fish_frames(sw, sh);
        let _: () = msg_send![nswindow, setLevel: 1000_i64];
        move_window(nswindow, sh, dock, 0.0); // start small, at the dock slot
        let _: () = msg_send![nswindow, orderFrontRegardless]; // show WITHOUT activating
        move_window(nswindow, sh, float, 0.6); // slow, smooth glide out to the desktop
    }
    FISH_FLOATING.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = app.emit("quill://fish", true);
}

/// Send the fish back: animate into the dock slot (shrinking), then hide it.
#[tauri::command]
fn fish_dock(app: tauri::AppHandle) {
    use objc::runtime::Object;
    use objc::{class, msg_send, sel, sel_impl};
    use tauri_nspanel::cocoa::foundation::NSRect;
    let Some(win) = app.get_webview_window("fish") else {
        return;
    };
    let Ok(ns) = win.ns_window() else { return };
    let nswindow = ns as *mut Object;
    unsafe {
        let screen: *mut Object = msg_send![class!(NSScreen), mainScreen];
        if screen.is_null() {
            return;
        }
        let sframe: NSRect = msg_send![screen, frame];
        let (sw, sh) = (sframe.size.width, sframe.size.height);
        let (dock, _float) = fish_frames(sw, sh);
        // Keep the fish ABOVE the panel while it flies home, otherwise the glide is hidden
        // behind the open panel and looks like it just vanishes.
        let _: () = msg_send![nswindow, setLevel: 1000_i64];
        let _: () = msg_send![nswindow, orderFrontRegardless];
        move_window(nswindow, sh, dock, 0.5); // slow, smooth glide back into the dock slot
    }
    FISH_FLOATING.store(false, std::sync::atomic::Ordering::Relaxed);
    let _ = app.emit("quill://fish", false);
    // Hide only AFTER the glide-in finishes (off-thread; hide() dispatches to the main loop).
    let win2 = win.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(560));
        let _ = win2.hide();
    });
}

/// Drag state for the focus-safe fish move: we reposition the window ourselves instead of using
/// the native window drag, which would activate our app and steal the user's text-field focus.
static FISH_DRAGGING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static FISH_MOVED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Global cursor position in top-left screen points (focus-independent, like the hover monitor).
fn read_cursor_topleft() -> Option<(f64, f64)> {
    use core_graphics::event::CGEvent;
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
    let src = CGEventSource::new(CGEventSourceStateID::CombinedSessionState).ok()?;
    let ev = CGEvent::new(src).ok()?;
    let loc = ev.location();
    Some((loc.x, loc.y))
}

/// Begin dragging the fish: a background thread follows the global cursor and repositions the
/// window. No native window-drag → our app never activates, so the user's text field keeps focus.
#[tauri::command]
fn fish_drag_start(app: tauri::AppHandle) {
    use objc::runtime::Object;
    use objc::{class, msg_send, sel, sel_impl};
    use std::sync::atomic::Ordering::Relaxed;
    use tauri_nspanel::cocoa::foundation::NSRect;
    let Some(win) = app.get_webview_window("fish") else {
        return;
    };
    let Ok(ns) = win.ns_window() else { return };
    let nswindow = ns as *mut Object;
    let Some((sx, sy)) = read_cursor_topleft() else {
        return;
    };
    let (wx, wy);
    unsafe {
        let screen: *mut Object = msg_send![class!(NSScreen), mainScreen];
        if screen.is_null() {
            return;
        }
        let sframe: NSRect = msg_send![screen, frame];
        let frame: NSRect = msg_send![nswindow, frame];
        wx = frame.origin.x;
        wy = sframe.size.height - frame.origin.y - frame.size.height; // bottom-left → top-left
    }
    let (ox, oy) = (sx - wx, sy - wy); // where inside the window the cursor grabbed
    FISH_DRAGGING.store(true, Relaxed);
    FISH_MOVED.store(false, Relaxed);
    let win2 = win.clone();
    std::thread::spawn(move || {
        while FISH_DRAGGING.load(Relaxed) {
            if let Some((cx, cy)) = read_cursor_topleft() {
                if (cx - sx).abs() > 4.0 || (cy - sy).abs() > 4.0 {
                    FISH_MOVED.store(true, Relaxed);
                }
                let _ = win2.set_position(tauri::LogicalPosition::new(cx - ox, cy - oy));
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    });
}

/// End the fish drag. A press with no movement is a click → send the fish home to the dock.
#[tauri::command]
fn fish_drag_stop(app: tauri::AppHandle) {
    use std::sync::atomic::Ordering::Relaxed;
    FISH_DRAGGING.store(false, Relaxed);
    if !FISH_MOVED.load(Relaxed) {
        fish_dock(app); // it was a click, not a drag
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // LLM/secrets config. In dev, dotenvy finds src-tauri/.env via the working dir. A packaged
    // .app has a different cwd and won't find it, so ALSO load the user config file at
    // ~/Library/Application Support/com.danishalisiddiqui.quill/.env — where SETUP.md tells
    // you to put your LLM key/model. dotenvy never overrides an already-set var, so real
    // env vars win over the dev .env, which wins over this user file.
    let _ = dotenvy::dotenv();
    if let Ok(home) = std::env::var("HOME") {
        let user_env = std::path::Path::new(&home)
            .join("Library/Application Support/com.danishalisiddiqui.quill/.env");
        let _ = dotenvy::from_path(&user_env);
    }

    tauri::Builder::default()
        .plugin(tauri_nspanel::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .on_window_event(|window, event| {
            // Closing the panel window (⌘W / red button) must NOT kill the agent — hide it instead,
            // so ambient capture keeps running. Only the tray "Quit" truly exits (sets QUITTING).
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if !QUITTING.load(std::sync::atomic::Ordering::SeqCst) {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            chronicle_today,
            get_paused,
            set_paused,
            get_exclusions,
            set_exclusions,
            list_memory,
            get_working_memory,
            memory_status,
            search_memory,
            graph_view,
            list_profiles,
            set_profile_signature,
            set_profile_circle,
            preview_voice,
            set_voice_learning,
            list_proposals,
            resolve_proposal,
            list_wiki,
            get_wiki_page,
            refresh_wiki,
            list_inbox,
            inbox_unread,
            mark_inbox_read,
            archive_chat,
            ax_trusted,
            open_ax_settings,
            wipe_all_data,
            get_user_name,
            set_user_name,
            ask,
            chat_starters,
            dock_panel,
            dock_centered,
            set_panel_expanded,
            fish_undock,
            fish_dock,
            fish_drag_start,
            fish_drag_stop,
            get_identity,
            get_identity_blurb,
            set_identity,
            rebuild_identity,
            get_autostart,
            set_autostart,
            run_memify
        ])
        .setup(|app| {
            // Run as an agent app: no Dock icon, just the floating pill.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            // Ask for Accessibility permission (needed to read/replace the focused field).
            let trusted = ax::ensure_accessibility_prompt();
            println!("[quill] accessibility trusted = {trusted}");
            if !trusted {
                // macOS revokes this grant on every rebuild/update — guide the user to the right
                // pane instead of leaving capture + the rewrite loop silently dead.
                eprintln!(
                    "[quill] Accessibility NOT granted — can't read/replace text or capture. \
                     Opening Settings → Privacy & Security → Accessibility; enable quill, relaunch."
                );
                ax::open_accessibility_settings();
            }

            // Start ambient capture (Phase 2): stores focused-window text to quill.db.
            if let Ok(dir) = app.path().app_data_dir() {
                let _ = std::fs::create_dir_all(&dir);
                let db_path = dir.join("quill.db");
                db::prepare(&db_path); // integrity gate: recover from a corrupt/torn DB first
                db::set_db_path(db_path.clone());
                // E1: re-key learned voice from app bundle → domain-aware profile key so today's
                // Teams/Outlook tuning survives the profile switch (idempotent).
                if let Some(conn) = db::open_default() {
                    match db::migrate_style_surface_keys(&conn) {
                        Ok(n) if n > 0 => println!("[quill] profiles: migrated {n} style surface key(s)"),
                        _ => {}
                    }
                    // Fold app/web platform variants onto one canonical key (Teams-app ⇄ Teams-web)
                    // so learned voice/identity/signature/circle carry over between them.
                    match db::migrate_canonical_profile_keys(&conn) {
                        Ok(n) if n > 0 => println!("[quill] profiles: folded {n} key(s) to canonical platform"),
                        _ => {}
                    }
                }
                settings::load_from_db(); // restore pause + user exclusions (Phase 2.3)
                capture::start(app.handle().clone(), db_path);
            }

            let window = app.get_webview_window("main").unwrap();

            // Put it at a clearly on-screen spot for the spike test.
            let _ = window.set_position(tauri::LogicalPosition::new(500.0, 120.0));

            // Convert the regular window into an NSPanel.
            let panel = window.to_panel().unwrap();

            // Re-apply native vibrancy AFTER the NSPanel conversion: the conversion drops the
            // window effect, leaving an opaque fill that hides whatever is behind the panel.
            // This frosted backdrop is what shows (blurred) through the translucent panel edges.
            // radius 0 here — we round the corners ourselves below (bottom only, so the native
            // effect must not round all four and fight that shape).
            let _ = window.set_effects(
                tauri::window::EffectsBuilder::new()
                    .effect(tauri::window::Effect::HudWindow)
                    .state(tauri::window::EffectState::Active)
                    .radius(0.0)
                    .build(),
            );

            // Round ONLY the bottom corners of the whole content layer (vibrancy + webview clip
            // to the SAME shape), so the panel stays flush/square against the menu bar at the top
            // and the bottom corners are rounded — with no untinted sliver at the corners.
            if let Ok(ns) = window.ns_window() {
                use objc::runtime::Object;
                use objc::{msg_send, sel, sel_impl};
                let nswindow = ns as *mut Object;
                unsafe {
                    let content: *mut Object = msg_send![nswindow, contentView];
                    if !content.is_null() {
                        let _: () = msg_send![content, setWantsLayer: true];
                        let layer: *mut Object = msg_send![content, layer];
                        if !layer.is_null() {
                            let _: () = msg_send![layer, setCornerRadius: 16.0_f64];
                            // kCALayerMinXMinYCorner | kCALayerMaxXMinYCorner = the two BOTTOM
                            // corners (NSView geometry: origin bottom-left, Y increases upward).
                            let _: () = msg_send![layer, setMaskedCorners: 3u64];
                            let _: () = msg_send![layer, setMasksToBounds: true];
                        }
                    }
                }
            }

            panel.set_level(PANEL_LEVEL);

            // Borderless + non-activating: clicking/showing it won't steal focus.
            panel.set_style_mask(NONACTIVATING_PANEL_MASK);

            // Show on every Space and above fullscreen apps; don't get managed
            // by Mission Control as a normal window.
            panel.set_collection_behaviour(
                NSWindowCollectionBehavior::NSWindowCollectionBehaviorCanJoinAllSpaces
                    | NSWindowCollectionBehavior::NSWindowCollectionBehaviorStationary
                    | NSWindowCollectionBehavior::NSWindowCollectionBehaviorFullScreenAuxiliary,
            );

            // Only take key focus if a control inside actually needs it.
            panel.set_becomes_key_only_if_needed(true);

            // Floating panel so clicks/drags register without a focus-click first.
            panel.set_floating_panel(true);

            // show() alone isn't enough for a non-activating panel — force it
            // onto the screen regardless of which app is active.
            panel.show();
            panel.order_front_regardless();

            println!(
                "[quill] setup complete; panel.is_visible() = {}",
                panel.is_visible()
            );

            // Companion fish window: a non-activating panel that lives docked (hidden) and pops
            // onto the desktop on demand. Configure it like the main panel, but leave it hidden.
            if let Some(fish) = app.get_webview_window("fish") {
                if let Ok(fpanel) = fish.to_panel() {
                    fpanel.set_level(PANEL_LEVEL);
                    fpanel.set_style_mask(NONACTIVATING_PANEL_MASK);
                    fpanel.set_collection_behaviour(
                        NSWindowCollectionBehavior::NSWindowCollectionBehaviorCanJoinAllSpaces
                            | NSWindowCollectionBehavior::NSWindowCollectionBehaviorStationary
                            | NSWindowCollectionBehavior::NSWindowCollectionBehaviorFullScreenAuxiliary,
                    );
                    fpanel.set_becomes_key_only_if_needed(true);
                }
            }

            // Install the global bare-right-Option key tap.
            trigger::install();

            // Hover-to-open driven by a focus-independent global cursor monitor (the panel
            // can't get DOM hover without becoming key + stealing focus).
            start_hover_monitor(app.handle().clone());

            // Launch at login (opt-out): enable ONCE after the first successful Accessibility grant,
            // so ambient memory has no gaps. A flag makes it a one-time default — we never re-enable
            // after the user turns it off in Privacy.
            if trusted {
                use tauri_plugin_autostart::ManagerExt;
                if let Some(conn) = db::open_default() {
                    if db::get_setting(&conn, "autostart_initialized").is_none() {
                        let _ = app.autolaunch().enable();
                        let _ = db::set_setting(&conn, "autostart_initialized", "1");
                        println!("[quill] autostart enabled by default (first setup)");
                    }
                }
            }

            // Menu-bar (status item) tray — the DELIBERATE quit path. With this present, ⌘Q and
            // window-close merely HIDE the panel (see the ExitRequested handler); only this menu's
            // "Quit" truly terminates, so a reflex keystroke never kills ambient capture.
            {
                use std::sync::atomic::Ordering;
                let show_i = tauri::menu::MenuItem::with_id(app, "show", "Show Quill", true, None::<&str>)?;
                let quit_i = tauri::menu::MenuItem::with_id(app, "quit", "Quit Quill", true, None::<&str>)?;
                let sep = tauri::menu::PredefinedMenuItem::separator(app)?;
                let tray_menu = tauri::menu::Menu::with_items(app, &[&show_i, &sep, &quit_i])?;
                let mut tray = tauri::tray::TrayIconBuilder::with_id("quill-tray")
                    .tooltip("Quill")
                    .menu(&tray_menu)
                    .show_menu_on_left_click(true)
                    .on_menu_event(|app, event| match event.id().as_ref() {
                        "quit" => {
                            QUITTING.store(true, Ordering::SeqCst);
                            capture::request_shutdown();
                            if let Some(conn) = db::open_default() {
                                let _ = db::checkpoint(&conn);
                            }
                            app.exit(0);
                        }
                        "show" => {
                            if let Some(w) = app.get_webview_window("main") {
                                let _ = w.show();
                                let _ = w.set_focus();
                            }
                        }
                        _ => {}
                    });
                if let Some(icon) = app.default_window_icon().cloned() {
                    tray = tray.icon(icon).icon_as_template(true);
                }
                tray.build(app)?;
            }

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            if let tauri::RunEvent::ExitRequested { api, .. } = event {
                if QUITTING.load(std::sync::atomic::Ordering::SeqCst) {
                    // Real quit (tray → Quit already ran cleanup) — let it exit.
                } else {
                    // Stray ⌘Q while the panel is focused: keep the agent alive, just hide the panel
                    // Quitting is only via the menu-bar "Quit Quill".
                    api.prevent_exit();
                    if let Some(w) = app.get_webview_window("main") {
                        let _ = w.hide();
                    }
                }
            }
        });
}
