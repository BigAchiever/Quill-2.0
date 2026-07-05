// Phase 3 (foundation): the entity-graph "memory" layer — structured People / Project
// / Tool / Org / Topic nodes plus typed ties, the substrate graph-augmented retrieval
// reads from (§12). This module owns the schema CRUD + entity resolution (name
// normalization / dedup — scar-map #14).
//
// NOT YET WIRED: the LLM-driven extraction that turns raw snapshots into entities.
// That step needs the live model + human eval of quality, so it's left for a
// supervised session. The storage/dedup below is pure and fully unit-tested.

// The write-side API (upsert_entity / add_tie / set_description / normalize_name) is
// the Phase-3 storage layer, intentionally ahead of the not-yet-wired LLM extractor
// that will call it. list_entities is already reachable via the `list_memory` command.
#![allow(dead_code)]

use rusqlite::{params, Connection, OptionalExtension};

/// A memory-graph node.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Entity {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub observation_count: i64,
    pub last_seen: i64,
    pub description: Option<String>,
}

/// Canonical form for dedup: trim, collapse internal whitespace, lowercase. So
/// "Y  Combinator", "y combinator" and " Y Combinator " resolve to one node.
pub fn normalize_name(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Insert a newly-seen entity, or bump an existing one matched by (normalized name,
/// kind): increments its observation count and refreshes `last_seen`. Returns the id.
pub fn upsert_entity(conn: &Connection, name: &str, kind: &str, ts: i64) -> rusqlite::Result<i64> {
    let norm = normalize_name(name);
    let existing: Option<i64> = conn
        .query_row(
            "SELECT id FROM entities WHERE norm_name = ?1 AND kind = ?2",
            params![norm, kind],
            |r| r.get(0),
        )
        .optional()?;
    match existing {
        Some(id) => {
            conn.execute(
                "UPDATE entities SET observation_count = observation_count + 1, last_seen = ?2
                 WHERE id = ?1",
                params![id, ts],
            )?;
            Ok(id)
        }
        None => {
            conn.execute(
                "INSERT INTO entities (name, norm_name, kind, observation_count, first_seen, last_seen)
                 VALUES (?1, ?2, ?3, 1, ?4, ?4)",
                params![name, norm, kind, ts],
            )?;
            Ok(conn.last_insert_rowid())
        }
    }
}

/// Set / replace an entity's description (the short, date-stamped LLM summary).
pub fn set_description(conn: &Connection, id: i64, description: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE entities SET description = ?2 WHERE id = ?1",
        params![id, description],
    )?;
    Ok(())
}

/// Record or strengthen a typed relationship (tie) between two entities.
pub fn add_tie(conn: &Connection, src: i64, dst: i64, kind: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO ties (src, dst, kind, count) VALUES (?1, ?2, ?3, 1)
         ON CONFLICT(src, dst, kind) DO UPDATE SET count = count + 1",
        params![src, dst, kind],
    )?;
    Ok(())
}

/// Entities, most-observed first (the "Memory" tab list).
pub fn list_entities(conn: &Connection, limit: usize) -> rusqlite::Result<Vec<Entity>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, kind, observation_count, last_seen, description
         FROM entities ORDER BY observation_count DESC, last_seen DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit as i64], |r| {
        Ok(Entity {
            id: r.get(0)?,
            name: r.get(1)?,
            kind: r.get(2)?,
            observation_count: r.get(3)?,
            last_seen: r.get(4)?,
            description: r.get(5)?,
        })
    })?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::db::SCHEMA).unwrap();
        conn
    }

    #[test]
    fn normalize_collapses_case_and_whitespace() {
        assert_eq!(normalize_name("  Y   Combinator "), "y combinator");
        assert_eq!(normalize_name("DOCKER"), "docker");
        assert_eq!(normalize_name("Jordan"), "jordan");
    }

    #[test]
    fn upsert_dedups_by_normalized_name_and_kind() {
        let conn = mem();
        let a = upsert_entity(&conn, "Y Combinator", "Org", 100).unwrap();
        let b = upsert_entity(&conn, "y  combinator", "Org", 200).unwrap();
        assert_eq!(a, b, "case/space variants resolve to one node");

        let (count, last): (i64, i64) = conn
            .query_row(
                "SELECT observation_count, last_seen FROM entities WHERE id = ?1",
                params![a],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(count, 2);
        assert_eq!(last, 200, "last_seen refreshed to the newer observation");
    }

    #[test]
    fn same_name_different_kind_is_distinct() {
        let conn = mem();
        let org = upsert_entity(&conn, "Comet", "Org", 1).unwrap();
        let tool = upsert_entity(&conn, "Comet", "Tool", 1).unwrap();
        assert_ne!(org, tool);
    }

    #[test]
    fn add_tie_is_idempotent_on_count() {
        let conn = mem();
        let a = upsert_entity(&conn, "Jordan", "Person", 1).unwrap();
        let b = upsert_entity(&conn, "Quill", "Project", 1).unwrap();
        add_tie(&conn, a, b, "works_on").unwrap();
        add_tie(&conn, a, b, "works_on").unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT count FROM ties WHERE src = ?1 AND dst = ?2 AND kind = 'works_on'",
                params![a, b],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 2, "repeated tie strengthens, not duplicates");
    }

    #[test]
    fn list_entities_orders_by_observation_count() {
        let conn = mem();
        upsert_entity(&conn, "rare", "Topic", 1).unwrap();
        let common = upsert_entity(&conn, "common", "Topic", 1).unwrap();
        upsert_entity(&conn, "common", "Topic", 2).unwrap();
        upsert_entity(&conn, "common", "Topic", 3).unwrap();
        let list = list_entities(&conn, 10).unwrap();
        assert_eq!(list[0].id, common);
        assert_eq!(list[0].observation_count, 3);
    }

    #[test]
    fn set_description_persists() {
        let conn = mem();
        let id = upsert_entity(&conn, "Docker", "Tool", 1).unwrap();
        set_description(&conn, id, "Containerization tool (as of 2026-06-30)").unwrap();
        let list = list_entities(&conn, 1).unwrap();
        assert_eq!(
            list[0].description.as_deref(),
            Some("Containerization tool (as of 2026-06-30)")
        );
    }
}
