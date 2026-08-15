//! Session full-text search: an FTS5 sidecar index over the on-disk event
//! logs. Lives in gasket-host behind Cargo feature `session-index`; the
//! gateway REST route and the desktop Tauri command are the two consumers.
//!
//! One SQLite database at `<config_dir>/index.db`. Every text-bearing
//! SessionEvent becomes one row; a per-session high-water mark in `meta`
//! keeps reindexing incremental. Built lazily on demand — no background
//! thread, write path untouched.

use std::path::Path;

use gasket_core::{AgentMessage, EventStorage, SessionEvent};
use rusqlite::{Connection, OptionalExtension};

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

struct Row {
    seq: usize,
    kind: &'static str,
    text: String,
}

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

/// Incremental reindex: for every session dir under `store_root`, append
/// only events past the per-session high-water mark stored in `meta`.
pub async fn reindex(store_root: &Path, db_path: &Path) -> anyhow::Result<Stats> {
    let conn = init_db(db_path)?;
    let storage = EventStorage::new(store_root);
    let mut stats = Stats::default();
    let mut ids: Vec<String> = std::fs::read_dir(store_root)?
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|id| gasket_core::is_valid_session_id(id))
        .collect();
    ids.sort(); // deterministic order for tests and logging
    for id in ids {
        let events = storage.load_events(&id).await?;
        // No mark yet → -1, so a text event at seq 0 is indexed on first run.
        let last: i64 = conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [&id], |r| r.get(0))
            .optional()?
            .unwrap_or(-1);
        let mut inserted = 0usize;
        let mut max_seq = last;
        {
            let mut stmt = conn.prepare(
                "INSERT INTO events(session_id, seq, kind, text) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for row in event_rows(&events) {
                if (row.seq as i64) <= last {
                    continue;
                }
                stmt.execute(rusqlite::params![id, row.seq as i64, row.kind, row.text])?;
                max_seq = max_seq.max(row.seq as i64);
                inserted += 1;
            }
        }
        if inserted > 0 {
            conn.execute(
                "INSERT INTO meta(key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                rusqlite::params![id, max_seq],
            )?;
            stats.sessions += 1;
            stats.events_indexed += inserted;
        }
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gasket_core::types::message::{FunctionCall, ToolCall};
    use gasket_core::types::session_event::TurnEndReason;
    use gasket_core::{
        AssistantMessage, ContentBlock, EventStorage, StopReason, ToolResultMessage,
    };

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

    #[tokio::test]
    async fn reindex_is_incremental_across_appends() {
        let tmp = tempfile::tempdir().unwrap();
        let store = EventStorage::new(tmp.path());
        let db = tmp.path().join("index.db");
        store
            .append_events("s1", &[SessionEvent::TurnStart, user_ev("first message")])
            .await
            .unwrap();
        let first = reindex(tmp.path(), &db).await.unwrap();
        assert_eq!((first.sessions, first.events_indexed), (1, 1));

        store
            .append_events(
                "s1",
                &[
                    user_ev("second message"),
                    SessionEvent::TurnEnd {
                        reason: TurnEndReason::Completed,
                    },
                ],
            )
            .await
            .unwrap();
        let second = reindex(tmp.path(), &db).await.unwrap();
        assert_eq!(
            second.events_indexed, 1,
            "only the newly appended text event"
        );

        let conn = init_db(&db).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2, "no duplicate rows");
        let mark: i64 = conn
            .query_row("SELECT value FROM meta WHERE key = 's1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mark, 2, "high-water mark is the max indexed seq (0-based)");
    }

    #[tokio::test]
    async fn reindex_on_empty_root_is_zero_stats() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            reindex(tmp.path(), &tmp.path().join("index.db"))
                .await
                .unwrap(),
            Stats::default()
        );
    }

    #[tokio::test]
    async fn reindex_indexes_tool_result_events() {
        let tmp = tempfile::tempdir().unwrap();
        let store = EventStorage::new(tmp.path());
        let db = tmp.path().join("index.db");
        store
            .append_events(
                "s1",
                &[SessionEvent::ToolResult(AgentMessage::ToolResult(
                    ToolResultMessage {
                        tool_call_id: "t1".into(),
                        tool_name: "bash".into(),
                        content: vec![ContentBlock::text("rg found nothing")],
                        is_error: false,
                        timestamp: 0,
                    },
                ))],
            )
            .await
            .unwrap();
        let stats = reindex(tmp.path(), &db).await.unwrap();
        assert_eq!((stats.sessions, stats.events_indexed), (1, 1));
        let conn = init_db(&db).unwrap();
        let text: String = conn
            .query_row(
                "SELECT text FROM events WHERE kind = 'tool_result'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(text, "rg found nothing");
    }

    #[tokio::test]
    async fn reindex_skips_tool_call_only_assistant_events() {
        let tmp = tempfile::tempdir().unwrap();
        let store = EventStorage::new(tmp.path());
        let db = tmp.path().join("index.db");
        let a = AssistantMessage {
            content: vec![ContentBlock::ToolCall {
                tool_call: ToolCall {
                    id: "t1".into(),
                    function: FunctionCall {
                        name: "bash".into(),
                        arguments: "{}".into(),
                    },
                },
            }],
            model: String::new(),
            stop_reason: StopReason::ToolUse,
            usage: None,
            timestamp: 0,
        };
        store
            .append_events(
                "s1",
                &[SessionEvent::Assistant {
                    message: AgentMessage::Assistant(a),
                    usage: None,
                }],
            )
            .await
            .unwrap();
        let stats = reindex(tmp.path(), &db).await.unwrap();
        assert_eq!(stats, Stats::default(), "no text content → no row");
        let conn = init_db(&db).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "indexed count unchanged");
    }

    #[tokio::test]
    async fn reindex_indexes_seq0_text_once() {
        let tmp = tempfile::tempdir().unwrap();
        let store = EventStorage::new(tmp.path());
        let db = tmp.path().join("index.db");
        // No TurnStart: the session's only text event sits at seq 0.
        store
            .append_events("s1", &[user_ev("sole message")])
            .await
            .unwrap();
        let first = reindex(tmp.path(), &db).await.unwrap();
        assert_eq!(
            (first.sessions, first.events_indexed),
            (1, 1),
            "seq-0 text is indexed on first run"
        );
        let second = reindex(tmp.path(), &db).await.unwrap();
        assert_eq!(second, Stats::default(), "not re-inserted on second run");
    }
}
