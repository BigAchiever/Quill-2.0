// Phase 2.6: roll captured snapshots up into a coarse timeline ("what was I working
// on?"). Pure grouping only — LLM narrative summaries arrive in Phase 3.

use rusqlite::Connection;

/// One contiguous stretch of activity in a single app.
/// `start_ts`/`end_ts` are unix seconds; the frontend derives duration from them.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Segment {
    pub app_bundle: String,
    pub start_ts: i64,
    pub end_ts: i64,
    pub count: usize,
}

/// Group chronologically-ordered `(ts, app_bundle)` rows into segments. A new segment
/// begins when the app changes OR the gap from the previous snapshot exceeds
/// `gap_secs`. Input MUST be sorted by `ts` ascending (the DB query guarantees this).
pub fn rollup(rows: &[(i64, String)], gap_secs: i64) -> Vec<Segment> {
    let mut segments: Vec<Segment> = Vec::new();
    for (ts, app) in rows {
        match segments.last_mut() {
            Some(seg) if &seg.app_bundle == app && *ts - seg.end_ts <= gap_secs => {
                seg.end_ts = *ts;
                seg.count += 1;
            }
            _ => segments.push(Segment {
                app_bundle: app.clone(),
                start_ts: *ts,
                end_ts: *ts,
                count: 1,
            }),
        }
    }
    segments
}

/// Build the chronicle for all snapshots since `since_ts`.
pub fn build(conn: &Connection, since_ts: i64, gap_secs: i64) -> rusqlite::Result<Vec<Segment>> {
    let rows = crate::db::snapshot_rows_since(conn, since_ts)?;
    Ok(rollup(&rows, gap_secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(ts: i64, app: &str) -> (i64, String) {
        (ts, app.to_string())
    }

    #[test]
    fn groups_same_app_within_gap() {
        let segs = rollup(&[r(0, "A"), r(10, "A"), r(20, "A")], 60);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].count, 3);
        assert_eq!((segs[0].start_ts, segs[0].end_ts), (0, 20));
    }

    #[test]
    fn splits_on_app_change() {
        let segs = rollup(&[r(0, "A"), r(10, "B"), r(20, "A")], 60);
        let apps: Vec<&str> = segs.iter().map(|s| s.app_bundle.as_str()).collect();
        assert_eq!(apps, vec!["A", "B", "A"]);
    }

    #[test]
    fn splits_on_time_gap() {
        let segs = rollup(&[r(0, "A"), r(10, "A"), r(1000, "A")], 60);
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].count, 2);
        assert_eq!(segs[1].start_ts, 1000);
    }

    #[test]
    fn empty_input_yields_no_segments() {
        assert!(rollup(&[], 60).is_empty());
    }
}
