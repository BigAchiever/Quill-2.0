// Cognee client (self-hosted memory layer). Quill's four memory operations —
// remember / recall / improve (memify) / forget — over the local cognee REST sidecar.
// Mirrors llm.rs: blocking reqwest, env-configured, errors are Strings the caller logs and
// survives (sidecar down ⇒ graceful screen-only degrade, never a crash).
//
// Sidecar: docker container `quill-cognee` on 127.0.0.1:8765 (8000 is taken by ComfyUI on this
// machine). See cognee-sidecar/.env — 100% local: a self-hosted LLM for cognify, Ollama embeddings, no
// telemetry. Dataset scheme: one dataset per surface ("app-outlook", "domain-linkedin.com",
// "corrections") so forget(surface) is one call and provenance is legible.

use serde_json::json;
use std::time::Duration;

/// Base URL of the sidecar's /api/v1 (no trailing slash).
fn base() -> String {
    std::env::var("QUILL_COGNEE_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8765/api/v1".to_string())
        .trim_end_matches('/')
        .to_string()
}

/// ONE shared, connection-pooled client for all cognee calls. A fresh client per call opened a
/// fresh TCP connection per capture, churning Docker Desktop's localhost port-forward (observed:
/// intermittent "error sending request" under load while curl succeeded). Keep-alive + pooling
/// fixes that; `.no_proxy()` keeps inherited proxy env vars from ever intercepting loopback.
/// Timeouts are per-request (`.timeout(...)` on the builder) since ops range from 2s to minutes.
fn client(timeout_secs: u64) -> Result<ClientWithTimeout, String> {
    static CLIENT: std::sync::OnceLock<reqwest::blocking::Client> = std::sync::OnceLock::new();
    let c = CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .no_proxy()
            .pool_max_idle_per_host(2)
            .build()
            .expect("reqwest client with static config cannot fail to build")
    });
    Ok(ClientWithTimeout { client: c, timeout: Duration::from_secs(timeout_secs) })
}

/// Thin shim so call sites keep their `client(N)?.post(...)` shape while timeouts apply per-request.
struct ClientWithTimeout {
    client: &'static reqwest::blocking::Client,
    timeout: Duration,
}

impl ClientWithTimeout {
    fn post(&self, url: String) -> reqwest::blocking::RequestBuilder {
        self.client.post(url).timeout(self.timeout)
    }
    fn get(&self, url: String) -> reqwest::blocking::RequestBuilder {
        self.client.get(url).timeout(self.timeout)
    }
}

/// One recalled fact, with enough provenance for the context block ("[app-outlook] …").
/// Serialize: the panel's memory view returns these over IPC as-is.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Fact {
    pub text: String,
    pub dataset: Option<String>,
}

/// Sidecar liveness for the panel's status strip (3s cap — a status dot must never hang the UI).
pub fn health() -> bool {
    let root = base().trim_end_matches("/api/v1").to_string();
    client(3)
        .ok()
        .and_then(|c| c.get(format!("{root}/health")).send().ok())
        .is_some_and(|r| r.status().is_success())
}

// ── Constellation view (panel) ────────────────────────────────────────────────
// The graph endpoint returns the WHOLE knowledge graph (it can be large — dataset scoping is
// loose with access control off). We distill it server-side into a
// compact payload: Entity nodes only, categorized via their `is_a` → EntityType edge, top-K by
// tie degree. Cached — the fetch+parse is the expensive part, and the graph moves slowly.

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GraphNode {
    pub label: String,
    pub cat: String, // People / Orgs / Tools / Projects / Topics / "" (untyped)
    pub deg: u32,
    pub desc: String, // cognify's own one-liner (most entities carry one)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GraphView {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<(u16, u16)>, // indices into `nodes`
    pub cats: Vec<(String, u32)>, // category counts over ALL typed entities
    pub total_entities: u32,
    pub total_ties: u32, // entity↔entity semantic edges in the full graph
}

/// Bucket a free-form EntityType label ("artificial intelligence model", "person") into the
/// panel's five constellation categories.
fn bucket(type_label: &str) -> &'static str {
    let l = type_label.to_lowercase();
    const PEOPLE: &[&str] =
        &["person", "people", "human", "developer", "engineer", "employee", "user", "contact",
          "author", "recruiter", "professional", "student", "manager", "individual"];
    const ORGS: &[&str] =
        &["organization", "organisation", "company", "team", "institution", "agency",
          "business", "brand", "community", "department"];
    const PROJECTS: &[&str] = &["project", "product", "initiative", "campaign", "venture"];
    const TOOLS: &[&str] =
        &["tool", "software", "application", "app", "model", "system", "technology",
          "framework", "library", "service", "website", "platform", "device", "language",
          "database", "api", "protocol", "server", "format"];
    if PEOPLE.iter().any(|k| l.contains(k)) {
        "People"
    } else if ORGS.iter().any(|k| l.contains(k)) {
        "Orgs"
    } else if PROJECTS.iter().any(|k| l.contains(k)) {
        "Projects"
    } else if TOOLS.iter().any(|k| l.contains(k)) {
        "Tools"
    } else {
        "Topics"
    }
}

