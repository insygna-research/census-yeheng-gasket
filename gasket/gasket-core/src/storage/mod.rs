//! Append-only JSONL stores for sessions.
//!
//! Layout under `~/.gasket/`:
//! ```text
//! sessions/
//!   {session_id}/
//!     events.jsonl     # append-only, one SessionEvent per line
//!     messages.jsonl   # legacy: append-only, one AgentMessage per line
//! tool_state/
//!   {session_id}/{tool_name}/  # per-plugin private state
//! ```
//!
//! ## Format contract
//!
//! - **Single writer per session.** Appends open their own handle with
//!   `O_APPEND`, so a whole-line write is atomic against other writers, but
//!   concurrent writers can interleave batches (and a torn tail can only be
//!   produced by the writer that owns the file). Hosts must serialize appends
//!   to one session — the CLI and the gateway each run one loop per session.
//! - **Torn tails are crash artifacts, not data.** If the final line fails to
//!   parse (an append interrupted by crash/power loss), loading drops it and
//!   truncates the file in place: the interrupted turn was incomplete anyway,
//!   and this keeps later appends clean. A corrupt line in the **middle**
//!   fails the load with the file line number — that is real damage (bit rot,
//!   external edit), not a crash artifact.
//! - **Schema evolution is additive only.** New struct fields must carry
//!   `#[serde(default)]`; adding enum variants is a breaking change for
//!   readers built against the older file format.

use std::path::{Path, PathBuf};

use crate::error::AgentError;
use crate::types::message::AgentMessage;
use crate::types::session_event::SessionEvent;

/// The gasket config/data root: `~/.gasket/`.
pub fn config_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".gasket")
}

/// A session id must be a flat, safe identifier: non-empty, ASCII
/// alphanumeric + `-`/`_` only, at most 128 chars. Rejects `/`, `\`, `..` -
/// defends against path traversal when the id originates from untrusted input
/// (e.g. the gateway's `?user_id=` query param).
pub fn is_valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn validate_session_id(id: &str) -> Result<(), AgentError> {
    if is_valid_session_id(id) {
        Ok(())
    } else {
        Err(AgentError::InvalidSessionId(id.to_string()))
    }
}

/// Append-only JSONL message store for sessions.
#[derive(Debug, Clone)]
pub struct JsonlStorage {
    base_dir: PathBuf,
}

impl JsonlStorage {
    /// Create a store rooted at `base_dir` (typically `~/.gasket/sessions`).
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
    }

    /// Default store at `<config_dir>/sessions`.
    pub fn default_root() -> Self {
        Self::new(config_dir().join("sessions"))
    }

    /// 这个 store 的 root 目录（host 用来列举 session）。
    pub fn base_dir_clone(&self) -> PathBuf {
        self.base_dir.clone()
    }

    fn session_dir(&self, session_id: &str) -> PathBuf {
        self.base_dir.join(session_id)
    }

    /// Path to a session's `messages.jsonl` (whether or not it exists yet).
    pub fn messages_path(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join("messages.jsonl")
    }

    /// Append a single message to the session's JSONL log. Creates the session
    /// directory if missing.
    pub async fn append_message(
        &self,
        session_id: &str,
        msg: &AgentMessage,
    ) -> Result<(), AgentError> {
        validate_session_id(session_id)?;
        let path = self.messages_path(session_id);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        append_line(&mut file, msg).await
    }

    /// Append a batch of messages in order. Creates the session directory once
    /// and writes all lines to a single open file handle. Hosts call this after
    /// a run to persist the returned `Vec<AgentMessage>` transcript.
    pub async fn append_messages(
        &self,
        session_id: &str,
        msgs: &[AgentMessage],
    ) -> Result<(), AgentError> {
        if msgs.is_empty() {
            return Ok(());
        }
        validate_session_id(session_id)?;
        let path = self.messages_path(session_id);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        for msg in msgs {
            append_line(&mut file, msg).await?;
        }
        Ok(())
    }

    /// Load all messages for a session, in append order. Returns empty vec for
    /// a session that has never been written.
    ///
    /// Applies the torn-tail recovery policy (see the module docs): a final
    /// line that fails to parse is dropped and the file truncated in place;
    /// a corrupt line in the middle fails with its file line number.
    pub async fn load_messages(&self, session_id: &str) -> Result<Vec<AgentMessage>, AgentError> {
        validate_session_id(session_id)?;
        parse_transcript(&self.messages_path(session_id)).await
    }

    /// Load messages from an arbitrary JSONL file (used by tests/hosts that
    /// point at a specific file). Same recovery policy as
    /// [`load_messages`](Self::load_messages).
    pub async fn load_from_file(path: &Path) -> Result<Vec<AgentMessage>, AgentError> {
        parse_transcript(path).await
    }
}

