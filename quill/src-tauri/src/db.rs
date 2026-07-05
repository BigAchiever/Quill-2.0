// Local SQLite store (Phase 2). Snapshots = raw captured on-screen text.
// Entities / chronicle / embeddings come in Phase 3.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use rusqlite::{params, Connection, OptionalExtension};

static DB_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Remember where the DB lives (set once at startup) so any thread can open it.
pub fn set_db_path(p: PathBuf) {
    let _ = DB_PATH.set(p);
}

/// Open a fresh connection to the configured DB. WAL mode → safe to read while the
/// capture thread writes. Cheap enough to open per draft (not on the hot path).
pub fn open_default() -> Option<Connection> {
    let p = DB_PATH.get()?;
    open(p).ok()
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Current LOCAL date+time as a human string (via SQLite's timezone-aware 'localtime'), e.g.
/// "Saturday 2026-07-05 19:42". Passed to the chat LLM so it knows what "today"/"now" mean and
/// doesn't treat dates found inside the captured text as today.
pub fn now_local(conn: &Connection) -> String {
    conn.query_row(
        "SELECT strftime('%Y-%m-%d %H:%M', 'now', 'localtime') || ' (' ||
                CASE cast(strftime('%w','now','localtime') as int)
                  WHEN 0 THEN 'Sunday' WHEN 1 THEN 'Monday' WHEN 2 THEN 'Tuesday'
                  WHEN 3 THEN 'Wednesday' WHEN 4 THEN 'Thursday' WHEN 5 THEN 'Friday'
                  ELSE 'Saturday' END || ')'",
        [],
        |r| r.get::<_, String>(0),
    )
    .unwrap_or_default()
}

/// Table + index definitions. Kept separate from the WAL pragma so tests (incl. other
/// modules') can build the same schema on an in-memory connection.
pub(crate) const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS snapshots (
        id             INTEGER PRIMARY KEY,
        ts             INTEGER NOT NULL,
        app_bundle     TEXT,
        text           TEXT NOT NULL,
        text_hash      INTEGER,
        window_title   TEXT,
        url            TEXT,
        domain         TEXT,
        focused_name   TEXT,
        focused_role   TEXT,
        focused_path   TEXT,
        last_seen_at   INTEGER,
        sighting_count INTEGER NOT NULL DEFAULT 1,
        duration_s     REAL NOT NULL DEFAULT 0
    );
    CREATE INDEX IF NOT EXISTS idx_snapshots_ts  ON snapshots(ts);
    CREATE INDEX IF NOT EXISTS idx_snapshots_app ON snapshots(app_bundle, ts);
    CREATE TABLE IF NOT EXISTS settings (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS entities (
        id                INTEGER PRIMARY KEY,
        name              TEXT NOT NULL,
        norm_name         TEXT NOT NULL,
        kind              TEXT NOT NULL,
        observation_count INTEGER NOT NULL DEFAULT 1,
        first_seen        INTEGER NOT NULL,
        last_seen         INTEGER NOT NULL,
        description       TEXT
    );
    CREATE UNIQUE INDEX IF NOT EXISTS idx_entities_norm ON entities(norm_name, kind);
    CREATE TABLE IF NOT EXISTS ties (
        id    INTEGER PRIMARY KEY,
        src   INTEGER NOT NULL,
        dst   INTEGER NOT NULL,
        kind  TEXT NOT NULL,
        count INTEGER NOT NULL DEFAULT 1
    );
    CREATE UNIQUE INDEX IF NOT EXISTS idx_ties_unique ON ties(src, dst, kind);
    CREATE TABLE IF NOT EXISTS style_samples (
        id         INTEGER PRIMARY KEY,
        surface    TEXT NOT NULL,
        text       TEXT NOT NULL,
        created_at INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_style_samples_surface ON style_samples(surface, created_at);
    CREATE TABLE IF NOT EXISTS style_notes (
        note_id    INTEGER PRIMARY KEY,
        surface    TEXT NOT NULL,
        bullet     TEXT NOT NULL,
        updated_at INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_style_notes_surface ON style_notes(surface);
    CREATE TABLE IF NOT EXISTS working_memory (
        snapshot_id INTEGER PRIMARY KEY,
        added_at    INTEGER NOT NULL,
        relevance   REAL NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_working_memory_rel ON working_memory(relevance DESC);
    CREATE TABLE IF NOT EXISTS events (
        id    INTEGER PRIMARY KEY,
        ts    INTEGER NOT NULL,
        kind  TEXT NOT NULL,
        title TEXT NOT NULL,
        body  TEXT NOT NULL DEFAULT '',
        read  INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX IF NOT EXISTS idx_events_ts ON events(ts DESC);
    CREATE TABLE IF NOT EXISTS wiki_pages (
        slug          TEXT PRIMARY KEY,
        title         TEXT NOT NULL,
        kind          TEXT NOT NULL DEFAULT '',
        aliases       TEXT NOT NULL DEFAULT '[]',
        summary       TEXT NOT NULL DEFAULT '',
        mention_count INTEGER NOT NULL DEFAULT 0,
        snapshot_ids  TEXT NOT NULL DEFAULT '[]',
        first_seen    INTEGER,
        last_seen     INTEGER,
        updated_at    INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_wiki_updated ON wiki_pages(updated_at DESC);
    CREATE TABLE IF NOT EXISTS identity_proposals (
        id         INTEGER PRIMARY KEY,
        kind       TEXT NOT NULL,               -- 'voice' | 'identity'
        prof_key   TEXT NOT NULL DEFAULT '',    -- profile key for voice; '' for global identity
        before_val TEXT NOT NULL DEFAULT '',
        after_val  TEXT NOT NULL,
        created_at INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_proposals_created ON identity_proposals(created_at DESC);";

/// Open (creating if needed) the quill DB and run migrations.
pub fn open(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    // WAL = concurrent readers. busy_timeout = a writer WAITS (up to 3s) instead of failing
    // with SQLITE_BUSY when capture / style / settings write from different connections at once.
    conn.execute_batch("PRAGMA journal_mode=WAL;")?;
    // NORMAL (not the default FULL) is the right durability/speed point under WAL: still safe
    // against app crashes, but far fewer fsyncs → no write "spikes". busy_timeout = a writer
    // WAITS (3s) instead of failing with SQLITE_BUSY when capture/style/settings write at once.
    conn.execute_batch("PRAGMA synchronous=NORMAL;")?;
    conn.execute_batch("PRAGMA busy_timeout=3000;")?;
    conn.execute_batch(SCHEMA)?;
    migrate_snapshots(&conn)?; // v2 columns on pre-existing DBs (fresh DBs get them from SCHEMA)
    migrate_fts(&conn)?; // local FTS5 lexical index (must run AFTER the v2 columns exist)
    Ok(conn)
}

/// FTS5 external-content index over snapshots (P2): a LOCAL, always-available lexical/exact
/// recall lane that works with the sidecar down. `content='snapshots'` stores no duplicate text;
/// triggers keep it in sync. The UPDATE trigger fires only on content columns, so dwell-merges
/// (which touch only last_seen_at/sighting_count/duration_s) don't churn the index.
const FTS_SCHEMA: &str = "
    CREATE VIRTUAL TABLE IF NOT EXISTS snapshots_fts USING fts5(
        text, window_title, domain, app_bundle,
        content='snapshots', content_rowid='id'
    );
    CREATE TRIGGER IF NOT EXISTS snapshots_fts_ai AFTER INSERT ON snapshots BEGIN
        INSERT INTO snapshots_fts(rowid, text, window_title, domain, app_bundle)
        VALUES (new.id, new.text, new.window_title, new.domain, new.app_bundle);
    END;
    CREATE TRIGGER IF NOT EXISTS snapshots_fts_ad AFTER DELETE ON snapshots BEGIN
        INSERT INTO snapshots_fts(snapshots_fts, rowid, text, window_title, domain, app_bundle)
        VALUES ('delete', old.id, old.text, old.window_title, old.domain, old.app_bundle);
    END;
    CREATE TRIGGER IF NOT EXISTS snapshots_fts_au AFTER UPDATE OF text, window_title, domain ON snapshots BEGIN
        INSERT INTO snapshots_fts(snapshots_fts, rowid, text, window_title, domain, app_bundle)
        VALUES ('delete', old.id, old.text, old.window_title, old.domain, old.app_bundle);
        INSERT INTO snapshots_fts(rowid, text, window_title, domain, app_bundle)
        VALUES (new.id, new.text, new.window_title, new.domain, new.app_bundle);
    END;";

fn migrate_fts(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(FTS_SCHEMA)?;
    // One-time backfill of pre-existing rows. 'rebuild' repopulates an external-content FTS from
    // its content table — but its `count(*)` reads THROUGH to the content table, so it can't be
    // used as an emptiness check (that was the original bug: the backfill never ran). Guard with
    // a settings flag instead: rebuild exactly once, self-healing for DBs opened before this fix.
    const FLAG: &str = "fts_backfilled_v1";
    if get_setting(conn, FLAG).is_none() {
        conn.execute_batch("INSERT INTO snapshots_fts(snapshots_fts) VALUES('rebuild');")?;
        set_setting(conn, FLAG, "1")?;
    }
    Ok(())
}

/// A local recall hit — a real snapshot row (with provenance) that the sidecar-free FTS lane found.
#[derive(Debug, Clone)]
pub struct LocalHit {
    pub id: i64,
    pub app_bundle: String,
    pub domain: Option<String>,
    pub window_title: Option<String>,
    pub text: String,
    pub score: f64, // bm25 (lower = more relevant)
}

/// Turn free text into a safe FTS5 MATCH: alphanumeric tokens, quoted, OR-joined (relevance via
/// bm25). Quoting neutralizes FTS operators in user text; a cap bounds the query.
fn fts_match_query(raw: &str) -> String {
    raw.split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.chars().count() >= 2)
        .take(24)
        .map(|t| format!("\"{}\"", t.to_lowercase()))
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// Local lexical recall over snapshots — always available, sub-second, with local provenance.
/// Excludes the current surface's own rows (its screen is already the primary context).
pub fn search_snapshots_fts(
    conn: &Connection,
    query: &str,
    exclude_bundle: &str,
    limit: usize,
) -> rusqlite::Result<Vec<LocalHit>> {
    let m = fts_match_query(query);
    if m.is_empty() {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT s.id, s.app_bundle, s.domain, s.window_title, s.text, bm25(snapshots_fts)
         FROM snapshots_fts f JOIN snapshots s ON s.id = f.rowid
         WHERE snapshots_fts MATCH ?1 AND s.app_bundle != ?2
         ORDER BY bm25(snapshots_fts) LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![m, exclude_bundle, limit as i64], |r| {
        Ok(LocalHit {
            id: r.get(0)?,
            app_bundle: r.get(1)?,
            domain: r.get(2)?,
            window_title: r.get(3)?,
            text: r.get(4)?,
            score: r.get(5)?,
        })
    })?;
    Ok(rows.flatten().collect())
}

/// Real UTC epoch for a LOCAL wall-clock datetime string ("YYYY-MM-DD HH:MM:SS"), using the
/// machine's current UTC↔local offset. Lets us compute "today 3 PM", "yesterday", etc. from the
/// local clock — no timezone assumptions, works on any machine.
pub fn local_epoch(conn: &Connection, local_dt: &str) -> Option<i64> {
    conn.query_row(
        "SELECT CAST(strftime('%s', ?1) AS INTEGER)
                - (strftime('%s','now','localtime') - strftime('%s','now'))",
        params![local_dt],
        |r| r.get::<_, i64>(0),
    )
    .ok()
}

/// Local calendar date `offset_days` from today as 'YYYY-MM-DD' (0 = today, -1 = yesterday).
pub fn local_date(conn: &Connection, offset_days: i64) -> Option<String> {
    conn.query_row(
        "SELECT date('now','localtime', ?1 || ' days')",
        params![offset_days],
        |r| r.get::<_, String>(0),
    )
    .ok()
}

/// Domains + apps captured within [start, end) epoch — the deterministic answer to "what did I do
/// between 3 and 5 PM" / "this morning" / "yesterday", straight from the snapshot rows in that window.
pub fn window_activity(conn: &Connection, start: i64, end: i64) -> (Vec<(String, i64)>, Vec<(String, i64)>) {
    fn agg(conn: &Connection, col: &str, start: i64, end: i64, lim: i64) -> Vec<(String, i64)> {
        // `col` is a fixed literal ("domain"/"app_bundle"), never user input.
        let sql = format!(
            "SELECT {col}, COUNT(*) n FROM snapshots
             WHERE ts >= ?1 AND ts < ?2 AND {col} IS NOT NULL AND {col} <> ''
             GROUP BY {col} ORDER BY n DESC LIMIT {lim}"
        );
        conn.prepare(&sql)
            .and_then(|mut s| {
                let rows = s.query_map(params![start, end], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                })?;
                rows.collect()
            })
            .unwrap_or_default()
    }
    (agg(conn, "domain", start, end, 15), agg(conn, "app_bundle", start, end, 12))
}

/// Domains visited "today" (local day), most-captured first — the FACTUAL answer to "what sites did
/// I visit today", which must come from structured data, NOT semantic recall of old browsing.
pub fn domains_today(conn: &Connection) -> rusqlite::Result<Vec<(String, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT domain, COUNT(*) n FROM snapshots
         WHERE date(ts,'unixepoch','localtime') = date('now','localtime')
           AND domain IS NOT NULL AND domain <> ''
         GROUP BY domain ORDER BY n DESC LIMIT 25",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
    rows.collect()
}

/// Apps used "today" (local day), most-captured first — factual answer to "what apps did I use".
pub fn apps_today(conn: &Connection) -> rusqlite::Result<Vec<(String, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT app_bundle, COUNT(*) n FROM snapshots
         WHERE date(ts,'unixepoch','localtime') = date('now','localtime')
           AND app_bundle IS NOT NULL AND app_bundle <> ''
         GROUP BY app_bundle ORDER BY n DESC LIMIT 20",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
    rows.collect()
}

/// Like `search_snapshots_fts` but ordered by RECENCY (newest first) and carrying each snapshot's
/// timestamp — for "what's the latest with X" questions, where recency beats keyword density.
pub fn search_snapshots_recent(
    conn: &Connection,
    query: &str,
    limit: usize,
) -> rusqlite::Result<Vec<(i64, LocalHit)>> {
    let m = fts_match_query(query);
    if m.is_empty() {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT s.ts, s.id, s.app_bundle, s.domain, s.window_title, s.text, bm25(snapshots_fts)
         FROM snapshots_fts f JOIN snapshots s ON s.id = f.rowid
         WHERE snapshots_fts MATCH ?1
         ORDER BY s.ts DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![m, limit as i64], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            LocalHit {
                id: r.get(1)?,
                app_bundle: r.get(2)?,
                domain: r.get(3)?,
                window_title: r.get(4)?,
                text: r.get(5)?,
                score: r.get(6)?,
            },
        ))
    })?;
    Ok(rows.flatten().collect())
}

/// Additive schema migration for the profiles/dwell work: add the v2 `snapshots` columns to an
/// existing DB. Idempotent — checks `table_info` and only ADDs missing columns, so it runs on
/// every open() harmlessly and needs no `user_version` bookkeeping.
fn migrate_snapshots(conn: &Connection) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(snapshots)")?;
    let existing: std::collections::HashSet<String> =
        stmt.query_map([], |r| r.get::<_, String>(1))?.flatten().collect();
    const ADDS: [(&str, &str); 9] = [
        ("window_title", "TEXT"),
        ("url", "TEXT"),
        ("domain", "TEXT"),
        ("focused_name", "TEXT"),
        ("focused_role", "TEXT"),
        ("focused_path", "TEXT"),
        ("last_seen_at", "INTEGER"),
        ("sighting_count", "INTEGER NOT NULL DEFAULT 1"),
        ("duration_s", "REAL NOT NULL DEFAULT 0"),
    ];
    for (col, ty) in ADDS {
        if !existing.contains(col) {
            conn.execute(&format!("ALTER TABLE snapshots ADD COLUMN {col} {ty}"), [])?;
        }
    }
    // The domain index lives HERE, not in SCHEMA: SCHEMA runs before this migration, so on a
    // pre-existing (v1) DB `domain` doesn't exist yet — creating the index there fails the whole
    // open() and (wrongly) quarantines the DB. Create it only after the column is guaranteed.
    conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_snapshots_domain ON snapshots(domain);")?;
    Ok(())
}

/// Truncating WAL checkpoint — flush the WAL back into the main DB file and shrink it to zero.
/// Best-effort, run on clean exit so we don't leave a large WAL behind for the next start.
pub fn checkpoint(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
}

/// One-time startup integrity gate. If the DB won't open or fails a quick corruption check
/// (e.g. truncated/torn by an abrupt shutdown), move the bad files aside so a fresh DB is
/// created instead of crashing or silently failing every read. Call ONCE before anything else
/// opens the DB.
pub fn prepare(path: &Path) {
    let healthy = match open(path) {
        Ok(conn) => conn
            .query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0))
            .map(|s| s == "ok")
            .unwrap_or(false),
        Err(e) => {
            eprintln!("[quill] db failed to open at startup: {e}");
            false
        }
    };
    if !healthy {
        eprintln!("[quill] db unhealthy — quarantining the bad file(s) and recreating");
        quarantine(path);
    }
}

/// Rename the DB and its WAL/SHM sidecars aside (`.corrupt-<ts>`) so `open()` recreates fresh.
fn quarantine(path: &Path) {
    let ts = now_secs();
    let base = path.to_string_lossy().to_string();
    for suffix in ["", "-wal", "-shm"] {
        let from = PathBuf::from(format!("{base}{suffix}"));
        if from.exists() {
            let to = PathBuf::from(format!("{base}.corrupt-{ts}{suffix}"));
            match std::fs::rename(&from, &to) {
                Ok(()) => eprintln!("[quill] quarantined {} → {}", from.display(), to.display()),
                Err(e) => eprintln!("[quill] could not quarantine {}: {e}", from.display()),
            }
        }
    }
}

/// Insert one captured snapshot.
/// Capture provenance persisted alongside the snapshot text — the anchors `read_anchors`
/// already computes (window title, URL/domain, focused-element breadcrumb). Local now, so
/// recall/UI/time-tracking can use them without the sidecar.
#[derive(Default)]
pub struct SnapMeta<'a> {
    pub window_title: Option<&'a str>,
    pub url: Option<&'a str>,
    pub domain: Option<&'a str>,
    pub focused_name: Option<&'a str>,
    pub focused_role: Option<&'a str>,
    pub focused_path: Option<&'a str>,
}

/// Dwell-aware store: if the app's most-recent snapshot is (near-)identical,
/// UPDATE its dwell — `last_seen_at`, `sighting_count`, `duration_s` — instead of inserting a
/// fresh near-duplicate row. Exact-hash match short-circuits the overlap compute. Returns
/// (row id, merged?). This replaces the in-memory dedup ring: per-app last-row comparison also
/// handles A→B→A alternation (each app's last row is its own), and it accrues a salience signal.
pub fn merge_or_insert_snapshot(
    conn: &Connection,
    app_bundle: &str,
    text: &str,
    text_hash: i64,
    meta: &SnapMeta,
) -> rusqlite::Result<(i64, bool)> {
    const MERGE_OVERLAP: f32 = 0.90;
    const DWELL_CAP_S: i64 = 15; // count continuous dwell only — a return-after-away adds ≤ this
    let last: Option<(i64, i64, String, Option<i64>)> = conn
        .query_row(
            "SELECT id, text_hash, text, last_seen_at FROM snapshots
             WHERE app_bundle=?1 ORDER BY id DESC LIMIT 1",
            params![app_bundle],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let now = now_secs();
    if let Some((id, last_hash, last_text, last_seen)) = last {
        let same = last_hash == text_hash
            || crate::util::word_overlap(text, &last_text) >= MERGE_OVERLAP;
        if same {
            let gap = (now - last_seen.unwrap_or(now)).clamp(0, DWELL_CAP_S);
            conn.execute(
                "UPDATE snapshots SET last_seen_at=?1, sighting_count=sighting_count+1,
                   duration_s=duration_s+?2 WHERE id=?3",
                params![now, gap as f64, id],
            )?;
            return Ok((id, true));
        }
    }
    let id = insert_snapshot(conn, app_bundle, text, text_hash, meta)?;
    Ok((id, false))
}

pub fn insert_snapshot(
    conn: &Connection,
    app_bundle: &str,
    text: &str,
    text_hash: i64,
    meta: &SnapMeta,
) -> rusqlite::Result<i64> {
    let ts = now_secs();
    conn.execute(
        "INSERT INTO snapshots
           (ts, app_bundle, text, text_hash, window_title, url, domain,
            focused_name, focused_role, focused_path, last_seen_at, sighting_count, duration_s)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?1, 1, 0)",
        params![
            ts,
            app_bundle,
            text,
            text_hash,
            meta.window_title,
            meta.url,
            meta.domain,
            meta.focused_name,
            meta.focused_role,
            meta.focused_path,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Total number of stored snapshots (for debug visibility).
pub fn count(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("SELECT COUNT(*) FROM snapshots", [], |r| r.get(0))
}

/// One recent snapshot, for the cross-app handoff.
pub struct Recent {
    pub app_bundle: String,
    pub text: String,
    pub age_secs: i64,
}

/// A snapshot fetched by id (for relevance-ranked retrieval → working memory).
pub struct StoredSnapshot {
    pub id: i64,
    pub app_bundle: String,
    pub ts: i64,
    pub text: String,
}

/// Latest snapshot per OTHER app (newest app first), no older than `max_age_secs`,
/// up to `limit` apps. Dumb recency — no decay, no scoring, no dwell. SQLite returns
/// the `text` from the MAX(ts) row of each group (documented bare-column behaviour).
pub fn recent_snapshots(
    conn: &Connection,
    exclude_bundle: &str,
    limit: usize,
    max_age_secs: i64,
) -> rusqlite::Result<Vec<Recent>> {
    let now = now_secs();
    let min_ts = now - max_age_secs;
    let mut stmt = conn.prepare(
        "SELECT app_bundle, text, MAX(ts) AS mts FROM snapshots
         WHERE app_bundle <> ?1 AND ts >= ?2
         GROUP BY app_bundle
         ORDER BY mts DESC
         LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![exclude_bundle, min_ts, limit as i64], |r| {
        Ok(Recent {
            app_bundle: r.get(0)?,
            text: r.get(1)?,
            age_secs: now - r.get::<_, i64>(2)?,
        })
    })?;
    rows.collect()
}

/// `(ts, app_bundle)` for every snapshot since `since_ts`, oldest first.
/// Feeds the chronicle rollup (Phase 2.6). NULL bundles become "".
pub fn snapshot_rows_since(
    conn: &Connection,
    since_ts: i64,
) -> rusqlite::Result<Vec<(i64, String)>> {
    let mut stmt =
        conn.prepare("SELECT ts, app_bundle FROM snapshots WHERE ts >= ?1 ORDER BY ts ASC")?;
    let rows = stmt.query_map(params![since_ts], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, Option<String>>(1)?.unwrap_or_default(),
        ))
    })?;
    rows.collect()
}

/// Highest snapshot id (0 if empty) — used to initialize the cognee sync cursor to "now".
pub fn max_snapshot_id(conn: &Connection) -> i64 {
    conn.query_row("SELECT COALESCE(MAX(id), 0) FROM snapshots", [], |r| r.get(0))
        .unwrap_or(0)
}

/// Snapshots strictly after `after_id` (ascending, up to `limit`) — the cognee sync-cursor lane.
pub fn snapshots_after(
    conn: &Connection,
    after_id: i64,
    limit: usize,
) -> rusqlite::Result<Vec<StoredSnapshot>> {
    let mut stmt = conn.prepare(
        "SELECT id, app_bundle, ts, text FROM snapshots WHERE id > ?1 ORDER BY id ASC LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![after_id, limit as i64], |r| {
        Ok(StoredSnapshot {
            id: r.get(0)?,
            app_bundle: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            ts: r.get(2)?,
            text: r.get(3)?,
        })
    })?;
    rows.collect()
}