/// True for entity labels that are extraction noise rather than real memory: bare dates/weekdays,
/// "maybe …" hedge fragments, and pure number/punctuation strings. Junk entities only ever surface
/// in the constellation (recall returns DocumentChunk text, never Entity nodes), so filtering them
/// here cleans the view with zero risk. The one-off graph cleanup deletes this SAME set from Kuzu.
/// Conservative on purpose — better to keep a borderline real entity than delete it.
pub fn is_junk_entity(label: &str) -> bool {
    let l = label.trim().to_lowercase();
    if l.chars().count() <= 1 {
        return true; // stray single chars / empties
    }
    if l.starts_with("maybe ") {
        return true; // uncertain extraction cognee shouldn't have made a node for
    }
    const DAYS: &[&str] = &[
        "monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday",
        "today", "tomorrow", "yesterday", "tonight", "this week", "next week", "last week",
        "this month", "next month", "last month", "this year", "next year", "last year",
    ];
    if DAYS.contains(&l.as_str()) {
        return true; // bare relative-time words
    }
    // Surface artifacts cognee extracted as "entities" but which are never real named memory:
    // URLs, emails, HTTP routes, file paths, filenames, UUIDs, code/markup fragments.
    if l.starts_with("http://") || l.starts_with("https://") || l.starts_with("www.") {
        return true; // URL
    }
    if l.contains('@') && l.rsplit('@').next().is_some_and(|d| d.contains('.') && !d.contains(' ')) {
        return true; // email address
    }
    if ["get /", "post /", "put /", "delete /", "patch /", "options /"].iter().any(|p| l.starts_with(p)) {
        return true; // HTTP route
    }
    if l.starts_with('/') || l.starts_with("~/") || l.starts_with("./") || l.starts_with("../")
        || l.contains('\\')
        || (l.contains('/') && !l.contains(' ') && l.split('/').count() >= 3)
    {
        return true; // file path
    }
    if is_uuid(&l) {
        return true;
    }
    if ['{', '}', '<', '>'].iter().any(|c| l.contains(*c))
        || ["::", "->", "==", "=>"].iter().any(|s| l.contains(s))
    {
        return true; // code / markup fragment
    }
    if has_file_ext(&l) {
        return true; // filename
    }
    // ISO date like 2026-06-25.
    let b = l.as_bytes();
    if l.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && l.chars().enumerate().all(|(i, c)| if i == 4 || i == 7 { c == '-' } else { c.is_ascii_digit() })
    {
        return true;
    }
    // Purely numeric / punctuation / whitespace — never a real named entity.
    if l.chars().all(|c| c.is_ascii_digit() || c.is_ascii_punctuation() || c.is_whitespace()) {
        return true;
    }
    false
}

fn is_uuid(l: &str) -> bool {
    let b = l.as_bytes();
    l.len() == 36
        && [8, 13, 18, 23].iter().all(|&i| b[i] == b'-')
        && l.chars().enumerate().all(|(i, c)| {
            if [8, 13, 18, 23].contains(&i) { c == '-' } else { c.is_ascii_hexdigit() }
        })
}

fn has_file_ext(l: &str) -> bool {
    const EXTS: &[&str] = &[
        ".png", ".jpg", ".jpeg", ".svg", ".gif", ".webp", ".ico", ".pdf", ".txt", ".md", ".json",
        ".csv", ".log", ".py", ".rs", ".js", ".ts", ".tsx", ".jsx", ".html", ".css", ".scss",
        ".zip", ".dmg", ".mp4", ".mov", ".mp3", ".wav", ".xlsx", ".xls", ".docx", ".doc", ".pptx",
        ".toml", ".yaml", ".yml", ".lock", ".sh", ".bat", ".ipynb", ".env", ".cfg", ".ini", ".git",
    ];
    EXTS.iter().any(|e| l.ends_with(e))
}

/// Description-based category for entities cognee left WITHOUT an is_a→EntityType edge (~95% of
/// them). cognee still writes a description that opens with the kind ("A person who…", "A company
/// that…", "An AI tool…"), so we bucket off the leading words. Falls back to Topics. This is why
/// the constellation's category chips are populated at all — is_a alone typed only a few hundred.
fn desc_type(desc: &str) -> &'static str {
    const PEOPLE: &[&str] = &[
        "person", "people", "user", "author", "contact", "member", "developer", "engineer",
        "recruiter", "student", "founder", "commenter", "employee", "colleague", "candidate",
        "manager", "individual", "participant", "attendee", "speaker", "coach", "contributor",
        "coordinator", "moderator", "admin", "host", "mentor", "lead", "reviewer", "ceo", "cto",
        "cofounder", "teammate", "friend",
    ];
    const ORGS: &[&str] = &[
        "organization", "organisation", "company", "team", "community", "agency", "institution",
        "startup", "business", "channel", "server", "group", "department", "brand", "club",
        "association", "firm", "studio",
    ];
    const TOOLS: &[&str] = &[
        "tool", "software", "application", "app", "model", "framework", "library", "platform",
        "api", "service", "system", "website", "technology", "language", "database", "protocol",
        "extension", "feature", "plugin", "package",
    ];
    const PROJECTS: &[&str] =
        &["project", "product", "initiative", "campaign", "repository", "repo", "hackathon"];
    let d = desc.trim().to_lowercase();
    let rest = d
        .strip_prefix("a ")
        .or_else(|| d.strip_prefix("an "))
        .or_else(|| d.strip_prefix("the "))
        .unwrap_or(&d);
    let words: Vec<&str> = rest
        .split(|c: char| !c.is_alphanumeric() && c != '-')
        .filter(|w| !w.is_empty())
        .take(5)
        .collect();
    let has = |set: &[&str]| words.iter().any(|w| set.contains(w));
    if has(PEOPLE) {
        "People"
    } else if has(ORGS) {
        "Orgs"
    } else if has(TOOLS) {
        "Tools"
    } else if has(PROJECTS) {
        "Projects"
    } else {
        "Topics"
    }
}

/// True when the description marks the entity as a media / file / ephemeral artifact (a video,
/// image, screenshot, timestamp, date, calendar entry, search suggestion, config value, emoji)
/// rather than real memory. Guarded by a People/Org check at the call site so a real person or org
/// merely mentioned near such a word is never dropped.
fn is_soft_noise(desc: &str) -> bool {
    let d = desc.trim().to_lowercase();
    // Raw token-grabs ("The token 'arabic' from the user text", "The word 'tracks' appearing in…")
    // and UI chrome (menu items, tabs, buttons, icons) — cognee made a node out of a stray word or
    // an interface control. Narrow on purpose: "a term/field/section/value…" is NOT here because
    // those catch real topics (coqui, cybersecurity, nihilism).
    const LOWVALUE_PREFIX: &[&str] = &[
        "the token ", "the word ", "the string ", "the text ", "the phrase ",
        "a menu item", "the menu item", "a tab in", "the tab", "a button", "the button",
        "an icon", "the icon", "a placeholder", "a keyboard shortcut", "a ui element",
    ];
    if LOWVALUE_PREFIX.iter().any(|p| d.starts_with(p)) {
        return true;
    }
    let rest = d
        .strip_prefix("a ")
        .or_else(|| d.strip_prefix("an "))
        .or_else(|| d.strip_prefix("the "))
        .unwrap_or(&d);
    let mut it = rest.split_whitespace();
    let Some(w0) = it.next() else { return false };
    const FIRST: &[&str] = &[
        "video", "youtube", "jpeg", "png", "image", "screenshot", "mp3", "mpeg", "timestamp",
        "date", "calendar", "emoji", "url", "hashtag",
    ];
    if FIRST.contains(&w0) {
        return true;
    }
    if let Some(w1) = it.next() {
        const TWO: &[&str] = &[
            "search suggestion", "search query", "search result", "search token", "value assigned",
            "file located", "file attached", "file named", "file containing", "word document",
            "excel file", "markdown file", "audio file",
        ];
        let two = format!("{w0} {w1}");
        if TWO.contains(&two.as_str()) {
            return true;
        }
    }
    false
}

