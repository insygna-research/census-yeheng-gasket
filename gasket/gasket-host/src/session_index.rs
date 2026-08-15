//! Session full-text search: an FTS5 sidecar index over the on-disk event
//! logs. Lives in gasket-host behind Cargo feature `session-index`; the
//! gateway REST route and the desktop Tauri command are the two consumers.
//!
//! One SQLite database at `<config_dir>/index.db`. Every text-bearing
//! SessionEvent becomes one row; a per-session high-water mark in `meta`
//! keeps reindexing incremental. Built lazily on demand — no background
//! thread, write path untouched.

use std::path::Path;

use gasket_core::{AgentMessage, SessionEvent};
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

// Module-private by design; first consumed by reindex (Task 3 of this
// feature), which turns rows into FTS5 inserts.
#[allow(dead_code)]
struct Row {
    seq: usize,
    kind: &'static str,
    text: String,
}

#[allow(dead_code)]
fn block_text(content: &[gasket_core::ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|b| match b {
            gasket_core::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Project one session's event log into indexable rows. `seq` is the event's
/// index in the log (0-based), so the high-water mark stays monotonic even
/// though marker events produce no row.
#[allow(dead_code)]
fn event_rows(events: &[SessionEvent]) -> Vec<Row> {
    events
        .iter()
        .enumerate()
        .filter_map(|(seq, ev)| {
            let (kind, msg): (&'static str, &AgentMessage) = match ev {
                SessionEvent::User(m) => ("user", m),
                SessionEvent::Assistant { message: m, .. } => ("assistant", m),
                SessionEvent::ToolResult(m) => ("tool_result", m),
                SessionEvent::TurnStart | SessionEvent::TurnEnd { .. } => return None,
            };
            let content = match msg {
                AgentMessage::User(u) => block_text(&u.content),
                AgentMessage::Assistant(a) => block_text(&a.content),
                AgentMessage::ToolResult(t) => block_text(&t.content),
                AgentMessage::Custom(_) => return None,
            };
            if content.is_empty() {
                return None;
            }
            Some(Row {
                seq,
                kind,
                text: content,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gasket_core::types::session_event::TurnEndReason;
    use gasket_core::EventStorage;

    fn user_ev(t: &str) -> SessionEvent {
        SessionEvent::User(AgentMessage::user(t))
    }

    #[tokio::test]
    async fn event_rows_extracts_text_and_skips_markers() {
        let tmp = tempfile::tempdir().unwrap();
        let store = EventStorage::new(tmp.path());
        let events = vec![
            SessionEvent::TurnStart,
            user_ev("find the flaky test"),
            SessionEvent::TurnEnd {
                reason: TurnEndReason::Completed,
            },
        ];
        store.append_events("s1", &events).await.unwrap();
        let rows = event_rows(&store.load_events("s1").await.unwrap());
        assert_eq!(rows.len(), 1, "markers produce no rows");
        assert_eq!(rows[0].seq, 1, "seq is the log index, not the row index");
        assert_eq!(rows[0].kind, "user");
        assert!(rows[0].text.contains("flaky"));
    }

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