/// One capture for the Phase-2 activity-digest lane (id + surface + domain + text).
pub struct ActivityRow {
    pub id: i64,
    pub app_bundle: String,
    pub domain: Option<String>,
    pub text: String,
}

/// Snapshots after `after_id`, WITH domain — for the digest lane's per-surface grouping.
pub fn activity_after(
    conn: &Connection,
    after_id: i64,
    limit: usize,
) -> rusqlite::Result<Vec<ActivityRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, app_bundle, domain, text FROM snapshots WHERE id > ?1 ORDER BY id ASC LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![after_id, limit as i64], |r| {
        Ok(ActivityRow {
            id: r.get(0)?,
            app_bundle: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            domain: r.get::<_, Option<String>>(2)?,
            text: r.get(3)?,
        })
    })?;
    rows.collect()
}

/// Fetch snapshots by id (order not guaranteed — the caller re-orders as needed).
pub fn snapshots_by_ids(conn: &Connection, ids: &[i64]) -> rusqlite::Result<Vec<StoredSnapshot>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!("SELECT id, app_bundle, ts, text FROM snapshots WHERE id IN ({placeholders})");
    let mut stmt = conn.prepare(&sql)?;
    let bound: Vec<&dyn rusqlite::ToSql> = ids.iter().map(|i| i as &dyn rusqlite::ToSql).collect();
    let rows = stmt.query_map(bound.as_slice(), |r| {
        Ok(StoredSnapshot {
            id: r.get(0)?,
            app_bundle: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            ts: r.get(2)?,
            text: r.get(3)?,
        })
    })?;
    rows.collect()
}

