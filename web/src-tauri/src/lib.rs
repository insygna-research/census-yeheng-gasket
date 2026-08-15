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


/// Cross-session full-text search (FTS5 sidecar at `~/.gasket/index.db`).
/// Stateless per call: open the connection, run the high-water incremental
/// reindex check, run the query, return hits. No registry, no cached
/// state — resource state belongs to the host, not process globals.
#[tauri::command]
async fn search_sessions(
  query: String,
) -> Result<Vec<gasket_host::session_index::SessionHit>, String> {
  let q = query.trim().to_string();
  if q.is_empty() {
    return Err("query must be non-empty".into());
  }
  let root = gasket_core::JsonlStorage::default_root().base_dir_clone();
  let db = gasket_core::storage::config_dir().join("index.db");
  tokio::task::spawn_blocking(move || {
    gasket_host::session_index::reindex(&root, &db).map_err(|e| e.to_string())?;
    gasket_host::session_index::search(&root, &db, &q, 20).map_err(|e| e.to_string())
  })
  .await
  .map_err(|e| format!("engine task join failed: {e}"))?
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

/// Extract `gasket_proxy` from the app config and install it as the
/// fetch/web_search proxy override. Missing or empty clears the override
/// (direct connection). Values may be raw strings (writeString path — not
/// JSON) or JSON strings; `as_str` covers both.
fn apply_proxy_from_config(config: &serde_json::Value) -> Result<(), String> {
  let url = config
    .get("gasket_proxy")
    .and_then(|v| v.as_str())
    .map(str::trim)
    .filter(|s| !s.is_empty());
  gasket_core::set_tool_proxy(url).map_err(|e| format!("gasket_proxy invalid: {e}"))
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
  std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
  // Apply only after the write succeeded, so runtime state can never
  // diverge from what is persisted.
  apply_proxy_from_config(&config)
}

/// Check a proxy URL against the same validation `set_tool_proxy` uses,
/// without installing it. The dialog calls this before saving so a bad
/// URL fails in the UI, not in a console.warn.
#[tauri::command]
fn validate_proxy(url: String) -> Result<(), String> {
  let url = url.trim();
  if url.is_empty() {
    return Ok(()); // clearing is always valid
  }
  gasket_core::validate_tool_proxy(url)
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
  let _ = dotenvy::dotenv();
  // gasket-core/gasket-host emit through `tracing`; without a global
  // subscriber those records vanish. This is separate from tauri-plugin-log
  // (fern, registered in setup) which handles `log`-crate records — the two
  // coexist only because tracing-subscriber is built without `tracing-log`
  // (see Cargo.toml). Bare `EnvFilter::from_default_env()` defaults to
  // ERROR-only when RUST_LOG is unset, so fall back to `info`.
  tracing_subscriber::fmt()
    .with_env_filter(
      tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
    )
    .init();
  tauri::Builder::default()
    .plugin(tauri_plugin_notification::init())
    .manage(chat::ChatState::new())
    .invoke_handler(tauri::generate_handler![
      list_sessions,
      get_session_messages,
      rename_session,
      search_sessions,
      delete_session,
      chat::send_message,
      chat::cancel_turn,
      chat::approval_response,
      chat::get_context,
      get_app_config,
      set_app_config,
      validate_proxy,
    ])
    .setup(|app| {
      if let Ok(Some(config)) = get_app_config() {
        if let Err(e) = apply_proxy_from_config(&config) {
          log::warn!("skipping invalid stored proxy: {e}");
        }
      }
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

#[cfg(test)]
mod tests {
  use super::*;

  struct NoopLogger;

  impl log::Log for NoopLogger {
    fn enabled(&self, _: &log::Metadata) -> bool {
      false
    }
    fn log(&self, _: &log::Record) {}
    fn flush(&self) {}
  }

  /// The tracing subscriber init must NOT claim the global `log` logger —
  /// tauri-plugin-log (fern) needs it, and losing that race aborts the app
  /// during setup. Guards the `default-features = false` on
  /// tracing-subscriber in Cargo.toml: re-enabling `tracing-log` makes
  /// `fmt().init()` install LogTracer and fails this test.
  #[test]
  fn tracing_init_leaves_log_logger_free() {
    tracing_subscriber::fmt()
      .with_env_filter(
        tracing_subscriber::EnvFilter::try_from_default_env()
          .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
      )
      .init();
    assert!(log::set_boxed_logger(Box::new(NoopLogger)).is_ok());
  }
}
