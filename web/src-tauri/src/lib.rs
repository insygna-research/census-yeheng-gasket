//! Gasket desktop backend.
//!
//! The session-management API lives here as Tauri commands so the desktop
//! app is self-contained: it reads/writes the on-disk session store
//! (`~/.gasket/sessions`) directly through gasket-core/gasket-host instead of
//! depending on a separately running gateway process. The `chat` module goes
//! one step further and hosts the agent loop itself: per-session Hosts stream
//! turn events over Tauri IPC (`chat-event`), replacing the gateway's
//! WebSocket transport inside the desktop shell. The gateway remains the
//! transport for plain-browser (dev) usage.

use gasket_core::{EventStorage, SessionMeta};

mod chat;

fn session_store() -> EventStorage {
  EventStorage::new(gasket_core::JsonlStorage::default_root().base_dir_clone())
}

#[derive(serde::Serialize)]
struct SessionInfoDto {
  id: String,
  msg_count: usize,
  name: Option<String>,
  /// Milliseconds since UNIX epoch; 0 when the file has no mtime.
  mtime: u64,
}

#[tauri::command]
async fn list_sessions() -> Result<Vec<SessionInfoDto>, String> {
  let mgr = gasket_host::SessionManager::new();
  let mut sessions = mgr.list().await.map_err(|e| e.to_string())?;
  // Newest first.
  sessions.sort_by_key(|s| std::cmp::Reverse(s.mtime));
  Ok(
    sessions
      .into_iter()
      .map(|s| SessionInfoDto {
        id: s.id,
        msg_count: s.msg_count,
        name: s.name,
        mtime: s
          .mtime
          .duration_since(std::time::UNIX_EPOCH)
          .map(|d| d.as_millis() as u64)
          .unwrap_or(0),
      })
      .collect(),
  )
}

/// Backend-truth transcript for a session. `Ok(None)` means the session has
/// no on-disk data yet (a local-only chat) — the frontend keeps its local
/// state in that case. Corruption fails loud with `Err`.
#[tauri::command]
async fn get_session_messages(id: String) -> Result<Option<Vec<serde_json::Value>>, String> {
  let storage = session_store();
  if !storage.has_events(&id) && !storage.messages_path(&id).exists() {
    return Ok(None);
  }
  let mgr = gasket_host::SessionManager::new();
  let events = mgr.open_or_migrate(&id).await.map_err(|e| e.to_string())?;
  let messages = gasket_core::derive_messages(&events);
  serde_json::to_value(messages)
    .map(|v| v.as_array().cloned())
    .map_err(|e| e.to_string())
}

/// Persist the session's display name (meta.json sidecar). Creates the
/// session directory if needed, so a chat can be named before its first
/// turn lands on disk.
#[tauri::command]
async fn rename_session(id: String, name: String) -> Result<(), String> {
  if !gasket_core::is_valid_session_id(&id) {
    return Err("invalid session id".into());
  }
  let trimmed = name.trim();
  if trimmed.is_empty() || trimmed.chars().count() > 200 {
    return Err("name must be 1..=200 chars".into());
  }
  session_store()
    .write_meta(
      &id,
      &SessionMeta {
        name: Some(trimmed.to_string()),
      },
    )
    .await
    .map_err(|e| e.to_string())
}

/// `~/.gasket/app_config.json` — the desktop shell's durable mirror of the
/// browser build's localStorage preferences (theme, sidebar state, chats
/// meta, hidden sessions). One JSON object keyed by storage key; values are
/// parsed JSON when possible, else the raw string — the frontend round-trips
/// them back into localStorage byte-for-byte. Same fail-loud conventions as
/// the session store: corruption is an error, never silently re-created.
fn app_config_path() -> std::path::PathBuf {
  gasket_core::storage::config_dir().join("app_config.json")
}

#[tauri::command]
fn get_app_config() -> Result<Option<serde_json::Value>, String> {
  match std::fs::read(app_config_path()) {
    Ok(bytes) => serde_json::from_slice(&bytes)
      .map(Some)
      .map_err(|e| format!("app config corrupt: {e}")),
    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
    Err(e) => Err(e.to_string()),
  }
}

/// Atomic write (tmp + rename): a crash can never leave a torn config
/// shadowing an intact one. The file is tiny and writes are debounced by the
/// frontend, so a blocking std::fs write is noise.
#[tauri::command]
fn set_app_config(config: serde_json::Value) -> Result<(), String> {
  let path = app_config_path();
  if let Some(parent) = path.parent() {
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
  }
  let bytes = serde_json::to_vec_pretty(&config).map_err(|e| e.to_string())?;
  let tmp = path.with_extension("json.tmp");
  std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
  std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

/// Delete the session's on-disk data wholesale (event log + meta sidecar).
/// Returns false when the session never existed.
#[tauri::command]
async fn delete_session(id: String) -> Result<bool, String> {
  session_store()
    .remove_session(&id)
    .await
    .map_err(|e| e.to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
  tauri::Builder::default()
    .plugin(tauri_plugin_notification::init())
    .manage(chat::ChatState::new())
    .invoke_handler(tauri::generate_handler![
      list_sessions,
      get_session_messages,
      rename_session,
      delete_session,
      chat::send_message,
      chat::cancel_turn,
      chat::approval_response,
      chat::get_context,
      get_app_config,
      set_app_config,
    ])
    .setup(|app| {
      if cfg!(debug_assertions) {
        app.handle().plugin(
          tauri_plugin_log::Builder::default()
            .level(log::LevelFilter::Info)
            .build(),
        )?;
      }
      Ok(())
    })
    .run(tauri::generate_context!())
    .expect("error while running tauri application");
}