// ── Working memory (the inspectable "now") ───────────────────────────────────
// What the most recent trigger's recall pulled in; read by the get_working_memory IPC.

/// Replace the working-memory set with the given (snapshot_id, relevance) pairs, atomically.
/// Populated again by the local FTS recall lane (P2) — cognee returns facts without snapshot ids,
/// so local hits restore the snapshot-grounded provenance the panel's inspect-now IPC shows.
pub fn set_working_memory(conn: &Connection, items: &[(i64, f32)]) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute("DELETE FROM working_memory", [])?;
    let now = now_secs();
    for (id, rel) in items {
        tx.execute(
            "INSERT INTO working_memory (snapshot_id, added_at, relevance) VALUES (?1, ?2, ?3)
             ON CONFLICT(snapshot_id) DO UPDATE SET added_at = excluded.added_at,
                relevance = excluded.relevance",
            params![id, now, *rel as f64],
        )?;
    }
    tx.commit()
}

/// The current working memory, most-relevant first.
pub fn working_memory(conn: &Connection, limit: usize) -> rusqlite::Result<Vec<(i64, f64)>> {
    let mut stmt = conn.prepare(
        "SELECT snapshot_id, relevance FROM working_memory ORDER BY relevance DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
    rows.collect()
}

/// Drop working-memory rows whose snapshot no longer exists (after retention pruning).
pub fn prune_orphans(conn: &Connection) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM working_memory WHERE snapshot_id NOT IN (SELECT id FROM snapshots)",
        [],
    )
}

