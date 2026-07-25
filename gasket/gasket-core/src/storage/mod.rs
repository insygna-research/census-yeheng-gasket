//! Append-only JSONL storage.
//!
//! See `gasket-refactor-plan.md` §6.
//!
//! Layout under `~/.gasket/`:
//! ```text
//! sessions/
//!   {session_id}/
//!     messages.jsonl   # append-only, one AgentMessage per line
//!     metadata.json    # session name, model, created_at
//! tool_state/
//!   {session_id}/{tool_name}/  # per-plugin private state
//! ```

use std::path::{Path, PathBuf};

use crate::error::AgentError;
use crate::types::message::AgentMessage;

/// The gasket config/data root: `~/.gasket/`.
pub fn config_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".gasket")
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

    fn session_dir(&self, session_id: &str) -> PathBuf {
        self.base_dir.join(session_id)
    }

    fn messages_path(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join("messages.jsonl")
    }

    /// Append a single message to the session's JSONL log. Creates the session
    /// directory if missing.
    pub async fn append_message(
        &self,
        session_id: &str,
        msg: &AgentMessage,
    ) -> Result<(), AgentError> {
        let path = self.messages_path(session_id);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        use tokio::io::AsyncWriteExt;
        let line = serde_json::to_string(msg)?;
        file.write_all(line.as_bytes()).await?;
        file.write_all(b"\n").await?;
        Ok(())
    }

    /// Load all messages for a session, in append order. Returns empty vec for
    /// a session that has never been written.
    pub async fn load_messages(
        &self,
        session_id: &str,
    ) -> Result<Vec<AgentMessage>, AgentError> {
        let path = self.messages_path(session_id);
        match tokio::fs::read_to_string(&path).await {
            Ok(contents) => {
                let mut messages = Vec::new();
                for line in contents.lines() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    messages.push(serde_json::from_str(line)?);
                }
                Ok(messages)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }

    /// Load messages from an arbitrary JSONL file (used by tests/hosts that
    /// point at a specific file).
    pub async fn load_from_file(path: &Path) -> Result<Vec<AgentMessage>, AgentError> {
        match tokio::fs::read_to_string(path).await {
            Ok(contents) => contents
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(|l| serde_json::from_str(l).map_err(AgentError::from))
                .collect(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
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

        store.append_message("s1", &user_msg("hello")).await.unwrap();
        store
            .append_message("s1", &user_msg("world"))
            .await
            .unwrap();

        let loaded = store.load_messages("s1").await.unwrap();
        assert_eq!(loaded.len(), 2);
        // Order preserved.
        assert!(matches!(&loaded[0], AgentMessage::User(u) if matches!(&u.content[0], ContentBlock::Text { text } if text == "hello")));
        assert!(matches!(&loaded[1], AgentMessage::User(u) if matches!(&u.content[0], ContentBlock::Text { text } if text == "world")));
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
        store.append_message("nested/s1", &user_msg("x")).await.unwrap();
        let loaded = store.load_messages("nested/s1").await.unwrap();
        assert_eq!(loaded.len(), 1);
    }
}