/// Write one record as a single `line\n` buffer: one `write_all`, so a crash
/// can never leave a complete line dangling without its terminator — it leaves
/// either a full line or a truncated fragment, and a truncated final fragment
/// is what [`scan_jsonl`] repairs on the next load.
async fn append_line<T: serde::Serialize>(
    file: &mut tokio::fs::File,
    value: &T,
) -> Result<(), AgentError> {
    use tokio::io::AsyncWriteExt;
    let mut line = serde_json::to_string(value)?;
    line.push('\n');
    file.write_all(line.as_bytes()).await?;
    Ok(())
}

/// Parse a message transcript, applying the torn-tail recovery policy.
///
/// Thin wrapper over the generic [`scan_jsonl`] scanner; see that function
/// (and the module docs) for the exact semantics.
async fn parse_transcript(path: &Path) -> Result<Vec<AgentMessage>, AgentError> {
    scan_jsonl::<AgentMessage>(path, true).await
}

/// Scan a JSONL file into `T` rows, applying the torn-tail recovery policy.
///
/// A missing file is an empty log. Returns `Err(AgentError::Transcript)`
/// naming the file line for a corrupt line in the middle of the file (real
/// damage). If only the **last** line is invalid it is a torn tail (an append
/// interrupted by crash/power loss): the line is dropped and — when
/// `repair_tail` is set — the file is truncated at that line's start, so
/// loading succeeds with the preceding records and later appends stay clean.
async fn scan_jsonl<T: serde::de::DeserializeOwned>(
    path: &Path,
    repair_tail: bool,
) -> Result<Vec<T>, AgentError> {
    let bytes = match tokio::fs::read(path).await {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut items = Vec::new();
    let mut line_start = 0usize;
    let mut line_no = 0usize;
    for (idx, b) in bytes.iter().enumerate() {
        if *b != b'\n' {
            continue;
        }
        line_no += 1;
        let this_line_start = line_start;
        let line = bytes[this_line_start..idx].trim_ascii();
        // Nothing but whitespace after this line means it is the last one.
        let is_last = bytes[idx + 1..].iter().all(|b| b.is_ascii_whitespace());
        line_start = idx + 1;
        if line.is_empty() {
            continue;
        }
        match serde_json::from_slice::<T>(line) {
            Ok(m) => items.push(m),
            Err(e) => {
                if is_last {
                    tracing::warn!(
                        path = %path.display(),
                        line = line_no,
                        error = %e,
                        "dropping torn transcript tail"
                    );
                    if repair_tail {
                        repair_torn_tail(path, this_line_start).await?;
                    }
                    return Ok(items);
                }
                return Err(AgentError::Transcript(format!(
                    "invalid line {line_no} in {}: {e}",
                    path.display()
                )));
            }
        }
    }
    // Trailing fragment after the last newline (no terminator yet).
    let tail = bytes[line_start..].trim_ascii();
    if !tail.is_empty() {
        line_no += 1;
        match serde_json::from_slice::<T>(tail) {
            Ok(m) => items.push(m),
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    line = line_no,
                    error = %e,
                    "dropping torn transcript tail"
                );
                if repair_tail {
                    repair_torn_tail(path, line_start).await?;
                }
            }
        }
    }
    Ok(items)
}

/// Truncate the transcript at `keep_until` (the byte offset where a torn line
/// starts), so later appends land after valid data and future loads never
/// re-hit the bad line.
async fn repair_torn_tail(path: &Path, keep_until: usize) -> Result<(), AgentError> {
    let file = tokio::fs::OpenOptions::new().write(true).open(path).await?;
    file.set_len(keep_until as u64).await?;
    Ok(())
}

/// Append-only JSONL session event store: `events.jsonl`, one
/// [`SessionEvent`] per line, written with the same discipline (one
/// `O_APPEND` handle, single `write_all` of `line\n`) and read with the same
/// torn-tail policy as [`JsonlStorage`]. `messages.jsonl` in the same
/// session directory is the legacy format; [`EventStorage`] can read it as
/// the migration source and remove it once migrated.
#[derive(Debug, Clone)]
pub struct EventStorage {
    root: PathBuf,
}