/// Danger zone: wipe every LOCAL memory table (snapshots cascade into the FTS index via its
/// triggers). Settings survive — identity/exclusions/name are configuration, not memory.
pub fn wipe_memory(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "DELETE FROM snapshots;
         DELETE FROM events;
         DELETE FROM wiki_pages;
         DELETE FROM style_samples;
         DELETE FROM style_notes;
         DELETE FROM working_memory;
         DELETE FROM entities;
         DELETE FROM ties;",
    )
}

// ── Inbox events: quill's activity feed (drafts delivered, wiki batches, system notices) ──────

#[derive(Debug, Clone, serde::Serialize)]
pub struct EventRow {
    pub id: i64,
    pub ts: i64,
    pub kind: String, // draft | wiki | system | chat
    pub title: String,
    pub body: String,
    pub read: bool,
}

/// Log an inbox event (best-effort — the feed must never break a hot path).
pub fn insert_event(kind: &str, title: &str, body: &str) {
    if let Some(conn) = open_default() {
        let _ = conn.execute(
            "INSERT INTO events (ts, kind, title, body) VALUES (?1, ?2, ?3, ?4)",
            params![now_secs(), kind, title, body],
        );
    }
}

pub fn list_events(conn: &Connection, limit: usize) -> rusqlite::Result<Vec<EventRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, ts, kind, title, body, read FROM events ORDER BY id DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit as i64], |r| {
        Ok(EventRow {
            id: r.get(0)?,
            ts: r.get(1)?,
            kind: r.get(2)?,
            title: r.get(3)?,
            body: r.get(4)?,
            read: r.get::<_, i64>(5)? != 0,
        })
    })?;
    rows.collect()
}