const GRAPH_CACHE_JSON: &str = "graph_view_cache_json";
const GRAPH_CACHE_TS: &str = "graph_view_cache_ts";
const GRAPH_STALE_SECS: i64 = 10 * 60;

/// The constellation's graph, SERVED FROM CACHE: memory (3 min) → disk (any age; a stale copy
/// still returns instantly while a background rebuild refreshes it) → full fetch only when no
/// cache exists at all (true first run). The raw fetch can be large + a distill pass — observed
/// live as a "mapping your memory graph…" wait on every open, because the in-memory cache dies
/// with each app relaunch. The distilled view is ~60KB, so it persists in the settings table.
pub fn graph_view() -> Result<GraphView, String> {
    if let Some(v) = mem_cache_get() {
        return Ok(v);
    }
    if let Some(conn) = crate::db::open_default() {
        if let (Some(js), Some(ts)) = (
            crate::db::get_setting(&conn, GRAPH_CACHE_JSON),
            crate::db::get_setting(&conn, GRAPH_CACHE_TS).and_then(|t| t.parse::<i64>().ok()),
        ) {
            if let Ok(v) = serde_json::from_str::<GraphView>(&js) {
                if crate::db::now_secs() - ts > GRAPH_STALE_SECS {
                    spawn_background_refresh(); // serve stale now, fresh next open
                }
                mem_cache_put(&v);
                return Ok(v);
            }
        }
    }
    build_graph_view()
}

fn mem_cache_get() -> Option<GraphView> {
    let guard = GRAPH_MEM.lock().unwrap_or_else(|p| p.into_inner());
    guard.as_ref().filter(|(at, _)| at.elapsed().as_secs() < 180).map(|(_, v)| v.clone())
}

fn mem_cache_put(v: &GraphView) {
    *GRAPH_MEM.lock().unwrap_or_else(|p| p.into_inner()) =
        Some((std::time::Instant::now(), v.clone()));
}

static GRAPH_MEM: std::sync::Mutex<Option<(std::time::Instant, GraphView)>> =
    std::sync::Mutex::new(None);
static GRAPH_REFRESHING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// One background rebuild at a time (no stampede when the view is opened repeatedly while stale).
fn spawn_background_refresh() {
    use std::sync::atomic::Ordering;
    if GRAPH_REFRESHING.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| {
        if let Err(e) = build_graph_view() {
            println!("[quill] graph refresh failed ({e}) — serving the cached view");
        }
        GRAPH_REFRESHING.store(false, Ordering::SeqCst);
    });
}

/// The expensive path: fetch the raw graph (which can be large), distill + fold, persist to both caches.
fn build_graph_view() -> Result<GraphView, String> {
    let ds_id = first_dataset_id()?;
    let resp = client(60)?
        .get(format!("{}/datasets/{ds_id}/graph", base()))
        .send()
        .map_err(|e| format!("cognee graph: {e}"))?;
    let val: serde_json::Value = resp.json().map_err(|e| format!("cognee graph: bad json: {e}"))?;

    let empty = Vec::new();
    let raw_nodes = val["nodes"].as_array().unwrap_or(&empty);
    let raw_edges = val["edges"].as_array().unwrap_or(&empty);

    // id → (label, description); EntityType id → label (for the is_a typing pass).
    let mut entities: std::collections::HashMap<&str, (&str, &str)> =
        std::collections::HashMap::new();
    let mut type_labels: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for n in raw_nodes {
        let (Some(id), Some(label)) = (n["id"].as_str(), n["label"].as_str()) else { continue };
        match n["type"].as_str() {
            Some("Entity") => {
                let desc = n["properties"]["description"].as_str().unwrap_or("");
                if is_junk_entity(label) {
                    continue; // hard label noise: dates/numbers/urls/paths/files/uuids/code
                }
                if is_soft_noise(desc) && !matches!(desc_type(desc), "People" | "Orgs") {
                    continue; // media/file/ephemera — unless it's really a person/org named in passing
                }
                entities.insert(id, (label, desc));
            }
            Some("EntityType") => {
                type_labels.insert(id, label);
            }
            _ => {}
        }
    }

    let mut cat_of: std::collections::HashMap<&str, &'static str> =
        std::collections::HashMap::new();
    let mut degree: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();
    let mut ties: Vec<(&str, &str)> = Vec::new();
    for e in raw_edges {
        let (Some(s), Some(t)) = (e["source"].as_str(), e["target"].as_str()) else { continue };
        let (s_ent, t_ent) = (entities.contains_key(s), entities.contains_key(t));
        if s_ent && t_ent {
            *degree.entry(s).or_default() += 1;
            *degree.entry(t).or_default() += 1;
            ties.push((s, t));
        } else if s_ent && e["label"].as_str() == Some("is_a") {
            if let Some(tl) = type_labels.get(t) {
                cat_of.insert(s, bucket(tl));
            }
        }
    }

    // cognee gives only ~5% of entities an is_a→EntityType edge, so without this the constellation
    // would be almost entirely uncategorized. Fill every remaining entity's category from its
    // description ("A person who…" → People, "A company that…" → Orgs). is_a stays authoritative.
    for (&id, &(_label, desc)) in &entities {
        cat_of.entry(id).or_insert_with(|| desc_type(desc));
    }

    // Selection: top-K by tie degree overall, UNIONED with the top of each category — so a
    // tapped category chip lights a real constellation, not just its members that happened to
    // crack the global top slice (observed live: "Topics 261" but a handful visible).
    const MAX_NODES: usize = 220;
    const PER_CAT: usize = 30;
    const MAX_EDGES: usize = 900;
    let mut ranked_all: Vec<(&str, u32)> = entities
        .keys()
        .map(|id| (*id, degree.get(id).copied().unwrap_or(0)))
        .collect();
    ranked_all.sort_by_key(|(id, d)| (std::cmp::Reverse(*d), !cat_of.contains_key(id)));
    let mut chosen: Vec<(&str, u32)> = Vec::new();
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for (id, d) in ranked_all.iter().take(MAX_NODES) {
        chosen.push((id, *d));
        seen.insert(id);
    }
    for cat in ["People", "Orgs", "Projects", "Tools", "Topics"] {
        let mut added = chosen.iter().filter(|(id, _)| cat_of.get(id) == Some(&cat)).count();
        for (id, d) in &ranked_all {
            if added >= PER_CAT {
                break;
            }
            if cat_of.get(id) == Some(&cat) && seen.insert(id) {
                chosen.push((id, *d));
                added += 1;
            }
        }
    }
    let ranked = chosen;
    let index: std::collections::HashMap<&str, u16> =
        ranked.iter().enumerate().map(|(i, (id, _))| (*id, i as u16)).collect();

    let nodes: Vec<GraphNode> = ranked
        .iter()
        .map(|(id, d)| {
            let (label, desc) = entities[id];
            GraphNode {
                label: label.to_string(),
                cat: cat_of.get(id).copied().unwrap_or("").to_string(),
                deg: *d,
                desc: desc.chars().take(220).collect(),
            }
        })
        .collect();
    let mut edges: Vec<(u16, u16)> = ties
        .iter()
        .filter_map(|(s, t)| Some((*index.get(s)?, *index.get(t)?)))
        .filter(|(a, b)| a != b)
        .collect();
    edges.sort_unstable();
    edges.dedup();
    edges.truncate(MAX_EDGES);

    // Chip counts = what's actually rendered and tappable; grand totals live in the footer.
    let mut cat_counts: std::collections::HashMap<&'static str, u32> =
        std::collections::HashMap::new();
    for (id, _) in &ranked {
        if let Some(c) = cat_of.get(id) {
            *cat_counts.entry(c).or_default() += 1;
        }
    }
    let mut cats: Vec<(String, u32)> =
        cat_counts.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    cats.sort_by_key(|(_, v)| std::cmp::Reverse(*v));

    let view = fold_aliases(GraphView {
        nodes,
        edges,
        cats,
        total_entities: entities.len() as u32,
        total_ties: ties.len() as u32,
    });
    mem_cache_put(&view);
    if let Some(conn) = crate::db::open_default() {
        if let Ok(js) = serde_json::to_string(&view) {
            let _ = crate::db::set_setting(&conn, GRAPH_CACHE_JSON, &js);
            let _ =
                crate::db::set_setting(&conn, GRAPH_CACHE_TS, &crate::db::now_secs().to_string());
        }
    }
    Ok(view)
}

