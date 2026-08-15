//! Session full-text search: an FTS5 sidecar index over the on-disk event
//! logs. Lives in gasket-host behind Cargo feature `session-index`; the
//! gateway REST route and the desktop Tauri command are the two consumers.
//!
//! One SQLite database at `<config_dir>/index.db`. Every text-bearing
//! SessionEvent becomes one row; a per-session high-water mark in `meta`
//! keeps reindexing incremental. Built lazily on demand — no background
//! thread, write path untouched.

use std::path::Path;

use rusqlite::Connection;

/// Shared hit shape for both consumers (gateway REST JSON and the desktop
/// Tauri command serialize this identically). `name` comes from the
/// session's `meta.json` sidecar; `snippet` is the FTS5 snippet.
#[derive(Debug, serde::Serialize)]
pub struct SessionHit {
    pub session_id: String,
    pub name: Option<String>,
    pub snippet: String,
}

#[derive(Debug, Default, PartialEq)]
pub struct Stats {
    /// Sessions that had new rows inserted this run.
    pub sessions: usize,
    pub events_indexed: usize,
}

/// Open (creating if needed) the sidecar index and ensure the schema.
pub fn init_db(db_path: &Path) -> anyhow::Result<Connection> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(db_path)?;
    conn.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS events USING fts5(\
             session_id UNINDEXED, seq UNINDEXED, kind UNINDEXED, text);\
         CREATE TABLE IF NOT EXISTS meta(\
             key TEXT PRIMARY KEY, value INTEGER NOT NULL);",
    )?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_db_creates_fts5_and_meta_tables() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("index.db");
        let conn = init_db(&db).unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('events', 'meta')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 2, "FTS5 table and meta table must both exist");
        assert!(init_db(&db).is_ok(), "second open is idempotent");
    }
}