pub fn unread_events(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM events WHERE read=0", [], |r| r.get(0)).unwrap_or(0)
}

pub fn mark_event_read(conn: &Connection, id: i64) -> rusqlite::Result<()> {
    conn.execute("UPDATE events SET read=1 WHERE id=?1", params![id])?;
    Ok(())
}

// ── Identity/voice proposals: ambient changes wait here for the user to approve (review-diff) ──

#[derive(serde::Serialize)]
pub struct ProposalRow {
    pub id: i64,
    pub kind: String, // "voice" | "identity"
    pub key: String,  // profile key for voice; "" for global identity
    pub before: String,
    pub after: String,
    pub created_at: i64,
}

fn read_proposal(r: &rusqlite::Row) -> rusqlite::Result<ProposalRow> {
    Ok(ProposalRow {
        id: r.get(0)?,
        kind: r.get(1)?,
        key: r.get(2)?,
        before: r.get(3)?,
        after: r.get(4)?,
        created_at: r.get(5)?,
    })
}

const PROPOSAL_COLS: &str = "id, kind, prof_key, before_val, after_val, created_at";

pub fn insert_proposal(
    conn: &Connection,
    kind: &str,
    key: &str,
    before: &str,
    after: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO identity_proposals (kind, prof_key, before_val, after_val, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![kind, key, before, after, now_secs()],
    )?;
    Ok(())
}

pub fn list_proposals(conn: &Connection) -> rusqlite::Result<Vec<ProposalRow>> {
    let mut stmt = conn
        .prepare(&format!("SELECT {PROPOSAL_COLS} FROM identity_proposals ORDER BY created_at DESC"))?;
    let rows = stmt.query_map([], read_proposal)?;
    rows.collect()
}

pub fn get_proposal(conn: &Connection, id: i64) -> Option<ProposalRow> {
    conn.query_row(
        &format!("SELECT {PROPOSAL_COLS} FROM identity_proposals WHERE id = ?1"),
        params![id],
        read_proposal,
    )
    .ok()
}

pub fn delete_proposal(conn: &Connection, id: i64) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM identity_proposals WHERE id = ?1", params![id])?;
    Ok(())
}

/// A pending proposal already exists for this kind+key — so an ambient re-learn doesn't pile up a
/// new duplicate every idle tick while one is awaiting review.
pub fn has_proposal(conn: &Connection, kind: &str, key: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM identity_proposals WHERE kind = ?1 AND prof_key = ?2 LIMIT 1",
        params![kind, key],
        |_| Ok(()),
    )
    .is_ok()
}

/// Coarse activity pattern for the identity dossier's "Habits & patterns": over the last `days`,
/// count snapshots per (time-of-day bucket × app). The caller turns the busiest rows into a
/// "when you do what" summary. Buckets use the machine's LOCAL hour.
pub fn activity_by_hour_app(
    conn: &Connection,
    days: i64,
) -> rusqlite::Result<Vec<(String, String, i64)>> {
    let since = now_secs() - days * 86400;
    let mut stmt = conn.prepare(
        "SELECT
            CASE
              WHEN CAST(strftime('%H', ts, 'unixepoch', 'localtime') AS INTEGER) BETWEEN 5 AND 11 THEN 'morning'
              WHEN CAST(strftime('%H', ts, 'unixepoch', 'localtime') AS INTEGER) BETWEEN 12 AND 17 THEN 'afternoon'
              WHEN CAST(strftime('%H', ts, 'unixepoch', 'localtime') AS INTEGER) BETWEEN 18 AND 22 THEN 'evening'
              ELSE 'night'
            END AS bucket,
            app_bundle,
            COUNT(*) AS n
         FROM snapshots
         WHERE ts >= ?1 AND app_bundle IS NOT NULL AND app_bundle <> ''
         GROUP BY bucket, app_bundle
         HAVING n >= 3
         ORDER BY n DESC",
    )?;
    let rows = stmt.query_map(params![since], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
    })?;
    rows.collect()
}

pub fn mark_all_events_read(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute("UPDATE events SET read=1", [])?;
    Ok(())
}

// ── Wiki pages: per-entity distilled memory ──────────────────────────────────

/// A distilled per-entity page: a summary of everything memory knows about one entity, grounded
/// in the local snapshots that mention it. The browsable, human unit of memory.
#[derive(Debug, Clone, serde::Serialize)]
pub struct WikiRow {
    pub slug: String,
    pub title: String,
    pub kind: String,
    pub aliases: Vec<String>,
    pub summary: String,
    pub mention_count: i64,
    pub snapshot_ids: Vec<i64>,
    pub first_seen: Option<i64>,
    pub last_seen: Option<i64>,
    pub updated_at: i64,
}

pub fn upsert_wiki_page(conn: &Connection, p: &WikiRow) -> rusqlite::Result<()> {
    let aliases = serde_json::to_string(&p.aliases).unwrap_or_else(|_| "[]".into());
    let snaps = serde_json::to_string(&p.snapshot_ids).unwrap_or_else(|_| "[]".into());
    conn.execute(
        "INSERT INTO wiki_pages
           (slug, title, kind, aliases, summary, mention_count, snapshot_ids, first_seen, last_seen, updated_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
         ON CONFLICT(slug) DO UPDATE SET
           title=?2, kind=?3, aliases=?4, summary=?5, mention_count=?6,
           snapshot_ids=?7, first_seen=?8, last_seen=?9, updated_at=?10",
        params![p.slug, p.title, p.kind, aliases, p.summary, p.mention_count, snaps,
                p.first_seen, p.last_seen, p.updated_at],
    )?;
    Ok(())
}

fn row_to_wiki(r: &rusqlite::Row) -> rusqlite::Result<WikiRow> {
    let aliases: String = r.get(3)?;
    let snaps: String = r.get(6)?;
    Ok(WikiRow {
        slug: r.get(0)?,
        title: r.get(1)?,
        kind: r.get(2)?,
        aliases: serde_json::from_str(&aliases).unwrap_or_default(),
        summary: r.get(4)?,
        mention_count: r.get(5)?,
        snapshot_ids: serde_json::from_str(&snaps).unwrap_or_default(),
        first_seen: r.get(7)?,
        last_seen: r.get(8)?,
        updated_at: r.get(9)?,
    })
}

const WIKI_COLS: &str =
    "slug, title, kind, aliases, summary, mention_count, snapshot_ids, first_seen, last_seen, updated_at";

pub fn get_wiki_page(conn: &Connection, slug: &str) -> Option<WikiRow> {
    conn.query_row(
        &format!("SELECT {WIKI_COLS} FROM wiki_pages WHERE slug=?1"),
        params![slug],
        |r| row_to_wiki(r),
    )
    .ok()
}

/// Pages most-recently distilled first (the wiki browse list).
pub fn list_wiki_pages(conn: &Connection, limit: usize) -> rusqlite::Result<Vec<WikiRow>> {
    let mut stmt =
        conn.prepare(&format!("SELECT {WIKI_COLS} FROM wiki_pages ORDER BY updated_at DESC LIMIT ?1"))?;
    let rows = stmt.query_map(params![limit as i64], |r| row_to_wiki(r))?;
    rows.collect()
}

/// MIN/MAX ts across a set of snapshot ids — the "first seen / last seen" for a wiki page.
pub fn snapshot_time_bounds(conn: &Connection, ids: &[i64]) -> (Option<i64>, Option<i64>) {
    if ids.is_empty() {
        return (None, None);
    }
    let ph = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    conn.query_row(
        &format!("SELECT MIN(ts), MAX(ts) FROM snapshots WHERE id IN ({ph})"),
        rusqlite::params_from_iter(ids),
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .unwrap_or((None, None))
}

/// Slugs whose page was refreshed within `max_age_secs` — skip re-distilling these (converge over time).
/// Find distilled wiki pages relevant to a question — the fast, local "summarized memory" lane
/// for the chat/ask flow. Matches the question's word-tokens against title/aliases/summary,
/// ranked by how many tokens hit and mention_count. Returns (title, summary).
pub fn search_wiki_pages(conn: &Connection, query: &str, limit: usize) -> Vec<(String, String)> {
    let tokens: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.chars().count() >= 3)
        .map(|t| t.to_lowercase())
        .take(12)
        .collect();
    if tokens.is_empty() {
        return Vec::new();
    }
    // Pull candidate pages once; score in Rust (small table — hundreds of rows).
    let mut stmt = match conn
        .prepare("SELECT title, aliases, summary, mention_count FROM wiki_pages WHERE summary != ''")
    {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let rows = match stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)?,
        ))
    }) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let mut scored: Vec<(i64, String, String)> = Vec::new();
    for (title, aliases, summary, mentions) in rows.flatten() {
        let hay = format!("{title} {aliases} {summary}").to_lowercase();
        let hits = tokens.iter().filter(|t| hay.contains(*t)).count() as i64;
        if hits > 0 {
            scored.push((hits * 100 + mentions, title, summary));
        }
    }
    scored.sort_by_key(|(s, _, _)| std::cmp::Reverse(*s));
    scored.truncate(limit);
    scored.into_iter().map(|(_, t, s)| (t, s)).collect()
}

