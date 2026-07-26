//! SessionManager: 包 JsonlStorage，加"当前 session/列举/最近"语义。
use std::path::PathBuf;
use std::time::SystemTime;

use gasket_core::{AgentMessage, JsonlStorage};

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub id: String,
    pub mtime: SystemTime,
    pub msg_count: usize,
}

pub struct SessionManager {
    storage: JsonlStorage,
    current_id: String,
}

impl SessionManager {
    pub fn new() -> Self {
        Self::with_root(JsonlStorage::default_root().base_dir_clone())
    }

    /// 测试用：指定 root。
    pub fn with_root(root: PathBuf) -> Self {
        let storage = JsonlStorage::new(root);
        let current_id = uuid::Uuid::new_v4().to_string();
        Self { storage, current_id }
    }

    pub fn current_id(&self) -> &str {
        &self.current_id
    }

    pub async fn resume(&mut self, id: &str) -> Result<Vec<AgentMessage>, crate::HostError> {
        let msgs = self
            .storage
            .load_messages(id)
            .await
            .map_err(|e| crate::HostError::Session(e.to_string()))?;
        self.current_id = id.to_string();
        Ok(msgs)
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
        let root = self.storage.base_dir_clone();
        let mut out = Vec::new();
        let mut rd = tokio::fs::read_dir(&root)
            .await
            .map_err(|e| crate::HostError::Session(e.to_string()))?;
        while let Some(entry) = rd
            .next_entry()
            .await
            .map_err(|e| crate::HostError::Session(e.to_string()))?
        {
            if !entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let id = entry.file_name().to_string_lossy().to_string();
            let mtime = entry
                .metadata()
                .await
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            let msg_count = self
                .storage
                .load_messages(&id)
                .await
                .map(|v| v.len())
                .unwrap_or(0);
            out.push(SessionInfo { id, mtime, msg_count });
        }
        Ok(out)
    }

    pub async fn append(&self, msgs: &[AgentMessage]) -> Result<(), crate::HostError> {
        self.storage
            .append_messages(&self.current_id, msgs)
            .await
            .map_err(|e| crate::HostError::Session(e.to_string()))
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

    #[tokio::test]
    async fn resume_loads_and_sets_current() {
        let tmp = tempfile::tempdir().unwrap();
        let mut sm = SessionManager::with_root(tmp.path().to_path_buf());
        let id = "fixed-id".to_string();
        sm.current_id = id.clone();
        sm.append(&[user_msg("a"), user_msg("b")]).await.unwrap();

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
        a.append(&[user_msg("old")]).await.unwrap();
        // 让 new 的 mtime 晚于 old
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut b = SessionManager::with_root(tmp.path().to_path_buf());
        b.current_id = "new".into();
        b.append(&[user_msg("new")]).await.unwrap();

        let mut pick = SessionManager::with_root(tmp.path().to_path_buf());
        let msgs = pick.resume_last().await.unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(pick.current_id(), "new");
    }

    #[tokio::test]
    async fn clear_starts_new_session() {
        let tmp = tempfile::tempdir().unwrap();
        let mut sm = SessionManager::with_root(tmp.path().to_path_buf());
        let id1 = sm.current_id().to_string();
        sm.clear();
        assert_ne!(sm.current_id(), id1);
    }
}
