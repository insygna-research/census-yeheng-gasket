//! SessionManager: wraps [`EventStorage`], adding "current session/list/
//! latest" semantics plus the one-shot legacy `messages.jsonl` migration.
//!
//! The event log (`events.jsonl`) is the single source of truth. Legacy
//! `messages.jsonl` transcripts migrate once — wrapped row-by-row into the
//! event log, deleted only after the full write succeeded (D1: not kept) —
//! and any corruption fails closed with `Err` instead of being adopted.
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use gasket_core::{
    derive_messages, AgentError, AgentMessage, EventStorage, JsonlStorage, SessionEvent,
};

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub id: String,
    pub mtime: SystemTime,
    pub msg_count: usize,
}

/// Cursor semantics: `new()` generates the initial `current_id`; only
/// `new` / `resume` / `clear` change it; events always append to the
/// current id. A fresh manager that was never resumed writes a brand-new
/// session — callers that want an existing session MUST `resume` first.
pub struct SessionManager {
    root: PathBuf,
    storage: EventStorage,
    current_id: String,
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionManager {
    pub fn new() -> Self {
        Self::with_root(JsonlStorage::default_root().base_dir_clone())
    }

    /// 测试用：指定 root。
    pub fn with_root(root: PathBuf) -> Self {
        Self {
            root: root.clone(),
            storage: EventStorage::new(root),
            current_id: uuid::Uuid::new_v4().to_string(),
        }
    }

    pub fn current_id(&self) -> &str {
        &self.current_id
    }

    /// 打开或迁移:events.jsonl 存在 → load;否则 messages.jsonl 存在 →
    /// 旧消息逐条 `SessionEvent::from_message` 包裹,经 tmp+rename 原子
    /// 写入 events.jsonl,迁移成功后删除旧 messages.jsonl(D1:不保留);
    /// 两者皆无 → 新会话。中间行损坏 → `Err`(fail closed,绝不 adopt)。
    ///
    /// Does not change the current-session cursor; [`resume`](Self::resume)
    /// adopts the id on top of this.
    pub async fn open_or_migrate(&self, session_id: &str) -> Result<Vec<SessionEvent>, AgentError> {
        if self.storage.has_events(session_id) {
            return self.storage.load_events(session_id).await;
        }
        let legacy = self.storage.load_messages(session_id).await?;
        if legacy.is_empty() {
            // Neither an event log nor legacy content: fresh session.
            return Ok(Vec::new());
        }
        let mut events = Vec::with_capacity(legacy.len());
        for msg in &legacy {
            let ev = SessionEvent::from_message(msg, None).ok_or_else(|| {
                AgentError::Transcript(format!(
                    "session {session_id}: legacy row cannot be represented as a session event"
                ))
            })?;
            events.push(ev);
        }
        // Atomic install (tmp + rename): a crash can never leave a torn
        // events.jsonl shadowing the intact legacy file. Crash windows:
        //  * before the rename — only the `.tmp` exists, `has_events` stays
        //    false, and the next open re-migrates from the untouched legacy
        //    (idempotent; the stale `.tmp` is replaced wholesale).
        //  * after the rename, before `delete_legacy` — events.jsonl is
        //    complete and `has_events` short-circuits to it, so the leftover
        //    legacy file is inert and is deliberately left in place: D1's
        //    "delete after success" is satisfied at the rename boundary, and
        //    a second opportunistic cleanup pass is not worth the code.
        self.storage
            .append_events_atomic(session_id, &events)
            .await?;
        self.storage.delete_legacy(session_id).await?;
        Ok(events)
    }

    /// Append one event to the current session's log.
    pub async fn append_event(&self, ev: &SessionEvent) -> Result<(), AgentError> {
        self.storage.append_event(&self.current_id, ev).await
    }

    /// The agent loop's sync `persist` callback, backed by the store's
    /// synchronous `std::fs` append — no runtime bridging, no
    /// thread-spawn-per-event, and safe to call from any thread (the
    /// `Handle::block_on` nested-runtime panic risk is gone by
    /// construction). Lines are small and events per turn are few, so the
    /// brief blocking write is noise next to an LLM round-trip.
    #[allow(clippy::type_complexity)]
    pub fn persist_fn(&self) -> Arc<dyn Fn(&SessionEvent) -> Result<(), AgentError> + Send + Sync> {
        let storage = self.storage.clone();
        let sid = self.current_id.clone();
        Arc::new(move |ev| storage.append_event_sync(&sid, ev))
    }

    /// Load (migrating if needed) a session and adopt it as the current one.
    /// Returns the derived model-visible history. Corruption fails closed.
    pub async fn resume(&mut self, id: &str) -> Result<Vec<AgentMessage>, crate::HostError> {
        let events = self
            .open_or_migrate(id)
            .await
            .map_err(|e| crate::HostError::Session(e.to_string()))?;
        self.current_id = id.to_string();
        Ok(derive_messages(&events))
    }

    pub async fn resume_last(&mut self) -> Result<Vec<AgentMessage>, crate::HostError> {
        let id = self
            .list()
            .await?
            .into_iter()
            .max_by_key(|s| s.mtime)
            .map(|s| s.id)
            .ok_or_else(|| crate::HostError::Session("no prior session".into()))?;
        self.resume(&id).await
    }

    pub async fn list(&self) -> Result<Vec<SessionInfo>, crate::HostError> {
        let root = self.root.clone();
        let mut out = Vec::new();
        // Fresh install: sessions dir doesn't exist yet -> no sessions.
        let mut rd = match tokio::fs::read_dir(&root).await {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(crate::HostError::Session(e.to_string())),
        };
        while let Some(entry) = rd
            .next_entry()
            .await
            .map_err(|e| crate::HostError::Session(e.to_string()))?
        {
            if !entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let id = entry.file_name().to_string_lossy().to_string();
            // mtime comes from the transcript file, NOT the session dir:
            // appending updates the file's mtime but leaves the dir's
            // untouched, so dir mtime would freeze at first write. Prefer
            // events.jsonl; an unmigrated legacy session falls back to
            // messages.jsonl.
            let path = if self.storage.has_events(&id) {
                self.storage.events_path(&id)
            } else {
                self.storage.messages_path(&id)
            };
            let mtime = tokio::fs::metadata(&path)
                .await
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            // Count lines without serde-parsing the whole transcript.
            let msg_count = match tokio::fs::read_to_string(&path).await {
                Ok(s) => s.lines().filter(|l| !l.trim().is_empty()).count(),
                Err(_) => 0,
            };
            out.push(SessionInfo {
                id,
                mtime,
                msg_count,
            });
        }
        Ok(out)
    }

    pub fn clear(&mut self) {
        self.current_id = uuid::Uuid::new_v4().to_string();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gasket_core::{ContentBlock, UserMessage};

    fn user_msg(t: &str) -> AgentMessage {
        AgentMessage::User(UserMessage {
            content: vec![ContentBlock::text(t)],
            timestamp: 1,
        })
    }

    fn user_event(t: &str) -> SessionEvent {
        SessionEvent::User(user_msg(t))
    }

    #[tokio::test]
    async fn resume_loads_and_sets_current() {
        let tmp = tempfile::tempdir().unwrap();
        let mut sm = SessionManager::with_root(tmp.path().to_path_buf());
        let id = "fixed-id".to_string();
        sm.current_id = id.clone();
        sm.append_event(&user_event("a")).await.unwrap();
        sm.append_event(&user_event("b")).await.unwrap();

        let mut sm2 = SessionManager::with_root(tmp.path().to_path_buf());
        let msgs = sm2.resume(&id).await.unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(sm2.current_id(), id);
    }

    #[tokio::test]
    async fn resume_last_picks_most_recent() {
        let tmp = tempfile::tempdir().unwrap();
        let mut a = SessionManager::with_root(tmp.path().to_path_buf());
        a.current_id = "old".into();
        a.append_event(&user_event("old")).await.unwrap();
        // 让 new 的 mtime 晚于 old
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut b = SessionManager::with_root(tmp.path().to_path_buf());
        b.current_id = "new".into();
        b.append_event(&user_event("new")).await.unwrap();

        let mut pick = SessionManager::with_root(tmp.path().to_path_buf());
        let msgs = pick.resume_last().await.unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(pick.current_id(), "new");
    }

    #[tokio::test]
    async fn resume_last_uses_latest_message_mtime() {
        // Regression: dir mtime freezes at first write, so a session that gets
        // a *second* append after another session was created must still win.
        let tmp = tempfile::tempdir().unwrap();
        let mut a = SessionManager::with_root(tmp.path().to_path_buf());
        a.current_id = "a".into();
        a.append_event(&user_event("a1")).await.unwrap();

        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut b = SessionManager::with_root(tmp.path().to_path_buf());
        b.current_id = "b".into();
        b.append_event(&user_event("b1")).await.unwrap();

        // a receives a later event -> a is the most recently active session.
        std::thread::sleep(std::time::Duration::from_millis(20));
        a.append_event(&user_event("a2")).await.unwrap();

        let mut pick = SessionManager::with_root(tmp.path().to_path_buf());
        let msgs = pick.resume_last().await.unwrap();
        assert_eq!(pick.current_id(), "a");
        assert_eq!(msgs.len(), 2);
    }

    #[tokio::test]
    async fn list_on_missing_root_is_empty() {
        // Fresh install: sessions dir doesn't exist yet.
        let tmp = tempfile::tempdir().unwrap();
        let sm = SessionManager::with_root(tmp.path().join("nope"));
        assert!(sm.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn clear_starts_new_session() {
        let tmp = tempfile::tempdir().unwrap();
        let mut sm = SessionManager::with_root(tmp.path().to_path_buf());
        let id1 = sm.current_id().to_string();
        sm.clear();
        assert_ne!(sm.current_id(), id1);
    }

    #[tokio::test]
    async fn persist_fn_writes_events_outside_async_ctx() {
        // The loop's sync persist callback must land events in the log —
        // including when called from a plain (non-async) caller.
        let tmp = tempfile::tempdir().unwrap();
        let mut sm = SessionManager::with_root(tmp.path().to_path_buf());
        sm.current_id = "persisted".into();
        let persist = sm.persist_fn();
        persist(&user_event("via-callback")).unwrap();

        let events = sm.open_or_migrate("persisted").await.unwrap();
        assert_eq!(events, vec![user_event("via-callback")]);
    }

    fn write_legacy(dir: &std::path::Path, msgs: &[AgentMessage]) {
        let mut raw = String::new();
        for m in msgs {
            raw.push_str(&serde_json::to_string(m).unwrap());
            raw.push('\n');
        }
        std::fs::write(dir.join("messages.jsonl"), raw).unwrap();
    }

    /// Crash window before the rename: `events.jsonl.tmp` exists (possibly
    /// torn) but `events.jsonl` never materialized. `has_events` must be
    /// false, the legacy file must still be intact, and the next open must
    /// re-migrate the full transcript from scratch.
    #[tokio::test]
    async fn migration_crash_before_rename_re_migrates_from_legacy() {
        let tmp = tempfile::tempdir().unwrap();
        let sid = "crashed-migration";
        let dir = tmp.path().join(sid);
        std::fs::create_dir_all(&dir).unwrap();
        let legacy = vec![user_msg("hello"), user_msg("again")];
        write_legacy(&dir, &legacy);
        // The torn staging file a kill -9 between write and rename leaves.
        std::fs::write(dir.join("events.jsonl.tmp"), "{\"type\":\"TurnSta").unwrap();

        let sm = SessionManager::with_root(tmp.path().to_path_buf());
        let events = sm.open_or_migrate(sid).await.unwrap();
        assert_eq!(
            derive_messages(&events),
            legacy,
            "the retry must migrate the full legacy transcript"
        );
        assert!(dir.join("events.jsonl").exists());
        assert!(
            !dir.join("events.jsonl.tmp").exists(),
            "the retry must replace the stale staging file wholesale"
        );
        assert!(
            !dir.join("messages.jsonl").exists(),
            "legacy removed only after the successful atomic install"
        );
    }

    /// Crash window after the rename but before `delete_legacy`:
    /// `events.jsonl` is complete, so the open short-circuits to it and the
    /// leftover legacy file is inert and left in place (documented choice —
    /// the rename boundary is the success point for D1).
    #[tokio::test]
    async fn migration_crash_after_rename_opens_events_and_leaves_legacy() {
        let tmp = tempfile::tempdir().unwrap();
        let sid = "crashed-cleanup";
        let dir = tmp.path().join(sid);
        std::fs::create_dir_all(&dir).unwrap();
        // The already-complete migrated log plus an orphaned legacy file.
        let events = vec![user_event("migrated-a"), user_event("migrated-b")];
        let mut raw = String::new();
        for ev in &events {
            raw.push_str(&serde_json::to_string(ev).unwrap());
            raw.push('\n');
        }
        std::fs::write(dir.join("events.jsonl"), raw).unwrap();
        write_legacy(&dir, &[user_msg("old-a"), user_msg("old-b")]);

        let sm = SessionManager::with_root(tmp.path().to_path_buf());
        let loaded = sm.open_or_migrate(sid).await.unwrap();
        assert_eq!(
            loaded, events,
            "must load the complete events.jsonl, never consult the legacy file"
        );
        assert!(
            dir.join("messages.jsonl").exists(),
            "the leftover legacy is inert cleanup, deliberately not deleted here"
        );
    }
}