pub fn fresh_wiki_slugs(conn: &Connection, max_age_secs: i64) -> std::collections::HashSet<String> {
    let cutoff = now_secs() - max_age_secs;
    conn.prepare("SELECT slug FROM wiki_pages WHERE updated_at >= ?1")
        .and_then(|mut s| {
            s.query_map(params![cutoff], |r| r.get::<_, String>(0))
                .map(|rows| rows.flatten().collect())
        })
        .unwrap_or_default()
}

// ── Style learning: per-surface tone ─────────────────────────────────────────

/// One-time re-key for profiles (E1): style rows were keyed by app BUNDLE
/// ("com.microsoft.teams2"); profiles key by cognee's dataset form ("app-teams2", domain-aware).
/// Rename any dotted (bundle) surface to its `dataset_for(_, None)` form so native-app voice
/// (Teams/Outlook) survives the switch. Idempotent — dataset-form keys contain no dots.
pub fn migrate_style_surface_keys(conn: &Connection) -> rusqlite::Result<usize> {
    let mut surfaces = std::collections::HashSet::new();
    for tbl in ["style_samples", "style_notes"] {
        let mut stmt = conn.prepare(&format!("SELECT DISTINCT surface FROM {tbl}"))?;
        for s in stmt.query_map([], |r| r.get::<_, String>(0))?.flatten() {
            surfaces.insert(s);
        }
    }
    let mut n = 0;
    for old in surfaces {
        if !old.contains('.') {
            continue; // already dataset-form
        }
        let new = crate::cognee::dataset_for(&old, None);
        if new == old {
            continue;
        }
        conn.execute("UPDATE style_samples SET surface=?1 WHERE surface=?2", params![new, old])?;
        conn.execute("UPDATE style_notes SET surface=?1 WHERE surface=?2", params![new, old])?;
        n += 1;
    }
    Ok(n)
}

/// Fold per-surface voice + persona settings onto CANONICAL platform keys (`profile::canonical`), so
/// app and web variants of a platform share one profile — the voice learned in Teams-desktop
/// ("app-teams2") now applies in Teams-web ("domain-teams-microsoft-com") and vice-versa. Idempotent;
/// when both variants exist the style rows simply combine (a later re-learn dedups any bullets) and
/// settings keep whichever canonical value is already present.
pub fn migrate_canonical_profile_keys(conn: &Connection) -> rusqlite::Result<usize> {
    let mut n = 0;

    // Voice: re-key the style tables' `surface` column onto the canonical platform key.
    for tbl in ["style_samples", "style_notes"] {
        let mut stmt = conn.prepare(&format!("SELECT DISTINCT surface FROM {tbl}"))?;
        let surfaces: Vec<String> =
            stmt.query_map([], |r| r.get::<_, String>(0))?.filter_map(|r| r.ok()).collect();
        drop(stmt);
        for old in surfaces {
            let newk = crate::profile::canonical(&old);
            if newk != old {
                conn.execute(
                    &format!("UPDATE {tbl} SET surface=?1 WHERE surface=?2"),
                    params![newk, old],
                )?;
                n += 1;
            }
        }
    }

    // Persona settings: signature / circle / voice-opt-out / (legacy) per-profile identity.
    for prefix in ["signature:", "profile_circle:", "voice_optout:", "identity_profile:"] {
        let mut stmt = conn.prepare("SELECT key FROM settings WHERE key LIKE ?1")?;
        let keys: Vec<String> = stmt
            .query_map(params![format!("{prefix}%")], |r| r.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);
        for full in keys {
            let surface = &full[prefix.len()..];
            let newk = crate::profile::canonical(surface);
            if newk == surface {
                continue;
            }
            let newfull = format!("{prefix}{newk}");
            let taken = conn
                .query_row("SELECT 1 FROM settings WHERE key=?1", params![newfull], |_| Ok(()))
                .is_ok();
            if taken {
                conn.execute("DELETE FROM settings WHERE key=?1", params![full])?; // redundant variant
            } else {
                conn.execute("UPDATE settings SET key=?1 WHERE key=?2", params![newfull, full])?;
                n += 1;
            }
        }
    }
    Ok(n)
}

/// Distinct profile keys the user has actually written on (drives the Profiles panel). Ordered
/// by most-recent activity so the platforms in play float to the top.
pub fn distinct_style_surfaces(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT surface, MAX(created_at) m FROM style_samples GROUP BY surface ORDER BY m DESC",
    )?;
    let out = stmt.query_map([], |r| r.get::<_, String>(0))?.flatten().collect();
    Ok(out)
}

/// Record a sample of the user's own writing for a surface (profile key).
pub fn insert_style_sample(conn: &Connection, surface: &str, text: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO style_samples (surface, text, created_at) VALUES (?1, ?2, ?3)",
        params![surface, text, now_secs()],
    )?;
    Ok(())
}

/// How many writing samples we have for a surface.
pub fn style_sample_count(conn: &Connection, surface: &str) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM style_samples WHERE surface = ?1",
        params![surface],
        |r| r.get(0),
    )
}

