//! Conga desktop backend.
//!
//! The session-management API lives here as Tauri commands so the desktop
//! app is self-contained: it reads/writes the on-disk session store
//! (`~/.conga/sessions`) directly through conga/conga-host instead of
//! depending on a separately running gateway process. The `chat` module
//! goes one step further and hosts the agent loop itself: per-session Hosts
//! stream turn events over Tauri IPC (`chat-event`), replacing the gateway's
//! WebSocket transport inside the desktop shell. The gateway remains the
//! transport for plain-browser (dev) usage.
//!
//! Session commands are thin wrappers over `conga_host::session_api` — the
//! SAME implementations the gateway's REST handlers call. One validation
//! rule, one DTO shape, one fail-loud policy.

use conga_host::session_api::{self, SessionListItem};

mod chat;

fn session_store_root() -> std::path::PathBuf {
    conga::JsonlStorage::default_root().base_dir_clone()
}

#[tauri::command]
async fn list_sessions() -> Result<Vec<SessionListItem>, String> {
    session_api::list_sessions(&session_store_root())
        .await
        .map_err(|e| e.to_string())
}

/// Backend-truth transcript for a session. `Ok(None)` means the session has
/// no on-disk data yet (a local-only chat) — the frontend keeps its local
/// state in that case. Corruption fails loud with `Err`.
#[tauri::command]
async fn get_session_messages(id: String) -> Result<Option<Vec<serde_json::Value>>, String> {
    session_api::session_messages(&session_store_root(), &id)
        .await
        .map_err(|e| e.to_string())
        .and_then(|messages| {
            messages
                .map(|m| {
                    serde_json::to_value(m)
                        .map(|v| v.as_array().cloned().unwrap_or_default())
                        .map_err(|e| e.to_string())
                })
                .transpose()
        })
}

/// Persist the session's display name (meta.json sidecar). Creates the
/// session directory if needed, so a chat can be named before its first
/// turn lands on disk. Validation is shared with the gateway.
#[tauri::command]
async fn rename_session(id: String, name: String) -> Result<(), String> {
    session_api::rename_session(&session_store_root(), &id, &name)
        .await
        .map_err(|e| e.to_string())
}

/// Cross-session full-text search (FTS5 sidecar at `~/.conga/index.db`).
/// Same engine as the gateway's REST route (conga_host::session_api):
/// incremental high-water reindex check + query, on a blocking thread.
#[tauri::command]
async fn search_sessions(
    query: String,
) -> Result<Vec<conga_host::session_index::SessionHit>, String> {
    let root = session_store_root();
    let db = conga::storage::config_dir().join("index.db");
    tokio::task::spawn_blocking(move || {
        session_api::search_sessions(&root, &db, &query, 20).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("engine task join failed: {e}"))?
}

/// Delete the session's on-disk data wholesale (event log + meta sidecar).
#[tauri::command]
async fn delete_session(id: String) -> Result<bool, String> {
    session_api::delete_session(&session_store_root(), &id)
        .await
        .map_err(|e| e.to_string())
}

/// `~/.conga/app_config.json` — the desktop shell's durable mirror of the
/// browser build's localStorage preferences (theme, sidebar state, chats
/// meta, hidden sessions). One JSON object keyed by storage key; values are
/// parsed JSON when possible, else the raw string — the frontend round-trips
/// them back into localStorage byte-for-byte. Same fail-loud conventions as
/// the session store: corruption is an error, never silently re-created.
fn app_config_path() -> std::path::PathBuf {
    conga::storage::config_dir().join("app_config.json")
}

/// Extract `conga_proxy` from the app config and install it as the
/// fetch/web_search proxy override. Missing or empty clears the override
/// (direct connection). Values may be raw strings (writeString path — not
/// JSON) or JSON strings; `as_str` covers both.
fn apply_proxy_from_config(config: &serde_json::Value) -> Result<(), String> {
    let url = config
        .get("conga_proxy")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    conga_host::set_tool_proxy(url).map_err(|e| format!("conga_proxy invalid: {e}"))
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
    conga_host::validate_tool_proxy(url)
}

/// The masked LLM env settings (raw keys never leave the process). See
#[tauri::command]
fn get_env_settings() -> serde_json::Value {
    conga_host::settings::settings_to_masked_json(&conga_host::settings::load_settings())
}

/// Validate → merge (blank `apiKey` keeps the stored one) → persist
/// atomically. The in-process Host re-resolves its provider from this file
/// every turn, so the next LLM call uses the new settings.
#[tauri::command]
fn set_env_settings(payload: serde_json::Value) -> Result<serde_json::Value, String> {
    conga_host::settings::put_settings(&payload)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let _ = dotenvy::dotenv();
    // conga/conga-host emit through `tracing`; without a global
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
            get_env_settings,
            set_env_settings,
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