impl EventStorage {
    /// Create a store rooted at `root` (typically `~/.gasket/sessions`).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn session_dir(&self, session_id: &str) -> PathBuf {
        self.root.join(session_id)
    }

    /// Path to a session's `events.jsonl` (whether or not it exists yet).
    pub fn events_path(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join("events.jsonl")
    }

    /// Path to a session's legacy `messages.jsonl` — the migration source.
    pub fn messages_path(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join("messages.jsonl")
    }

    /// Whether the session has an event log on disk.
    pub fn has_events(&self, session_id: &str) -> bool {
        is_valid_session_id(session_id) && self.events_path(session_id).exists()
    }

    /// Append a single event to the session's JSONL log. Creates the session
    /// directory if missing.
    pub async fn append_event(
        &self,
        session_id: &str,
        ev: &SessionEvent,
    ) -> Result<(), AgentError> {
        validate_session_id(session_id)?;
        let path = self.events_path(session_id);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        append_line(&mut file, ev).await
    }

    /// Append a batch of events in order to a single open file handle.
    /// An empty batch is a no-op (no directory or file is created).
    pub async fn append_events(
        &self,
        session_id: &str,
        evs: &[SessionEvent],
    ) -> Result<(), AgentError> {
        if evs.is_empty() {
            return Ok(());
        }
        validate_session_id(session_id)?;
        let path = self.events_path(session_id);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        for ev in evs {
            append_line(&mut file, ev).await?;
        }
        Ok(())
    }

    /// Load all events for a session, in append order. Returns empty vec for
    /// a session that has never been written. Same recovery policy as
    /// [`JsonlStorage::load_messages`].
    pub async fn load_events(&self, session_id: &str) -> Result<Vec<SessionEvent>, AgentError> {
        validate_session_id(session_id)?;
        scan_jsonl::<SessionEvent>(&self.events_path(session_id), true).await
    }

    /// Load the legacy `messages.jsonl` for a session — the migration source.
    /// Returns empty vec when there is nothing to migrate.
    pub async fn load_messages(&self, session_id: &str) -> Result<Vec<AgentMessage>, AgentError> {
        validate_session_id(session_id)?;
        scan_jsonl::<AgentMessage>(&self.messages_path(session_id), true).await
    }

    /// Remove the session's legacy `messages.jsonl` after a successful
    /// migration. Other files in the session directory are untouched; a
    /// missing file is not an error.
    pub async fn delete_legacy(&self, session_id: &str) -> Result<(), AgentError> {
        validate_session_id(session_id)?;
        match tokio::fs::remove_file(self.messages_path(session_id)).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::message::{ContentBlock, UserMessage};

    fn user_msg(text: &str) -> AgentMessage {
        AgentMessage::User(UserMessage {
            content: vec![ContentBlock::text(text)],
            timestamp: 42,
        })
    }

    #[tokio::test]
    async fn append_then_load_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let store = JsonlStorage::new(tmp.path());

        store
            .append_message("s1", &user_msg("hello"))
            .await
            .unwrap();
        store
            .append_message("s1", &user_msg("world"))
            .await
            .unwrap();

        let loaded = store.load_messages("s1").await.unwrap();
        assert_eq!(loaded.len(), 2);
        // Order preserved.
        assert!(
            matches!(&loaded[0], AgentMessage::User(u) if matches!(&u.content[0], ContentBlock::Text { text } if text == "hello"))
        );
        assert!(
            matches!(&loaded[1], AgentMessage::User(u) if matches!(&u.content[0], ContentBlock::Text { text } if text == "world"))
        );
    }

    #[tokio::test]
    async fn append_messages_batch_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let store = JsonlStorage::new(tmp.path());
        let batch = vec![user_msg("a"), user_msg("b"), user_msg("c")];
        store.append_messages("s1", &batch).await.unwrap();

        let loaded = store.load_messages("s1").await.unwrap();
        assert_eq!(loaded.len(), 3);
        assert_eq!(
            store.messages_path("s1"),
            tmp.path().join("s1").join("messages.jsonl")
        );
    }

    #[tokio::test]
    async fn append_messages_empty_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let store = JsonlStorage::new(tmp.path());
        store.append_messages("s1", &[]).await.unwrap();
        // No file created for an empty batch.
        assert!(!store.messages_path("s1").exists());
    }

    #[tokio::test]
    async fn load_missing_session_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let store = JsonlStorage::new(tmp.path());
        let loaded = store.load_messages("never-existed").await.unwrap();
        assert!(loaded.is_empty());
    }

    #[tokio::test]
    async fn append_creates_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let store = JsonlStorage::new(tmp.path());
        // Session dir does not exist yet.
        store.append_message("s1", &user_msg("x")).await.unwrap();
        let loaded = store.load_messages("s1").await.unwrap();
        assert_eq!(loaded.len(), 1);
    }

    #[tokio::test]
    async fn rejects_path_traversal_session_id() {
        // A session id carrying path components must never reach the filesystem.
        let tmp = tempfile::tempdir().unwrap();
        let store = JsonlStorage::new(tmp.path());
        for bad in ["../evil", "nested/s1", "/etc", "..", "a\\b", ""] {
            assert!(
                store.append_message(bad, &user_msg("x")).await.is_err(),
                "{bad:?} should be rejected"
            );
            assert!(
                store.append_messages(bad, &[user_msg("x")]).await.is_err(),
                "{bad:?} should be rejected (batch)"
            );
            assert!(
                store.load_messages(bad).await.is_err(),
                "{bad:?} should be rejected (load)"
            );
        }
        // Nothing was written outside the store root.
        assert!(!tmp.path().join("../evil").exists());
    }

    // ── Torn-tail recovery ────────────────────────────────────────

    fn raw_line(msg: &AgentMessage) -> String {
        serde_json::to_string(msg).unwrap()
    }

    fn write_raw(store: &JsonlStorage, session_id: &str, content: &str) {
        let path = store.messages_path(session_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    /// A truncated final line (write interrupted by crash/power loss) must be
    /// dropped, the file repaired in place, and later appends must load clean.
    #[tokio::test]
    async fn torn_tail_is_dropped_and_file_repaired() {
        let tmp = tempfile::tempdir().unwrap();
        let store = JsonlStorage::new(tmp.path());

        let good = raw_line(&user_msg("ok"));
        let torn = &good[..good.len() / 2]; // mid-JSON truncation
        write_raw(&store, "s1", &format!("{good}\n{torn}"));

        let loaded = store.load_messages("s1").await.unwrap();
        assert_eq!(loaded.len(), 1, "torn tail must be dropped, prefix kept");
        assert!(
            matches!(&loaded[0], AgentMessage::User(u) if matches!(&u.content[0], ContentBlock::Text { text } if text == "ok"))
        );

        // File on disk is repaired: only the good line (plus terminator) remains.
        let raw = std::fs::read_to_string(store.messages_path("s1")).unwrap();
        assert_eq!(
            raw,
            format!("{good}\n"),
            "file must be truncated at the torn line"
        );

        // Appending after the repair works and never re-hits the bad line.
        store
            .append_message("s1", &user_msg("after"))
            .await
            .unwrap();
        let loaded = store.load_messages("s1").await.unwrap();
        assert_eq!(loaded.len(), 2);
        assert!(
            matches!(&loaded[1], AgentMessage::User(u) if matches!(&u.content[0], ContentBlock::Text { text } if text == "after"))
        );
    }

    /// A torn tail that still ends with a newline (crash after the `\n` of a
    /// garbage line) must be repaired the same way.
    #[tokio::test]
    async fn torn_tail_with_newline_is_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        let store = JsonlStorage::new(tmp.path());
        let good = raw_line(&user_msg("ok"));
        write_raw(&store, "s1", &format!("{good}\nNOT_JSON\n"));

        let loaded = store.load_messages("s1").await.unwrap();
        assert_eq!(loaded.len(), 1);
        let raw = std::fs::read_to_string(store.messages_path("s1")).unwrap();
        assert_eq!(raw, format!("{good}\n"));
    }

    /// A file whose only line is torn (first write crashed) becomes an empty
    /// but usable session.
    #[tokio::test]
    async fn single_torn_line_becomes_empty_session() {
        let tmp = tempfile::tempdir().unwrap();
        let store = JsonlStorage::new(tmp.path());
        let good = raw_line(&user_msg("ok"));
        write_raw(&store, "s1", &good[..good.len() / 2]);

        assert!(store.load_messages("s1").await.unwrap().is_empty());
        store
            .append_message("s1", &user_msg("first"))
            .await
            .unwrap();
        let loaded = store.load_messages("s1").await.unwrap();
        assert_eq!(loaded.len(), 1);
    }

    /// A corrupt line in the middle is real damage, not a crash artifact: the
    /// load must fail loudly with the file line number.
    #[tokio::test]
    async fn mid_file_corruption_fails_with_line_number() {
        let tmp = tempfile::tempdir().unwrap();
        let store = JsonlStorage::new(tmp.path());
        let good = raw_line(&user_msg("ok"));
        write_raw(&store, "s1", &format!("{good}\nNOT_JSON\n{good}\n"));

        let err = store.load_messages("s1").await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("line 2"),
            "error must name the file line, got: {msg}"
        );
        assert!(
            msg.contains("messages.jsonl"),
            "error must name the file, got: {msg}"
        );
        // No repair for mid-file damage: the file is untouched.
        let raw = std::fs::read_to_string(store.messages_path("s1")).unwrap();
        assert_eq!(raw, format!("{good}\nNOT_JSON\n{good}\n"));
    }

    /// A trailing fragment that is *valid* JSON without a newline (crash
    /// between line and terminator under the old two-write scheme) parses
    /// fine — `load_from_file` shares the same policy.
    #[tokio::test]
    async fn load_from_file_shares_recovery_policy() {
        let tmp = tempfile::tempdir().unwrap();
        let store = JsonlStorage::new(tmp.path());
        let good = raw_line(&user_msg("ok"));
        write_raw(
            &store,
            "s1",
            &format!("{good}\n{}", &good[..good.len() / 2]),
        );

        let loaded = JsonlStorage::load_from_file(&store.messages_path("s1"))
            .await
            .unwrap();
        assert_eq!(loaded.len(), 1);
    }

    // ── EventStorage ──────────────────────────────────────────────

    use crate::types::session_event::{SessionEvent, TurnEndReason};

    fn sample_events() -> Vec<SessionEvent> {
        vec![
            SessionEvent::TurnStart,
            SessionEvent::User(user_msg("hello")),
            SessionEvent::TurnEnd {
                reason: TurnEndReason::Completed,
            },
        ]
    }

    fn write_raw_events(store: &EventStorage, session_id: &str, content: &str) {
        let path = store.events_path(session_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[tokio::test]
    async fn events_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let store = EventStorage::new(tmp.path());
        let events = sample_events();

        store.append_events("s1", &events).await.unwrap();

        let loaded = store.load_events("s1").await.unwrap();
        assert_eq!(loaded, events);
        assert_eq!(
            store.events_path("s1"),
            tmp.path().join("s1").join("events.jsonl")
        );
    }

    #[tokio::test]
    async fn events_torn_tail_last_line_dropped_and_repaired() {
        let tmp = tempfile::tempdir().unwrap();
        let store = EventStorage::new(tmp.path());

        let good1 = serde_json::to_string(&SessionEvent::TurnStart).unwrap();
        let good2 = serde_json::to_string(&SessionEvent::User(user_msg("ok"))).unwrap();
        let torn = &good2[..good2.len() / 2]; // mid-JSON truncation
        write_raw_events(&store, "s1", &format!("{good1}\n{good2}\n{torn}"));

        let loaded = store.load_events("s1").await.unwrap();
        assert_eq!(loaded.len(), 2, "torn tail must be dropped, prefix kept");

        // File on disk is repaired: truncated at the torn line.
        let raw = std::fs::read_to_string(store.events_path("s1")).unwrap();
        assert_eq!(raw, format!("{good1}\n{good2}\n"));
    }

    #[tokio::test]
    async fn events_mid_file_corruption_errors_with_line_number() {
        let tmp = tempfile::tempdir().unwrap();
        let store = EventStorage::new(tmp.path());
        let good = serde_json::to_string(&SessionEvent::TurnStart).unwrap();
        write_raw_events(&store, "s1", &format!("NOT_JSON\n{good}\n"));

        let err = store.load_events("s1").await.unwrap_err();
        assert!(matches!(err, AgentError::Transcript(_)));
        let msg = err.to_string();
        assert!(
            msg.contains("line 1"),
            "error must name the file line, got: {msg}"
        );
        assert!(
            msg.contains("events.jsonl"),
            "error must name the file, got: {msg}"
        );
    }

    #[tokio::test]
    async fn events_unknown_variant_fails_load() {
        // Fail closed: a row the reader does not know must fail the load, not
        // be silently skipped. Placed mid-file so it is treated as real
        // damage rather than a torn tail.
        let tmp = tempfile::tempdir().unwrap();
        let store = EventStorage::new(tmp.path());
        let good = serde_json::to_string(&SessionEvent::TurnStart).unwrap();
        write_raw_events(
            &store,
            "s1",
            &format!("{{\"type\":\"from_the_future\"}}\n{good}\n"),
        );

        let err = store.load_events("s1").await.unwrap_err();
        assert!(matches!(err, AgentError::Transcript(_)));
    }
}