/// The most recent writing samples for a surface (newest first).
pub fn recent_style_samples(
    conn: &Connection,
    surface: &str,
    limit: usize,
) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT text FROM style_samples WHERE surface = ?1 ORDER BY created_at DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![surface, limit as i64], |r| r.get::<_, String>(0))?;
    rows.collect()
}

/// Replace the learned style bullets for a surface (delete old, insert new).
pub fn replace_style_notes(
    conn: &Connection,
    surface: &str,
    bullets: &[String],
) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM style_notes WHERE surface = ?1", params![surface])?;
    let now = now_secs();
    for b in bullets {
        conn.execute(
            "INSERT INTO style_notes (surface, bullet, updated_at) VALUES (?1, ?2, ?3)",
            params![surface, b, now],
        )?;
    }
    Ok(())
}

/// The learned style bullets for a surface (empty if none yet).
pub fn style_notes_for(conn: &Connection, surface: &str) -> rusqlite::Result<Vec<String>> {
    let mut stmt =
        conn.prepare("SELECT bullet FROM style_notes WHERE surface = ?1 ORDER BY note_id")?;
    let rows = stmt.query_map(params![surface], |r| r.get::<_, String>(0))?;
    rows.collect()
}

/// Keep only the newest `keep` style samples for a surface; delete the rest. Bounds growth
/// (style_samples gets a row per rewrite/edit and was otherwise never pruned).
pub fn prune_style_samples(conn: &Connection, surface: &str, keep: i64) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM style_samples WHERE surface = ?1 AND id NOT IN (
            SELECT id FROM style_samples WHERE surface = ?1 ORDER BY created_at DESC, id DESC LIMIT ?2
         )",
        params![surface, keep],
    )
}

// ── Settings key/value (Phase 2.3) ───────────────────────────────────────────

/// Read a setting value, or None if unset.
pub fn get_setting(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row(
        "SELECT value FROM settings WHERE key = ?1",
        params![key],
        |r| r.get::<_, String>(0),
    )
    .ok()
}

/// Insert or update a setting value.
pub fn set_setting(conn: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO settings (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

// ── Retention / pruning (Phase 2.4) ──────────────────────────────────────────
// Keep local memory bounded so quill.db can't grow without limit. Two policies,
// applied together: drop anything older than a max age, then cap total row count.
// These only ever remove OLD / EXCESS snapshots — never reset the store.

/// Delete snapshots older than `max_age_secs`. Returns the number removed.
pub fn prune_older_than(conn: &Connection, max_age_secs: i64) -> rusqlite::Result<usize> {
    let cutoff = now_secs() - max_age_secs;
    conn.execute("DELETE FROM snapshots WHERE ts < ?1", params![cutoff])
}

/// Keep only the newest `max_rows` snapshots; delete the rest. Returns the number
/// removed. Newest = highest ts, ties broken by id (insertion order).
pub fn prune_to_cap(conn: &Connection, max_rows: i64) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM snapshots WHERE id NOT IN (
            SELECT id FROM snapshots ORDER BY ts DESC, id DESC LIMIT ?1
         )",
        params![max_rows],
    )
}