/// Client-side alias folding for the constellation (P4a). cognee keys entities by exact
/// normalized name (`generate_node_id` = uuid5 of the lowercased/underscored label), so "Jordan
/// Lee Rivera", "jordan lee rivera" and "jordan lee" are separate nodes FOREVER — memify
/// can't merge them (verified in cognee source). We can't safely rewrite the graph, but we CAN
/// fold look-alikes in the VIEW so the constellation reads as one dot. Conservative by design:
///   (1) exact letters-only equality, ALL categories — 100% safe (same words, differing only in
///       case / spacing / punctuation): "ACME" ≡ "acme", "jordan_lee_rivera" ≡ "Jordan Lee…".
///   (2) People-only token-PREFIX: a ≥2-token name that prefixes a longer one ("jordan lee" ⊂
///       "jordan lee rivera"). A person-naming pattern — never merges "Microsoft" into
///       "Microsoft Azure" (those aren't People, and single-token names never fold).
/// Degrees sum, edges repoint to the canonical (highest-degree) node, chip counts recompute.
fn fold_aliases(v: GraphView) -> GraphView {
    use std::collections::HashMap;
    let n = v.nodes.len();
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(p: &mut Vec<usize>, mut x: usize) -> usize {
        while p[x] != x {
            p[x] = p[p[x]];
            x = p[x];
        }
        x
    }
    let union = |p: &mut Vec<usize>, a: usize, b: usize| {
        let (ra, rb) = (find(p, a), find(p, b));
        if ra != rb {
            p[ra] = rb;
        }
    };
    let letters = |s: &str| -> String {
        s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
    };
    let tokens = |s: &str| -> Vec<String> {
        s.split_whitespace()
            .map(|t| t.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect::<String>())
            .filter(|t| !t.is_empty())
            .collect()
    };
    // (1) exact letters-equality — all categories.
    let mut by_letters: HashMap<String, usize> = HashMap::new();
    for i in 0..n {
        let k = letters(&v.nodes[i].label);
        if k.len() < 3 {
            continue;
        }
        match by_letters.get(&k) {
            Some(&j) => union(&mut parent, i, j),
            None => {
                by_letters.insert(k, i);
            }
        }
    }
    // (2) People token-prefix.
    let people: Vec<usize> = (0..n).filter(|&i| v.nodes[i].cat == "People").collect();
    let toks: Vec<Vec<String>> = people.iter().map(|&i| tokens(&v.nodes[i].label)).collect();
    for a in 0..people.len() {
        for b in 0..people.len() {
            if a == b {
                continue;
            }
            let (ta, tb) = (&toks[a], &toks[b]);
            if ta.len() >= 2 && ta.len() < tb.len() && tb[..ta.len()] == ta[..] {
                union(&mut parent, people[a], people[b]);
            }
        }
    }
    // Summed degree per set; representative = highest degree (tie → longest label).
    let mut deg_sum: HashMap<usize, u32> = HashMap::new();
    for i in 0..n {
        *deg_sum.entry(find(&mut parent, i)).or_default() += v.nodes[i].deg;
    }
    let mut rep: HashMap<usize, usize> = HashMap::new();
    for i in 0..n {
        let r = find(&mut parent, i);
        let e = rep.entry(r).or_insert(i);
        let better = v.nodes[i].deg > v.nodes[*e].deg
            || (v.nodes[i].deg == v.nodes[*e].deg
                && v.nodes[i].label.len() > v.nodes[*e].label.len());
        if better {
            *e = i;
        }
    }
    // Collapse: one node per set (rep's label/cat/desc, summed degree); remap edges.
    let mut root_new: HashMap<usize, u16> = HashMap::new();
    let mut new_nodes: Vec<GraphNode> = Vec::new();
    let mut old_to_new: Vec<u16> = vec![0; n];
    for i in 0..n {
        let r = find(&mut parent, i);
        let idx = *root_new.entry(r).or_insert_with(|| {
            let rp = rep[&r];
            let idx = new_nodes.len() as u16;
            new_nodes.push(GraphNode {
                label: v.nodes[rp].label.clone(),
                cat: v.nodes[rp].cat.clone(),
                deg: deg_sum[&r],
                desc: v.nodes[rp].desc.clone(),
            });
            idx
        });
        old_to_new[i] = idx;
    }
    let mut edges: Vec<(u16, u16)> = v
        .edges
        .iter()
        .map(|&(a, b)| {
            let (na, nb) = (old_to_new[a as usize], old_to_new[b as usize]);
            if na <= nb { (na, nb) } else { (nb, na) }
        })
        .filter(|(a, b)| a != b)
        .collect();
    edges.sort_unstable();
    edges.dedup();
    let mut cat_counts: HashMap<String, u32> = HashMap::new();
    for node in &new_nodes {
        if !node.cat.is_empty() {
            *cat_counts.entry(node.cat.clone()).or_default() += 1;
        }
    }
    let mut cats: Vec<(String, u32)> = cat_counts.into_iter().collect();
    cats.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
    GraphView { nodes: new_nodes, edges, cats, total_entities: v.total_entities, total_ties: v.total_ties }
}