/// Apply both retention policies (age first, then row cap). Returns total removed.
pub fn enforce_retention(
    conn: &Connection,
    max_age_secs: i64,
    max_rows: i64,
) -> rusqlite::Result<usize> {
    let by_age = prune_older_than(conn, max_age_secs)?;
    let by_cap = prune_to_cap(conn, max_rows)?;
    // Keep the vector store / working memory from pointing at deleted snapshots.
    let _ = prune_orphans(conn);
    Ok(by_age + by_cap)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        migrate_snapshots(&conn).unwrap();
        migrate_fts(&conn).unwrap();
        conn
    }

    #[test]
    fn fts_recall_finds_relevant_local_snapshots() {
        let conn = mem();
        let m = SnapMeta { window_title: Some("Chat"), ..Default::default() };
        insert_snapshot(&conn, "app-teams2", "we decided to migrate the search index next sprint", 1, &m).unwrap();
        insert_snapshot(&conn, "app-teams2", "lunch plans for friday at the new place", 2, &m).unwrap();
        insert_snapshot(&conn, "app-outlook", "the search index budget approval is pending finance", 3, &m).unwrap();
        // Query hits the two search-index rows; excludes the current surface (app-outlook).
        let hits = search_snapshots_fts(&conn, "migrate the search index", "app-outlook", 5).unwrap();
        assert!(!hits.is_empty(), "local FTS must find the search-index discussion");
        assert!(hits.iter().all(|h| h.app_bundle != "app-outlook"), "current surface excluded");
        assert!(hits.iter().any(|h| h.text.contains("migrate the search index")), "the relevant row surfaces");
    }

    #[test]
    fn dwell_merge_accrues_near_dups_and_inserts_new() {
        let conn = mem();
        let m = SnapMeta::default();
        let base = "alpha beta gamma delta epsilon zeta eta theta iota kappa \
                    lambda mu nu xi omicron pi rho sigma tau upsilon";
        let (id1, merged1) = merge_or_insert_snapshot(&conn, "app", base, 1, &m).unwrap();
        assert!(!merged1, "first is a fresh insert");
        // Near-duplicate (same words + one appended, overlap ≈0.95) → merges into the same row.
        let near = format!("{base} phi");
        let (id2, merged2) = merge_or_insert_snapshot(&conn, "app", &near, 2, &m).unwrap();
        assert!(merged2 && id2 == id1);
        let sc: i64 = conn
            .query_row("SELECT sighting_count FROM snapshots WHERE id=?1", [id1], |r| r.get(0))
            .unwrap();
        assert_eq!(sc, 2, "sighting accrued, not a new row");
        // Genuinely different content → a new row.
        let (id3, merged3) =
            merge_or_insert_snapshot(&conn, "app", "one two three four five six", 3, &m).unwrap();
        assert!(!merged3 && id3 != id1);
        // Same content in a DIFFERENT app → new row (per-app last-row comparison).
        let (_id4, merged4) = merge_or_insert_snapshot(&conn, "other", base, 1, &m).unwrap();
        assert!(!merged4);
    }

    #[test]
    fn open_sequence_upgrades_a_v1_db_without_failing() {
        // Reproduces the quarantine bug: SCHEMA + migrations run over a PRE-EXISTING v1 table.
        // SCHEMA must not reference a column only the migration adds (domain), or open() fails
        // and the DB is wrongly quarantined.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE snapshots (id INTEGER PRIMARY KEY, ts INTEGER NOT NULL,
             app_bundle TEXT, text TEXT NOT NULL, text_hash INTEGER);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO snapshots (ts, app_bundle, text, text_hash)
             VALUES (1, 'app-teams2', 'we will migrate the search index next sprint', 1)",
            [],
        )
        .unwrap();
        // Exactly what open() does, in order — must all succeed.
        conn.execute_batch(SCHEMA).unwrap();
        migrate_snapshots(&conn).unwrap();
        migrate_fts(&conn).unwrap();
        // Data survived and is now searchable via the backfilled local index.
        let n: i64 = conn.query_row("SELECT count(*) FROM snapshots", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
        let hits = search_snapshots_fts(&conn, "search index", "other", 5).unwrap();
        assert_eq!(hits.len(), 1, "the pre-existing row backfilled into FTS");
    }

    #[test]
    fn migrate_snapshots_adds_v2_columns_idempotently() {
        let conn = Connection::open_in_memory().unwrap();
        // An OLD (v1) snapshots table — no anchor/dwell columns.
        conn.execute_batch(
            "CREATE TABLE snapshots (id INTEGER PRIMARY KEY, ts INTEGER NOT NULL,
             app_bundle TEXT, text TEXT NOT NULL, text_hash INTEGER);",
        )
        .unwrap();
        migrate_snapshots(&conn).unwrap();
        migrate_snapshots(&conn).unwrap(); // second run must be a no-op, not an error
        let meta = SnapMeta {
            window_title: Some("Inbox"),
            url: Some("https://mail.example.com"),
            domain: Some("mail.example.com"),
            ..Default::default()
        };
        let id = insert_snapshot(&conn, "app-mail", "hello there world", 7, &meta).unwrap();
        let (dom, sc): (String, i64) = conn
            .query_row(
                "SELECT domain, sighting_count FROM snapshots WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(dom, "mail.example.com");
        assert_eq!(sc, 1);
    }

    /// Insert a snapshot with an explicit timestamp (production path uses now()).
    fn insert_at(conn: &Connection, ts: i64, app: &str, text: &str) {
        conn.execute(
            "INSERT INTO snapshots (ts, app_bundle, text, text_hash) VALUES (?1, ?2, ?3, ?4)",
            params![ts, app, text, 0i64],
        )
        .unwrap();
    }

    #[test]
    fn prune_older_than_removes_only_aged_rows() {
        let conn = mem();
        let now = now_secs();
        insert_at(&conn, now - 100, "a", "fresh");
        insert_at(&conn, now - 10_000, "a", "stale");
        let removed = prune_older_than(&conn, 3_600).unwrap();
        assert_eq!(removed, 1);
        assert_eq!(count(&conn).unwrap(), 1);
    }

    #[test]
    fn prune_to_cap_keeps_newest() {
        let conn = mem();
        let now = now_secs();
        for i in 0..5 {
            insert_at(&conn, now - (i * 10), "a", &format!("msg{i}"));
        }
        let removed = prune_to_cap(&conn, 3).unwrap();
        assert_eq!(removed, 2);
        assert_eq!(count(&conn).unwrap(), 3);
        // The three newest (smallest age offset) must survive.
        let survivors: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM snapshots WHERE ts >= ?1",
                params![now - 20],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(survivors, 3);
    }

    #[test]
    fn enforce_retention_applies_both() {
        let conn = mem();
        let now = now_secs();
        insert_at(&conn, now - 1, "a", "keep");
        insert_at(&conn, now - 2, "a", "keep");
        insert_at(&conn, now - 999_999, "a", "too old");
        // age drops 1, then cap of 1 drops 1 more of the remaining 2.
        let removed = enforce_retention(&conn, 3_600, 1).unwrap();
        assert_eq!(removed, 2);
        assert_eq!(count(&conn).unwrap(), 1);
    }

    #[test]
    fn retention_is_noop_when_under_limits() {
        let conn = mem();
        let now = now_secs();
        insert_at(&conn, now, "a", "x");
        let removed = enforce_retention(&conn, 3_600, 100).unwrap();
        assert_eq!(removed, 0);
        assert_eq!(count(&conn).unwrap(), 1);
    }

    #[test]
    fn settings_round_trip_and_upsert() {
        let conn = mem();
        assert_eq!(get_setting(&conn, "paused"), None);
        set_setting(&conn, "paused", "1").unwrap();
        assert_eq!(get_setting(&conn, "paused").as_deref(), Some("1"));
        // upsert overwrites, doesn't duplicate
        set_setting(&conn, "paused", "0").unwrap();
        assert_eq!(get_setting(&conn, "paused").as_deref(), Some("0"));
    }

    #[test]
    fn prepare_quarantines_a_corrupt_db_and_recreates() {
        let dir = std::env::temp_dir().join(format!("quill-db-test-{}", now_secs()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("quill.db");
        // A non-SQLite file simulates corruption / a torn write.
        std::fs::write(&path, b"this is not a sqlite database").unwrap();

        prepare(&path); // should detect it's bad and move it aside

        // A fresh, healthy DB now opens cleanly with an empty snapshots table.
        let conn = open(&path).unwrap();
        assert_eq!(count(&conn).unwrap(), 0);

        // The bad file was quarantined, not deleted.
        let quarantined = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().contains("corrupt"));
        assert!(quarantined, "expected a .corrupt-* file to exist");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn snapshot_rows_since_filters_and_orders() {
        let conn = mem();
        insert_at(&conn, 100, "a", "old");
        insert_at(&conn, 300, "b", "newest");
        insert_at(&conn, 200, "a", "middle");
        let rows = snapshot_rows_since(&conn, 150).unwrap();
        // 100 excluded; remaining returned ascending by ts.
        assert_eq!(rows, vec![(200, "a".to_string()), (300, "b".to_string())]);
    }

    #[test]
    fn working_memory_replaces_and_orders_by_relevance() {
        let conn = mem();
        insert_at(&conn, 1, "a", "x");
        insert_at(&conn, 2, "a", "y");
        let ids: Vec<i64> = {
            let mut s = conn.prepare("SELECT id FROM snapshots ORDER BY ts").unwrap();
            s.query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap()
        };
        set_working_memory(&conn, &[(ids[0], 0.2), (ids[1], 0.9)]).unwrap();
        let wm = working_memory(&conn, 10).unwrap();
        assert_eq!(wm[0].0, ids[1], "highest relevance first");
        // replace wholesale
        set_working_memory(&conn, &[(ids[0], 0.5)]).unwrap();
        let wm = working_memory(&conn, 10).unwrap();
        assert_eq!(wm.len(), 1);
        assert_eq!(wm[0].0, ids[0]);
    }

    #[test]
    fn prune_orphans_drops_vectors_for_deleted_snapshots() {
        let conn = mem();
        insert_at(&conn, 1, "a", "x");
        let id: i64 = conn
            .query_row("SELECT id FROM snapshots", [], |r| r.get(0))
            .unwrap();
        set_working_memory(&conn, &[(id, 1.0)]).unwrap();
        conn.execute("DELETE FROM snapshots", []).unwrap();
        let removed = prune_orphans(&conn).unwrap();
        assert_eq!(removed, 1); // the orphaned working_memory row
        assert!(working_memory(&conn, 10).unwrap().is_empty());
    }

    #[test]
    fn snapshots_by_ids_fetches_requested() {
        let conn = mem();
        insert_at(&conn, 1, "a", "alpha");
        insert_at(&conn, 2, "b", "beta");
        insert_at(&conn, 3, "c", "gamma");
        let ids: Vec<i64> = {
            let mut s = conn.prepare("SELECT id FROM snapshots ORDER BY ts").unwrap();
            s.query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap()
        };
        let got = snapshots_by_ids(&conn, &[ids[0], ids[2]]).unwrap();
        let texts: std::collections::HashSet<String> =
            got.into_iter().map(|s| s.text).collect();
        assert!(texts.contains("alpha"));
        assert!(texts.contains("gamma"));
        assert!(!texts.contains("beta"));
    }
}