/// Any dataset id — the graph endpoint returns the shared graph regardless (verified live).
fn first_dataset_id() -> Result<String, String> {
    let resp = client(10)?
        .get(format!("{}/datasets", base()))
        .send()
        .map_err(|e| format!("cognee datasets: {e}"))?;
    let val: serde_json::Value =
        resp.json().map_err(|e| format!("cognee datasets: bad json: {e}"))?;
    val.as_array()
        .and_then(|a| a.first())
        .and_then(|d| d["id"].as_str())
        .map(str::to_string)
        .ok_or_else(|| "no datasets yet".to_string())
}

/// Dataset names for the panel's memory view (what memory is organized into).
pub fn list_datasets() -> Result<Vec<String>, String> {
    let resp = client(10)?
        .get(format!("{}/datasets", base()))
        .send()
        .map_err(|e| format!("cognee datasets: {e}"))?;
    let val: serde_json::Value =
        resp.json().map_err(|e| format!("cognee datasets: bad json: {e}"))?;
    Ok(val
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|d| d["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default())
}

/// remember(): ingest text AND build the graph in one call (add + cognify + improve).
/// The hot lane — high-signal events only (sent drafts, user edits, ⌥-press field content).
/// `dataset` = surface dataset ("app-outlook", "corrections"). Blocking (~7-15s) — call it from
/// a background thread, never the trigger hot path.
pub fn remember(text: &str, dataset: &str) -> Result<(), String> {
    let form = reqwest::blocking::multipart::Form::new()
        .part(
            "data",
            reqwest::blocking::multipart::Part::text(text.to_string())
                .file_name("quill-memory.txt")
                .mime_str("text/plain")
                .map_err(|e| format!("cognee remember: mime: {e}"))?,
        )
        .text("datasetName", dataset.to_string())
        // Background like add(): we never consume the result inline, and synchronous remember
        // competes with running pipelines (observed timeouts under cognify load).
        .text("run_in_background", "true");
    let resp = client(180)?
        .post(format!("{}/remember", base()))
        .multipart(form)
        .send()
        .map_err(|e| format!("cognee remember: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().unwrap_or_default();
        return Err(format!("cognee remember {status}: {}", clip_chars(&body, 300)));
    }
    Ok(())
}

/// add(): cheap ingestion WITHOUT graph building (the bulk lane — ambient snapshots).
/// A later `cognify(dataset)` processes everything unprocessed.
pub fn add(text: &str, dataset: &str) -> Result<(), String> {
    let form = reqwest::blocking::multipart::Form::new()
        .part(
            "data",
            reqwest::blocking::multipart::Part::text(text.to_string())
                .file_name("quill-snapshot.txt")
                .mime_str("text/plain")
                .map_err(|e| format!("cognee add: mime: {e}"))?,
        )
        .text("datasetName", dataset.to_string())
        // Fire-and-forget: synchronous ingestion competes with running cognify pipelines and
        // was observed timing out (>60s) under load. The batched cognify tick does the LLM work.
        .text("run_in_background", "true");
    let resp = client(60)?
        .post(format!("{}/add", base()))
        .multipart(form)
        .send()
        .map_err(|e| format!("cognee add: {e:?}"))?; // debug fmt: full error chain (connect vs timeout vs proxy)
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().unwrap_or_default();
        return Err(format!("cognee add {status}: {}", clip_chars(&body, 300)));
    }
    Ok(())
}

/// Extraction prompt for cognify (P4b): cognee's DEFAULT prompt verbatim (its coreference +
/// fullest-name rules are already good) PLUS an anti-fragmentation clause. The default resolves
/// coreference WITHIN a chunk but can't merge across chunks, so a capture that only shows
/// "jordan@example.com" or "@jordan_r" became a separate Person node. Section 5 stops that
/// class of NEW fragmentation (existing nodes are folded in the constellation view — see
/// fold_aliases). `customPrompt` REPLACES the default, so the default is reproduced faithfully.
const EXTRACTION_PROMPT: &str = r##"You are a top-tier algorithm designed for extracting information in structured formats to build a knowledge graph.
**Nodes** represent entities and concepts. They're akin to Wikipedia nodes.
**Edges** represent relationships between concepts. They're akin to Wikipedia links.
Every edge should include a description when the text supports relevant
information about the endpoints. The description must use the endpoint names,
stay dry and efficient, and may include useful qualifiers from the source text.
Do not add outside knowledge.
  - Good: Alice works at Acme as a platform engineer on the search team.
  - Bad: This edge describes an employment relationship.

The aim is to achieve simplicity and clarity in the knowledge graph.
# 1. Labeling Nodes
**Consistency**: Ensure you use basic or elementary types for node labels.
  - For example, when you identify an entity representing a person, always label it as **"Person"**.
  - Avoid using more specific terms like "Mathematician" or "Scientist", keep those as "profession" property.
  - Don't use too generic terms like "Entity".
**Node IDs**: Never utilize integers as node IDs.
  - Node IDs should be names or human-readable identifiers found in the text.
**Node Names**: Every node MUST include a "name" field.
  - Use the most complete human-readable name for the entity (e.g., "Albert Einstein", "Python").
# 2. Handling Numerical Data and Dates
  - For example, when you identify an entity representing a date, make sure it has type **"Date"**.
  - Extract the date in the format "YYYY-MM-DD"
  - If not possible to extract the whole date, extract month or year, or both if available.
  - **Property Format**: Properties must be in a key-value format.
  - **Quotation Marks**: Never use escaped single or double quotes within property values.
  - **Naming Convention**: Use snake_case for relationship names, e.g., `acted_in`.
# 3. Coreference Resolution
  - **Maintain Entity Consistency**: When extracting entities, it's vital to ensure consistency.
  If an entity, is mentioned multiple times in the text but is referred to by different names or pronouns,
  always use the most complete identifier for that entity throughout the knowledge graph.
Remember, the knowledge graph should be coherent and easily understandable, so maintaining consistency in entity references is crucial.
# 4. Canonical Entities (avoid fragmentation)
  - For PEOPLE and ORGANIZATIONS, the node name MUST be the single most complete real-world name.
  - Email addresses, usernames, @handles, login IDs, and machine/device names are NOT separate
    entities. Attach them as properties of the person or organization they identify when that
    entity is named in the text; if the owner is not named here, omit them rather than create a
    standalone node.
  - Never create a separate node for a partial name (e.g. a first name alone) when a fuller name
    for the same entity appears in the text — use the fuller name.
# 5. Strict Compliance
Adhere to the rules strictly. Non-compliance will result in termination"##;

/// cognify(): build the graph from everything un-processed in `datasets` (background on the
/// sidecar — returns fast; the sidecar churns through the LLM work). Uses EXTRACTION_PROMPT so
/// new captures don't fragment people/orgs into email/handle/partial-name duplicates.
pub fn cognify(datasets: &[&str]) -> Result<(), String> {
    let resp = client(60)?
        .post(format!("{}/cognify", base()))
        .json(&json!({
            "datasets": datasets,
            "runInBackground": true,
            "customPrompt": EXTRACTION_PROMPT,
        }))
        .send()
        .map_err(|e| format!("cognee cognify: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().unwrap_or_default();
        return Err(format!("cognee cognify {status}: {}", clip_chars(&body, 300)));
    }
    Ok(())
}

/// Tuning knobs for a recall. `None` everywhere = the sidecar's own defaults. Every
/// non-default value routed through here must first pass the bench gate
/// (tools/search-bench.py) on a local graph — strategy defaults tuned for cloud models don't
/// automatically transfer to a self-hosted LLM with Ollama embeddings.
#[derive(Debug, Clone, Default)]
pub struct RecallOpts {
    /// cognee SearchType ("GRAPH_COMPLETION", "TEMPORAL", …). None = sidecar default.
    pub search_type: Option<&'static str>,
    pub top_k: Option<u32>,
    pub system_prompt: Option<&'static str>,
    /// Restrict retrieval to these datasets (the active profile's memory circle). None = all
    /// datasets — the pre-circles behaviour, preserved when no memory walls exist.
    pub datasets: Option<Vec<String>>,
}

impl RecallOpts {
    fn is_tuned(&self) -> bool {
        self.search_type.is_some()
            || self.top_k.is_some()
            || self.system_prompt.is_some()
            || self.datasets.is_some()
    }
}

/// recall_opts(): tuned retrieval. If the sidecar REJECTS tuned parameters (4xx/5xx — e.g.
/// an image that lacks a strategy), retry once with plain defaults. Transport errors and
/// timeouts do NOT retry: a slow sidecar won't be faster the second time — the caller
/// degrades to screen-only instead, same as always.
/// Recall circuit breaker: after a recall TIMES OUT (overloaded / unreachable sidecar), skip cognee
/// recall for a short window so every ⌥ press doesn't re-pay the full timeout — the caller falls
/// back to its local FTS lane instantly. One probe after the window auto-recovers when cognee is back.
static RECALL_COLD_UNTIL: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

pub fn recall_opts(query: &str, opts: &RecallOpts) -> Result<Vec<Fact>, String> {
    use std::sync::atomic::Ordering;
    let now = crate::db::now_secs();
    if now < RECALL_COLD_UNTIL.load(Ordering::Relaxed) {
        return Err("cognee recall paused (recent timeout) — using local lane".to_string());
    }
    let r = match recall_once(query, opts) {
        Err(e) if opts.is_tuned() && (e.contains("recall 4") || e.contains("recall 5")) => {
            println!("[quill] tuned recall rejected ({e}) — retrying with sidecar defaults");
            recall_once(query, &RecallOpts::default())
        }
        r => r,
    };
    match &r {
        // Open the breaker ONLY on a genuine TIMEOUT (cognee is slow/overloaded → skip it briefly so
        // the ⌥ press isn't held hostage). A transient connection reset is already retried once in
        // recall_once, and a 4xx/5xx returns fast — NEITHER should blackout recall for 45s, or one
        // blip drops cognee grounding on EVERY draft for the next window (observed live mid-demo).
        Err(e) if e.contains("recall timeout") => {
            RECALL_COLD_UNTIL.store(now + 45, Ordering::Relaxed);
            println!("[quill] cognee recall TIMED OUT — pausing recall 45s so drafts stay fast ({e})");
        }
        Ok(_) => {
            RECALL_COLD_UNTIL.store(0, Ordering::Relaxed);
        }
        // Transient conn error (already retried) or 4xx/5xx: this draft falls back to local FTS, but
        // keep the breaker CLOSED so the very next ⌥ press tries cognee again.
        Err(_) => {}
    }
    r
}

/// Hard 10s budget: memory is an ENHANCEMENT to the draft, not a requirement — under sidecar
/// load we'd rather draft from the screen than hold the user's ⌥ press hostage (observed:
/// a 45s budget stacked with LLM latency = a 2-minute press).
fn recall_once(query: &str, opts: &RecallOpts) -> Result<Vec<Fact>, String> {
    // Use the V1 /search endpoint with an EXPLICIT strategy, NOT /recall. cognee's newer /recall
    // runs a query-router that OVERRIDES our requested CHUNKS with GRAPH_COMPLETION (a full LLM
    // pass — 40s+ on our rate-limited LLM), which times out and jams the sidecar's workers
    // (root cause of the "cognee failing" outages). /search honours searchType, so recall stays
    // embedding-only and fast (~3s, verified live). systemPrompt is moot for CHUNKS; circle-scoping
    // (datasets) is temporarily dropped here until /search's scoping param is verified.
    let mut body = json!({
        "query": query,
        "searchType": opts.search_type.unwrap_or("CHUNKS"),
    });
    if let Some(k) = opts.top_k {
        body["topK"] = json!(k);
    }
    // Send with ONE retry on a transient connection reset. The single-worker sidecar intermittently
    // drops the hot-path connection when it's busy (mid-cognify) — the symptom where curl succeeds
    // but our pooled client sees "error sending request". A quick retry almost always lands, keeping
    // the flagship ⌥ draft grounded in cognee instead of silently degrading to local FTS. The error
    // is tagged timeout-vs-conn so recall_opts only trips the 45s breaker on a genuine timeout.
    let url = format!("{}/search", base());
    let do_send = || -> Result<reqwest::blocking::Response, String> {
        // 10s, matching the documented budget above: healthy recall is ~4s but spikes to 6-7s while
        // the single-worker sidecar drains a cognify burst — a 6s cap tripped the breaker on those,
        // blacking out cognee grounding on every draft for the next 45s. 10s rides through the spike.
        client(10)?
            .post(url.clone())
            .json(&body)
            .send()
            .map_err(|e| {
                if e.is_timeout() {
                    format!("cognee recall timeout: {e}")
                } else {
                    format!("cognee recall conn: {e}")
                }
            })
    };
    let resp = match do_send() {
        Ok(r) => r,
        Err(e) if e.contains("recall conn") => {
            println!("[quill] recall: transient connection reset — retrying once");
            do_send()?
        }
        Err(e) => return Err(e),
    };
    let status = resp.status();
    let val: serde_json::Value = resp.json().map_err(|e| format!("cognee recall: bad json: {e}"))?;
    if !status.is_success() {
        return Err(format!("cognee recall {status}: {val}"));
    }
    let arr = val.as_array().cloned().unwrap_or_default();
    // Diagnostic (observed live: the app got 1 item where an identical curl got 12 — this
    // line splits "server returned few" from "parse dropped many" without a debugger).
    println!(
        "[quill] recall raw: {} item(s) for query {}ch {:?}, first: {:?}",
        arr.len(),
        query.chars().count(),
        query.chars().take(60).collect::<String>(),
        arr.first().map(|r| {
            let t = r["text"].as_str().unwrap_or("<no text field>");
            t.chars().take(60).collect::<String>()
        })
    );
    if arr.len() <= 2 {
        let dump = val.to_string();
        println!("[quill] recall raw json: {}", clip_chars(&dump, 400));
    }
    Ok(arr
        .into_iter()
        .filter_map(|r| {
            let text = r["text"].as_str()?.trim().to_string();
            if text.is_empty() {
                return None;
            }
            Some(Fact {
                text,
                dataset: r["dataset_name"].as_str().map(str::to_string),
            })
        })
        .collect())
}

/// memify(): cognee's graph-enrichment pass. Its `/memify` is PER-DATASET — it 400s on an empty
/// body ("Either datasetId or datasetName must be provided"), so we rotate through the datasets one
/// per call via a persisted cursor: bounded load, full coverage over time. With our config
/// (triplet_embedding off) memify runs only the enrichment task (re-index datapoints) to keep the
/// vector index fresh — it does NOT prune nodes. Node pruning is `forget` at the dataset level (or
/// the one-off graph cleanup). runInBackground so the call returns fast and the sidecar churns.
const MEMIFY_CURSOR_KEY: &str = "memify_cursor";

pub fn memify() -> Result<(), String> {
    // Skip test/eval datasets — memifying them is wasted work.
    let datasets: Vec<String> = list_datasets()?
        .into_iter()
        .filter(|d| !d.ends_with("-test") && !d.starts_with("m0-") && !d.starts_with("m1-"))
        .collect();
    if datasets.is_empty() {
        return Ok(());
    }
    let next = crate::db::open_default()
        .and_then(|c| crate::db::get_setting(&c, MEMIFY_CURSOR_KEY))
        .and_then(|last| datasets.iter().position(|d| *d == last))
        .map(|i| (i + 1) % datasets.len())
        .unwrap_or(0);
    let ds = datasets[next].clone();
    let resp = client(120)?
        .post(format!("{}/memify", base()))
        .json(&json!({ "datasetName": ds, "runInBackground": true }))
        .send()
        .map_err(|e| format!("cognee memify: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().unwrap_or_default();
        return Err(format!("cognee memify {status}: {}", clip_chars(&body, 300)));
    }
    if let Some(c) = crate::db::open_default() {
        let _ = crate::db::set_setting(&c, MEMIFY_CURSOR_KEY, &ds);
    }
    println!("[quill] memify → cognee [{ds}] (enrichment pass, background)");
    Ok(())
}

/// forget(): drop an entire surface dataset (app exclusion → its memory disappears).
pub fn forget_dataset(dataset: &str) -> Result<(), String> {
    let resp = client(60)?
        .post(format!("{}/forget", base()))
        .json(&json!({ "dataset": dataset }))
        .send()
        .map_err(|e| format!("cognee forget: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().unwrap_or_default();
        return Err(format!("cognee forget {status}: {}", clip_chars(&body, 300)));
    }
    Ok(())
}

// ── Cursor-based sync (the self-healing bulk lane) ────────────────────────────
// An ambient memory product must never silently lose a capture. The push-lane can drop adds
// under load (observed: hundreds of TimedOuts), so this lane keeps a PERSISTENT cursor in the
// settings table and pulls everything after it into cognee. A failed batch simply doesn't
// advance the cursor — retry is automatic, dropping is structurally impossible, and time (not
// manual sweeps) converges the graph.

pub const SYNC_CURSOR_KEY: &str = "cognee_sync_cursor";
const SYNC_BATCH: usize = 25;

/// One sync step: push up to SYNC_BATCH un-synced snapshots, advance the cursor on success.
/// Duplicate-content rejections count as success (the data is already in the graph — the rich
/// live-add path got it there first with domain routing).
pub fn sync_step() {
    let Some(conn) = crate::db::open_default() else {
        return;
    };
    let cursor: i64 = match crate::db::get_setting(&conn, SYNC_CURSOR_KEY).and_then(|v| v.parse().ok())
    {
        Some(c) => c,
        None => {
            // First run: sync from NOW. Pre-existing history is a one-time migration concern
            // (seed/sweep tooling), not the ambient lane's — syncing 8k old rows through the
            // app on first launch would be its own storm.
            let now_max = crate::db::max_snapshot_id(&conn);
            let _ = crate::db::set_setting(&conn, SYNC_CURSOR_KEY, &now_max.to_string());
            println!("[quill] cognee sync cursor initialized → #{now_max}");
            return;
        }
    };
    let rows = match crate::db::snapshots_after(&conn, cursor, SYNC_BATCH) {
        Ok(r) if !r.is_empty() => r,
        _ => return, // fully synced (or DB hiccup — retried next tick)
    };
    let mut new_cursor = cursor;
    let mut synced = 0;
    for s in &rows {
        if s.app_bundle.is_empty() || crate::app::is_excluded(&s.app_bundle) {
            new_cursor = s.id; // never memorize excluded surfaces; skip permanently
            continue;
        }
        let ds = dataset_for(&s.app_bundle, None);
        let body = format!("[screen capture · {}]\n{}", s.app_bundle, clip_chars(&s.text, 5000));
        match add(&body, &ds) {
            Ok(()) => {
                mark_dirty(&ds);
                new_cursor = s.id;
                synced += 1;
            }
            // 4xx/5xx = the server judged the content (usually duplicate) — it exists; advance.
            Err(e) if e.contains("500") || e.contains("409") || e.contains("400") => {
                new_cursor = s.id;
            }
            // Connection/timeout: stop here; the cursor holds and this batch retries later.
            Err(e) => {
                println!("[quill] cognee sync paused at #{} ({e})", s.id);
                break;
            }
        }
    }
    if new_cursor != cursor {
        let _ = crate::db::set_setting(&conn, SYNC_CURSOR_KEY, &new_cursor.to_string());
        if synced > 0 {
            println!("[quill] cognee sync: {synced} snapshot(s), cursor → #{new_cursor}");
        }
    }
}

fn clip_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

// ── Dirty-dataset tracking (bulk lane) ────────────────────────────────────────
// The capture loop `add`s snapshots cheaply and marks their dataset dirty; the existing
// consolidation tick drains the set with one batched `cognify` (a two-speed memory design).

use std::collections::HashSet;
use std::sync::Mutex;

static DIRTY: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// Mark a dataset as having un-cognified data.
pub fn mark_dirty(dataset: &str) {
    let mut guard = DIRTY.lock().unwrap_or_else(|p| p.into_inner());
    guard.get_or_insert_with(HashSet::new).insert(dataset.to_string());
}

/// Cognify everything marked dirty since the last drain (no-op when clean / sidecar down).
pub fn cognify_dirty() {
    let drained: Vec<String> = {
        let mut guard = DIRTY.lock().unwrap_or_else(|p| p.into_inner());
        match guard.take() {
            Some(s) if !s.is_empty() => s.into_iter().collect(),
            _ => return,
        }
    };
    let refs: Vec<&str> = drained.iter().map(String::as_str).collect();
    match cognify(&refs) {
        Ok(()) => println!("[quill] cognee: cognify started for {refs:?}"),
        Err(e) => {
            eprintln!("[quill] cognee: cognify failed ({e}) — re-marking dirty");
            for d in &drained {
                mark_dirty(d);
            }
        }
    }
}

/// The cognee dataset for a capture: domain when we have one (web surfaces work in ANY browser),
/// else the app bundle. "corrections" is reserved for improve()-lane edits.
/// Cognee rejects dataset names containing spaces or dots (500) — sanitize to [a-z0-9_-].
pub fn dataset_for(app_bundle: &str, domain: Option<&str>) -> String {
    let raw = match domain {
        Some(d) if !d.is_empty() => format!("domain-{d}"),
        _ => format!(
            "app-{}",
            app_bundle.rsplit('.').next().unwrap_or(app_bundle)
        ),
    };
    raw.to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '-' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_aliases_merges_variants_and_person_partials_not_distinct_orgs() {
        let mk = |label: &str, cat: &str, deg: u32| GraphNode {
            label: label.into(),
            cat: cat.into(),
            deg,
            desc: String::new(),
        };
        let nodes = vec![
            mk("Jordan Lee Rivera", "People", 10),
            mk("jordan lee rivera", "People", 3), // case/space variant → folds
            mk("jordan lee", "People", 2),          // person partial → folds into the full name
            mk("Microsoft", "Orgs", 8),
            mk("Microsoft Azure", "Tools", 5),      // distinct — must NOT fold into Microsoft
            mk("Acme", "Orgs", 6),
            mk("acme", "Orgs", 1),                 // case variant → folds
        ];
        let v = GraphView {
            nodes,
            edges: vec![(0, 3), (2, 5)],
            cats: vec![],
            total_entities: 7,
            total_ties: 2,
        };
        let f = fold_aliases(v);
        let labels: Vec<&str> = f.nodes.iter().map(|n| n.label.as_str()).collect();
        // The three "jordan" nodes collapse to one, keeping the fullest/highest-degree label.
        assert_eq!(labels.iter().filter(|l| l.to_lowercase().contains("jordan")).count(), 1);
        let jordan = f.nodes.iter().find(|n| n.label == "Jordan Lee Rivera").unwrap();
        assert_eq!(jordan.deg, 15, "folded degree sums 10+3+2");
        // Microsoft ≠ Microsoft Azure (distinct entities preserved).
        assert!(labels.contains(&"Microsoft") && labels.iter().any(|l| l.contains("Azure")));
        // Acme case variants merge to one.
        assert_eq!(labels.iter().filter(|l| l.to_lowercase() == "acme").count(), 1);
    }

    #[test]
    fn dataset_for_prefers_domain_and_sanitizes() {
        // Cognee 500s on dots/spaces — verified live (M3 runtime check): they must be dashed.
        assert_eq!(
            dataset_for("ai.perplexity.comet", Some("linkedin.com")),
            "domain-linkedin-com"
        );
        assert_eq!(
            dataset_for("com.microsoft.teams2", Some("teams.microsoft.com")),
            "domain-teams-microsoft-com"
        );
        assert_eq!(dataset_for("com.microsoft.Outlook", None), "app-outlook");
        assert_eq!(dataset_for("com.microsoft.teams2", Some("")), "app-teams2");
    }

    #[test]
    fn base_url_default_points_at_sidecar() {
        // No env override in tests → the documented local sidecar port.
        if std::env::var("QUILL_COGNEE_URL").is_err() {
            assert_eq!(base(), "http://127.0.0.1:8765/api/v1");
        }
    }
}
